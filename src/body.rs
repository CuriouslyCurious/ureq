use crate::char_enc::CharCodec;
use crate::conn::ProtocolImpl;
use crate::h1::RecvStream as H1RecvStream;
use crate::h1::SendRequest as H1SendRequest;
use crate::Connection;
use crate::Error;
use crate::{AsyncBufRead, AsyncRead};
use bytes::Bytes;
use futures_util::future::poll_fn;
use futures_util::ready;
use h2::client::SendRequest as H2SendRequest;
use h2::RecvStream as H2RecvStream;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

#[cfg(feature = "gzip")]
use async_compression::futures::bufread::{GzipDecoder, GzipEncoder};

#[cfg(feature = "gzip")]
use futures_util::io::BufReader;

const BUF_SIZE: usize = 16_384;

pub struct Body {
    codec: BodyCodec,
    has_read: bool,
    char_codec: Option<CharCodec>,
}

impl Body {
    pub fn empty() -> Self {
        Self::new(BodyImpl::RequestEmpty, ContentEncoding::Deferred)
    }
    pub fn from_async_read<R: AsyncRead + Unpin + Send + 'static>(reader: R) -> Self {
        Self::new(
            BodyImpl::RequestAsyncRead(Box::new(reader)),
            ContentEncoding::Deferred,
        )
    }
    pub fn from_sync_read<R: io::Read + Send + 'static>(reader: R) -> Self {
        Self::new(
            BodyImpl::RequestRead(Box::new(reader)),
            ContentEncoding::Deferred,
        )
    }
    pub(crate) fn new(bimpl: BodyImpl, codec_kind: ContentEncoding) -> Self {
        let reader = BodyReader::new(bimpl);
        let codec = BodyCodec::new(codec_kind, reader);
        Body {
            codec,
            has_read: false,
            char_codec: None,
        }
    }

    pub(crate) fn resolve_deferred(&mut self, codec_kind: ContentEncoding) {
        if let BodyCodec::Deferred(reader) = &mut self.codec {
            if let Some(reader) = reader.take() {
                let new_codec = BodyCodec::new(codec_kind, reader);
                self.codec = new_codec;
            }
        }
    }

    pub(crate) fn set_char_codec(&mut self, charset: &str, decode: bool) {
        if self.has_read {
            panic!("set_char_codec after body started reading");
        }
        self.char_codec = Some(CharCodec::new(charset, decode));
    }

    pub async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
        Ok(poll_fn(|cx| Pin::new(&mut *self).poll_read(cx, buf)).await?)
    }

    pub async fn into_connection(self) -> Result<Connection, Error> {
        self.codec.into_inner().into_connection().await
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentEncoding {
    Deferred,
    Plain,
    #[cfg(feature = "gzip")]
    GzipDecode,
    #[cfg(feature = "gzip")]
    GzipEncode,
}

impl ContentEncoding {
    pub fn from_headers(headers: &http::header::HeaderMap, is_decode: bool) -> ContentEncoding {
        let cenc = headers
            .get("content-encoding")
            .and_then(|v| v.to_str().ok());
        match (cenc, is_decode) {
            (None, _) => ContentEncoding::Plain,
            #[cfg(feature = "gzip")]
            (Some("gzip"), true) => ContentEncoding::GzipDecode,
            #[cfg(feature = "gzip")]
            (Some("gzip"), false) => ContentEncoding::GzipEncode,
            (Some(v), _) => {
                error!("Unsupported content-encoding: {}", v);
                ContentEncoding::Plain
            }
        }
    }
}

pub fn charset_from_headers(headers: &http::header::HeaderMap) -> Option<&str> {
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
    GzipDecoder(BufReader<GzipDecoder<BodyReader>>),
    #[cfg(feature = "gzip")]
    GzipEncoder(BufReader<GzipEncoder<BodyReader>>),
}

impl BodyCodec {
    fn new(kind: ContentEncoding, reader: BodyReader) -> Self {
        trace!("Body codec: {:?}", kind);
        match kind {
            ContentEncoding::Deferred => BodyCodec::Deferred(Some(reader)),
            ContentEncoding::Plain => BodyCodec::Plain(reader),
            #[cfg(feature = "gzip")]
            ContentEncoding::GzipDecode => {
                BodyCodec::GzipDecoder(BufReader::new(GzipDecoder::new(reader)))
            }
            #[cfg(feature = "gzip")]
            ContentEncoding::GzipEncode => BodyCodec::GzipEncoder(BufReader::new(
                GzipEncoder::new(reader, flate2::Compression::fast()),
            )),
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
    read_buf: Vec<u8>,
    read_buf_end: usize,
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
            read_buf: vec![0; BUF_SIZE],
            read_buf_end: 0,
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

    fn do_poll_fill(&mut self, cx: &mut Context) -> Poll<io::Result<usize>> {
        if self.read_buf_end > 0 {
            return Ok(self.read_buf_end).into();
        }
        if self.is_finished {
            return Ok(0).into();
        }
        self.read_buf.resize(BUF_SIZE, 0);
        let buf = &mut self.read_buf[self.read_buf_end..];
        // this buf is not touched anywhere in poll_read_to_buf(), this *should* be ok.
        // TODO: find a way around this unsafe.
        let buf = unsafe { std::mem::transmute::<&'_ mut [u8], &'static mut [u8]>(buf) };
        let amount = ready!(self.poll_read_to_buf(cx, buf))?;
        self.read_buf_end += amount;
        trace!("Body read_buf filled to: {}", self.read_buf_end);
        Ok(self.read_buf_end).into()
    }

    fn poll_read_to_buf(&mut self, cx: &mut Context, buf: &mut [u8]) -> Poll<io::Result<usize>> {
        if self.is_finished {
            return Ok(0).into();
        }
        // h2 streams might have leftovers to use up before reading any more.
        if let Some(data) = self.leftover_bytes.take() {
            let amount = self.bytes_to_buf(data, buf);
            return Ok(amount).into();
        }
        let read = match &mut self.imp {
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
                    self.bytes_to_buf(data, buf)
                } else {
                    0
                }
            }
        };
        if read == 0 {
            self.is_finished = true;
        }
        Ok(read).into()
    }
}

impl AsyncRead for BodyReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.read_buf_end == 0 {
            if this.is_finished {
                return Ok(0).into();
            } else {
                let amount = ready!(this.do_poll_fill(cx))?;
                if amount == 0 {
                    return Ok(0).into();
                }
            }
        }
        let max = this.read_buf_end.min(buf.len());
        (&mut buf[0..max]).copy_from_slice(&this.read_buf[0..max]);
        this.read_buf_end -= max;
        this.read_buf = this.read_buf.split_off(max);
        Ok(max).into()
    }
}

impl AsyncBufRead for BodyReader {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        let this = self.get_mut();
        let available = ready!(this.do_poll_fill(cx))?;
        Ok(&this.read_buf[0..available]).into()
    }
    fn consume(self: Pin<&mut Self>, amount: usize) {
        let this = self.get_mut();
        this.read_buf_end -= amount;
        this.read_buf = this.read_buf.split_off(amount);
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

impl AsyncBufRead for BodyCodec {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        let this = self.get_mut();
        match this {
            BodyCodec::Deferred(_) => panic!("poll_fill_buf on BodyCodec::Deferred"),
            BodyCodec::Plain(r) => Pin::new(r).poll_fill_buf(cx),
            #[cfg(feature = "gzip")]
            BodyCodec::GzipDecoder(r) => Pin::new(r).poll_fill_buf(cx),
            #[cfg(feature = "gzip")]
            BodyCodec::GzipEncoder(r) => Pin::new(r).poll_fill_buf(cx),
        }
    }
    fn consume(self: Pin<&mut Self>, amount: usize) {
        let this = self.get_mut();
        match this {
            BodyCodec::Deferred(_) => panic!("consume on BodyCodec::Deferred"),
            BodyCodec::Plain(r) => Pin::new(r).consume(amount),
            #[cfg(feature = "gzip")]
            BodyCodec::GzipDecoder(r) => Pin::new(r).consume(amount),
            #[cfg(feature = "gzip")]
            BodyCodec::GzipEncoder(r) => Pin::new(r).consume(amount),
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
