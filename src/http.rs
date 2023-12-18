use base64::Engine;
use fastwebsockets::body::Empty;
use fastwebsockets::handshake::IntoClientRequest;
use fastwebsockets::WebSocketError;
use hyper::body::Bytes;
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

impl<'a> IntoClientRequest<Empty<Bytes>> for &'a ConnectionRequest {
    fn into_client_request(self) -> Result<Request<Empty<Bytes>>, WebSocketError> {
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
            .header("Sec-WebSocket-Key", base64::engine::general_purpose::STANDARD.encode(self.token.to_le_bytes()))
            .uri(uri)
            .body(Empty::<Bytes>::new())?;

        Ok(req)
    }
}