use crate::body::ContentEncoding;
use crate::conn_http1::send_request_http1;
use crate::conn_http2::send_request_http2;
use crate::h1::SendRequest as H1SendRequest;
use crate::Body;
use crate::Error;
use bytes::Bytes;
use h2::client::SendRequest as H2SendRequest;
use std::fmt;

pub enum ProtocolImpl {
    Http1(H1SendRequest),
    Http2(H2SendRequest<Bytes>),
}

impl fmt::Display for ProtocolImpl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtocolImpl::Http1(_) => write!(f, "Http1"),
            ProtocolImpl::Http2(_) => write!(f, "Http2"),
        }
    }
}

pub struct Connection {
    p: ProtocolImpl,
}

impl Connection {
    pub(crate) fn new(p: ProtocolImpl) -> Self {
        Connection { p }
    }

    pub fn maybe_clone(&self) -> Option<Connection> {
        if let ProtocolImpl::Http2(send_req) = &self.p {
            return Some(Connection::new(ProtocolImpl::Http2(send_req.clone())));
        }
        None
    }

    pub async fn send_request(
        self,
        req: http::Request<Body>,
    ) -> Result<http::Response<Body>, Error> {
        // resolve deferred body codec now that we know the headers.
        let (parts, mut body) = req.into_parts();
        let content_encoding = ContentEncoding::from_headers(&parts.headers, false);
        body.resolve_deferred(content_encoding);
        let req = http::Request::from_parts(parts, body);

        match self.p {
            ProtocolImpl::Http2(send_req) => send_request_http2(send_req, req).await,
            ProtocolImpl::Http1(send_req) => send_request_http1(send_req, req).await,
        }
    }
}
