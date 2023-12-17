use std::convert::Infallible;
use std::pin::Pin;
use std::task::{Context, Poll};
use base64::Engine;
use fastwebsockets::handshake::IntoClientRequest;
use fastwebsockets::WebSocketError;
use hyper::body::{Body, Bytes, SizeHint};
use hyper::{Request, Uri};

pub struct ConnectionRequest {
    url: String,
    token: u128
}

impl ConnectionRequest {
    pub fn new<S>(url: S) -> Self where S: ToString {
        Self {
            url: url.to_string(),
            token: rand::random()
        }
    }
}

pub struct EmptyBody;

impl Body for EmptyBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
        Poll::Ready(None)
    }

    fn is_end_stream(&self) -> bool {
        true
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(0)
    }
}

impl<'a> IntoClientRequest<EmptyBody> for &'a ConnectionRequest {
    fn into_client_request(self) -> Result<Request<EmptyBody>, WebSocketError> {
        let uri = self.url
            .parse::<Uri>()
            .map_err(|_| WebSocketError::InvalidUri)?;
        let authority =
            uri.authority().ok_or(WebSocketError::NoHostName)?.as_str();
        let host = authority
            .find('@')
            .map(|idx| authority.split_at(idx + 1).1)
            .unwrap_or_else(|| authority);

        if host.is_empty() {
            return Err(WebSocketError::EmptyHostName);
        }

        let req = Request::builder()
            .method("GET")
            .header("Host", host)
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Key", base64::engine::general_purpose::STANDARD.encode(self.token.to_ne_bytes()))
            .uri(uri)
            .body(EmptyBody)?;

        Ok(req)
    }
}