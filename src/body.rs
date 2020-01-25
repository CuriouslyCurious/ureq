use crate::char_enc::CharCodec;
use crate::conn::ProtocolImpl;
use crate::h1::RecvStream as H1RecvStream;
use crate::h1::SendRequest as H1SendRequest;
use crate::AsyncRead;
use crate::Connection;
use crate::Error;
use bytes::Bytes;
use futures_util::future::poll_fn;
use futures_util::io::BufReader;
use futures_util::ready;
use h2::client::SendRequest as H2SendRequest;
use h2::RecvStream as H2RecvStream;
use std::io;
use std::mem;
use std::pin::Pin;
use std::task::{Context, Poll};

#[cfg(feature = "gzip")]
use async_compression::futures::bufread::{GzipDecoder, GzipEncoder};

const BUF_SIZE: usize = 16_384;

pub struct Body {
    codec: BufReader<BodyCodec>,
    has_read: bool,
    char_codec: Option<CharCodec>,
}

impl Body {
    pub fn empty() -> Self {
        Self::new(BodyImpl::RequestEmpty)
    }
    pub fn from_async_read<R: AsyncRead + Unpin + Send + 'static>(reader: R) -> Self {
        Self::new(BodyImpl::RequestAsyncRead(Box::new(reader)))
    }
    pub fn from_sync_read<R: io::Read + Send + 'static>(reader: R) -> Self {
        Self::new(BodyImpl::RequestRead(Box::new(reader)))
    }
    pub(crate) fn new(bimpl: BodyImpl) -> Self {
        let reader = BodyReader::new(bimpl);
        let codec = BufReader::new(BodyCodec::deferred(reader));
        Body {
            codec,
            has_read: false,
            char_codec: None,
        }
    }

    pub(crate) fn setup_codecs(&mut self, headers: &http::header::HeaderMap, is_decode: bool) {
        if self.has_read {
            panic!("setup_codecs after body started reading");
        }

        let mut new_codec = None;
        if let BodyCodec::Deferred(reader) = self.codec.get_mut() {
            if let Some(reader) = reader.take() {
                let encoding = content_encoding_from_headers(headers);
                new_codec = Some(BodyCodec::from_encoding(reader, encoding, is_decode))
            }
        }

        if let Some(new_codec) = new_codec {
            // to avoid creating another BufReader
            mem::replace(self.codec.get_mut(), new_codec);
        }

        if let Some(charset) = charset_from_headers(headers) {
            self.char_codec = Some(CharCodec::new(charset, is_decode));
        }
    }

    pub async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
        Ok(poll_fn(|cx| Pin::new(&mut *self).poll_read(cx, buf)).await?)
    }

    pub async fn into_connection(self) -> Result<Connection, Error> {
        self.codec.into_inner().into_inner().into_connection().await
    }
}

fn content_encoding_from_headers(headers: &http::header::HeaderMap) -> Option<&str> {
    headers
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
}

fn charset_from_headers(headers: &http::header::HeaderMap) -> Option<&str> {
    headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .and_then(|x| {
            // text/html; charset=utf-8
            let s = x.split(';');
            s.last().map(|l| l.trim())
        })
        .and_then(|x| {
            // charset=utf-8
            let mut s = x.split('=');
            s.nth(1)
        })
}

#[allow(clippy::large_enum_variant)]
enum BodyCodec {
    Deferred(Option<BodyReader>),
    Plain(BodyReader),
    #[cfg(feature = "gzip")]
    GzipDecoder(GzipDecoder<BufReader<BodyReader>>),
    #[cfg(feature = "gzip")]
    GzipEncoder(GzipEncoder<BufReader<BodyReader>>),
}

impl BodyCodec {
    fn deferred(reader: BodyReader) -> Self {
        BodyCodec::Deferred(Some(reader))
    }
    fn from_encoding(reader: BodyReader, encoding: Option<&str>, is_decode: bool) -> Self {
        trace!("Body codec: {:?}", encoding);
        match (encoding, is_decode) {
            (None, _) => BodyCodec::Plain(reader),
            (Some("gzip"), true) => {
                let buf = BufReader::new(reader);
                BodyCodec::GzipDecoder(GzipDecoder::new(buf))
            }
            (Some("gzip"), false) => {
                let buf = BufReader::new(reader);
                let comp = flate2::Compression::fast();
                BodyCodec::GzipEncoder(GzipEncoder::new(buf, comp))
            }
            _ => {
                warn!("Unknown content-encoding: {:?}", encoding);
                BodyCodec::Plain(reader)
            }
        }
    }

    fn into_inner(self) -> BodyReader {
        match self {
            BodyCodec::Deferred(_) => panic!("into_inner() on BodyCodec::Deferred"),
            BodyCodec::Plain(r) => r,
            #[cfg(feature = "gzip")]
            BodyCodec::GzipDecoder(r) => r.into_inner().into_inner(),
            #[cfg(feature = "gzip")]
            BodyCodec::GzipEncoder(r) => r.into_inner().into_inner(),
        }
    }
}

pub struct BodyReader {
    imp: BodyImpl,
    leftover_bytes: Option<Bytes>,
    is_finished: bool,
}

pub enum BodyImpl {
    RequestEmpty,
    RequestAsyncRead(Box<dyn AsyncRead + Unpin + Send>),
    RequestRead(Box<dyn io::Read + Send>),
    Http1(H1RecvStream, H1SendRequest),
    Http2(H2RecvStream, H2SendRequest<Bytes>),
}

impl BodyReader {
    fn new(imp: BodyImpl) -> Self {
        BodyReader {
            imp,
            leftover_bytes: None,
            is_finished: false,
        }
    }

    async fn into_connection(mut self) -> Result<Connection, Error> {
        // http11 reuses the same connection, and we can't leave the body
        // half way through read.
        if self.is_http11() && !self.is_finished {
            self.read_to_end().await?;
        }

        let conn = match self.imp {
            BodyImpl::Http1(_, h1) => Connection::new(ProtocolImpl::Http1(h1)),
            BodyImpl::Http2(_, h2) => Connection::new(ProtocolImpl::Http2(h2)),
            _ => return Err(Error::Static("Can't do into_connection() on request body")),
        };

        Ok(conn)
    }

    fn is_http11(&self) -> bool {
        match &self.imp {
            BodyImpl::Http1(_, _) => true,
            _ => false,
        }
    }

    async fn read_to_end(&mut self) -> Result<(), Error> {
        let mut buf = vec![0_u8; BUF_SIZE];
        loop {
            let read = self.read(&mut buf).await?;
            if read == 0 {
                break;
            }
        }
        Ok(())
    }

    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
        Ok(poll_fn(|cx| Pin::new(&mut *self).poll_read(cx, buf)).await?)
    }

    // helper to shuffle Bytes into a &[u8] and handle the remains.
    fn bytes_to_buf(&mut self, mut data: Bytes, buf: &mut [u8]) -> usize {
        let max = data.len().min(buf.len());
        (&mut buf[0..max]).copy_from_slice(&data[0..max]);
        let remain = if max < data.len() {
            Some(data.split_off(max))
        } else {
            None
        };
        self.leftover_bytes = remain;
        max
    }
}

impl AsyncRead for BodyReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.is_finished {
            return Ok(0).into();
        }
        // h2 streams might have leftovers to use up before reading any more.
        if let Some(data) = this.leftover_bytes.take() {
            let amount = this.bytes_to_buf(data, buf);
            return Ok(amount).into();
        }
        let read = match &mut this.imp {
            BodyImpl::RequestEmpty => 0,
            BodyImpl::RequestAsyncRead(reader) => ready!(Pin::new(reader).poll_read(cx, buf))?,
            BodyImpl::RequestRead(reader) => reader.read(buf)?,
            BodyImpl::Http1(recv, _) => ready!(recv.poll_read(cx, buf))?,
            BodyImpl::Http2(recv, _) => {
                if let Some(data) = ready!(recv.poll_data(cx)) {
                    let data = data.map_err(|e| {
                        e.into_io().unwrap_or_else(|| {
                            io::Error::new(io::ErrorKind::Other, "Other h2 error")
                        })
                    })?;
                    this.bytes_to_buf(data, buf)
                } else {
                    0
                }
            }
        };
        if read == 0 {
            this.is_finished = true;
        }
        Ok(read).into()
    }
}

impl From<()> for Body {
    fn from(_: ()) -> Self {
        Body::empty()
    }
}

impl<'a> From<&'a str> for Body {
    fn from(s: &'a str) -> Self {
        s.to_owned().into()
    }
}

impl<'a> From<&'a String> for Body {
    fn from(s: &'a String) -> Self {
        s.clone().into()
    }
}

impl From<String> for Body {
    fn from(s: String) -> Self {
        let bytes = s.into_bytes();
        bytes.into()
    }
}

impl<'a> From<&'a [u8]> for Body {
    fn from(bytes: &'a [u8]) -> Self {
        bytes.to_vec().into()
    }
}

impl From<Vec<u8>> for Body {
    fn from(bytes: Vec<u8>) -> Self {
        let cursor = io::Cursor::new(bytes);
        Body::from_sync_read(cursor)
    }
}

impl AsyncRead for BodyCodec {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        match this {
            BodyCodec::Deferred(_) => panic!("poll_read on BodyCodec::Deferred"),
            BodyCodec::Plain(r) => Pin::new(r).poll_read(cx, buf),
            #[cfg(feature = "gzip")]
            BodyCodec::GzipDecoder(r) => Pin::new(r).poll_read(cx, buf),
            #[cfg(feature = "gzip")]
            BodyCodec::GzipEncoder(r) => Pin::new(r).poll_read(cx, buf),
        }
    }
}

impl AsyncRead for Body {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        this.has_read = true;
        if let Some(char_codec) = &mut this.char_codec {
            char_codec.poll_decode(cx, &mut this.codec, buf)
        } else {
            Pin::new(&mut this.codec).poll_read(cx, buf)
        }
    }
}
