use std::borrow::Cow;
use std::fmt::Debug;
use std::future::Future;
use std::marker::PhantomData;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use base64::Engine;

use serde::Serialize;
use serde::de::DeserializeOwned;

use fastwebsockets::{FragmentCollectorRead, Frame, OpCode, Payload, TokioIo, WebSocketError, WebSocketRead, WebSocketWrite};
use fastwebsockets::body::Empty;

use thiserror::Error;

use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender, UnboundedReceiver};
use tokio::sync::mpsc::error::TryRecvError;
use tokio::net::TcpListener;
use tokio::time::interval;

use hyper::{HeaderMap, Request, Response};
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;

pub use tokio;
pub use hyper;
pub use bincode;
pub use fastwebsockets as ws;

pub mod http;
pub mod macros;
mod interval;

use http::ConnectionRequest;
use interval::TimedInterval;

type Re<'a> = UnboundedReceiver<Result<Frame<'a>, WebSocketError>>;
type Se<'a> = UnboundedSender<Result<Frame<'a>, WebSocketError>>;

type Rx<T> = WebSocketRead<ReadHalf<T>>;
type Tx<T> = WebSocketWrite<WriteHalf<T>>;

pub struct Reconnect<'a> {
    in_s: Se<'a>,
    out_r: Re<'static>,
    out_s: Se<'static>,
}

#[derive(Error, Debug)]
pub enum SendError {
    #[error("serialization failed")]
    Serialize(#[from] bincode::Error),
    #[error("pipe lost")]
    Pipe,
}

#[derive(Error, Debug)]
pub enum RecvError {
    #[error("deserialization failed: {0}")]
    Deserialize(bincode::Error, Vec<u8>),
    #[error("pipe lost")]
    Pipe,
    #[error("channel closed: {1} ({0})")]
    ChannelClosed(u16, String),
    #[error("websocket error: {0}")]
    WebSocket(#[from] WebSocketError),
}

pub struct PacketSender<P> {
    channel: Se<'static>,
    _ty: PhantomData<P>,
}

impl<P> Clone for PacketSender<P> {
    fn clone(&self) -> Self {
        Self {
            channel: self.channel.clone(),
            _ty: PhantomData,
        }
    }
}

impl<P> PacketSender<P> where P: Serialize + Debug + Send {
    pub fn ping(&self) -> Result<(), SendError> {
        self.channel.send(Ok(Frame::new(true, OpCode::Ping, None, Payload::Borrowed(&[])))).map_err(|_| SendError::Pipe)
    }

    pub fn send(&self, msg: &P) -> Result<(), SendError> {
        let serialized = bincode::serialize(msg)?;
        let payload = Payload::Owned(serialized);
        self.channel.send(Ok(Frame::binary(payload))).map_err(|_| SendError::Pipe)
    }

    pub fn close(&self, code: u16, reason: &str) -> Result<(), SendError> {
        self.channel.send(Ok(Frame::close(code, reason.as_bytes()))).map_err(|_| SendError::Pipe)
    }
}

pub struct PacketReceiver<'a, P> {
    channel: Re<'a>,
    close_code: Option<(u16, String)>,
    _ty: PhantomData<P>,
}

impl<'a, P> PacketReceiver<'a, P> where P: DeserializeOwned + Debug + Send {
    pub fn get_close_code(&self) -> &Option<(u16, String)> {
        &self.close_code
    }

    pub async fn recv(&mut self) -> Result<Option<P>, RecvError> {
        if let Some(code) = self.close_code.clone() {
            return Err(RecvError::ChannelClosed(code.0, code.1));
        }
        if let Some(result) = self.channel.recv().await {
            self.process(result)
        } else {
            Ok(None)
        }
    }

    pub fn try_recv(&mut self) -> Result<Option<P>, RecvError> {
        if let Some(code) = self.close_code.clone() {
            return Err(RecvError::ChannelClosed(code.0, code.1));
        }
        match self.channel.try_recv() {
            Ok(command) => {
                self.process(command)
            }
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(RecvError::Pipe)
        }
    }

    fn process(&mut self, result: Result<Frame<'a>, WebSocketError>) -> Result<Option<P>, RecvError> {
        match result {
            Ok(frame) => {
                let mut payload = &*frame.payload;
                Ok(match frame.opcode {
                    OpCode::Close => {
                        let code = u16::from_be_bytes([payload[0], payload[1]]);
                        payload = &payload[2..];
                        let reason = match String::from_utf8_lossy(payload) {
                            Cow::Borrowed(unchanged) => unchanged.to_owned(),
                            Cow::Owned(damaged) => damaged
                        };
                        self.close_code = Some((code, reason.clone()));
                        return Err(RecvError::ChannelClosed(code, reason));
                    }
                    OpCode::Text | OpCode::Binary => {
                        match bincode::deserialize::<P>(&*frame.payload) {
                            Ok(message) => {
                                Some(message)
                            }
                            Err(de) => {
                                return Err(RecvError::Deserialize(de, Vec::from(frame.payload)))
                            }
                        }
                    }
                    _ => None
                })
            }
            Err(e) => Err(RecvError::WebSocket(e))
        }
    }
}

fn get_peer_ip(headers: &HeaderMap, fallback: IpAddr) -> IpAddr {
    if let Some(header) = headers.get("x-real-ip") {
        if let Ok(s) = std::str::from_utf8(header.as_ref()) {
            if let Ok(ip) = s.parse::<IpAddr>() {
                return ip;
            }
        }
    }
    fallback
}

fn get_peer_token(headers: &HeaderMap) -> Result<u128, WebSocketError> {
    if let Some(header) = headers.get("sec-websocket-key") {
        if let Ok(s) = std::str::from_utf8(header.as_ref()) {
            return decode_token(s);
        }
    }
    Err(WebSocketError::MissingSecWebSocketKey)
}

const TOKEN_LENGTH: usize = 16;

fn decode_token(s: &str) -> Result<u128, WebSocketError> {
    if let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(s) {
        if raw.len() == TOKEN_LENGTH {
            let mut token = [0; TOKEN_LENGTH];
            token.copy_from_slice(&raw);
            return Ok(u128::from_le_bytes(token));
        }
    }
    Err(WebSocketError::InvalidValue)
}

pub struct Peer<'a, P> {
    pub ip: IpAddr,
    pub token: u128,
    pub rx: PacketReceiver<'a, P>,
    pub tx: PacketSender<P>,
}

pub enum Connection {
    Established(bool),
    // is primary
    Reconnecting,
    Disconnected,
    Failed(WebSocketError),
}

async fn upgrade<'a, P>(
    mut req: Request<Incoming>,
    peer: SocketAddr,
    callback: UnboundedSender<Peer<'static, P>>,
) -> Result<Response<Empty<Bytes>>, WebSocketError>
    where
        P: Send + 'static
{
    let headers = req.headers();
    let ip = get_peer_ip(headers, peer.ip());
    let token = get_peer_token(headers)?;
    let (response, upgrade) = fastwebsockets::upgrade::upgrade(&mut req)?;

    tokio::spawn(async move {
        let ws = upgrade.await.expect("fail");
        let (rx, tx) = ws.split(tokio::io::split);
        let (rx, tx, worker) = create_channels(rx, tx);
        callback.send(Peer {
            ip,
            token,
            rx,
            tx,
        }).unwrap();
        worker.await;
        eprintln!("Closing socket for {:08X}", token);
    });

    Ok(response)
}

pub async fn serve<'a, P: Send>(port: u16, callback: UnboundedSender<Peer<'static, P>>)
    where
        P: Send + 'static
{
    let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port);
    let listener = TcpListener::bind(&address).await.expect("Failed to bind to port");

    loop {
        let (stream, peer) = listener.accept().await.unwrap();
        let callback = callback.clone();

        let io = TokioIo::new(stream);
        let fut = hyper::server::conn::http1::Builder::new()
            .serve_connection(io, service_fn(move |req| upgrade(req, peer, callback.clone())))
            .with_upgrades();

        if let Err(e) = fut.await {
            eprintln!("An error occurred: {:?}", e);
        }
    }
}

pub async fn connect<'a, P, C>(request: C, max_tries: u32, reconnect_in: Duration, callback: UnboundedSender<Connection>) -> Result<(PacketReceiver<'a, P>, PacketSender<P>, impl Future<Output=WebSocketError> + 'a), WebSocketError>
    where C: Into<ConnectionRequest>
{
    let request = request.into();
    let conn = fastwebsockets::handshake::connect(&request).await;
    conn.map(|ws| {
        let (rx, tx) = ws.split(tokio::io::split);
        let (receiver, sender, worker) = create_channels::<'a, _, P>(rx, tx);

        let _ = callback.send(Connection::Established(true));

        let worker = async move {
            let mut reconnect = worker.await;
            let mut reconnect_timeout = TimedInterval::from(interval(reconnect_in), max_tries);

            reconnect_timeout.begin();

            loop {
                let _ = callback.send(Connection::Reconnecting);

                match fastwebsockets::handshake::connect(&request).await {
                    Err(e) => {
                        if reconnect_timeout.check_expired().await {
                            let _ = callback.send(Connection::Disconnected);
                            break e;
                        }
                        let _ = callback.send(Connection::Failed(e));
                    }
                    Ok(ws) => {
                        let (rx, tx) = ws.split(tokio::io::split);
                        reconnect_timeout.reset();
                        let worker = create_workers(rx, tx, reconnect);
                        let _ = callback.send(Connection::Established(false));
                        reconnect = worker.await;
                        reconnect_timeout.begin();
                    }
                }
            }
        };

        (receiver, sender, worker)
    })
}


pub async fn create_workers<'a, T>(rx: Rx<T>, tx: Tx<T>, Reconnect { in_s, out_r, out_s }: Reconnect<'a>) -> Reconnect<'a>
    where T: AsyncReadExt + AsyncWriteExt + Unpin + Send + 'a
{
    let ((in_s, out_s), out_r) = tokio::join!(
        inbound(rx, in_s, out_s),
        outbound(tx, out_r)
    );
    Reconnect { in_s, out_r, out_s }
}

async fn outbound<'a, T>(mut tx: Tx<T>, mut out_r: Re<'static>) -> Re<'static>
    where T: AsyncReadExt + AsyncWriteExt + Unpin + Send + 'a
{
    loop {
        if let Some(command) = out_r.recv().await {
            if let Ok(frame) = command {
                if let Err(e) = tx.write_frame(frame).await {
                    eprintln!("Send error: {:?}", e);
                } else {
                    continue;
                }
            }
        }
        eprintln!("outbound closed");
        break out_r;
    }
}


async fn inbound<'a, T>(rx: Rx<T>, in_s: Se<'a>, out_s: Se<'static>) -> (Se<'a>, Se<'static>)
    where T: AsyncReadExt + AsyncWriteExt + Unpin + Send + 'a
{
    let mut rx = FragmentCollectorRead::new(rx);

    let mut obligated_send = |f| async {
        if let Err(_) = out_s.send(Ok(f)) {
            Err(SendError::Pipe)
        } else {
            Ok(())
        }
    };

    loop {
        match rx.read_frame(&mut obligated_send).await {
            Ok(frame) => {
                if let Err(_) = in_s.send(Ok(frame)) {
                    eprintln!("inbound channel closed");
                    break (in_s, out_s);
                }
            }
            Err(e) => {
                eprintln!("inbound ws error: {:?}", e);
                out_s.send(Err(e)).unwrap();
                break (in_s, out_s);
            }
        }
    }
}

pub fn create_channels<'a, T, P>(rx: Rx<T>, tx: Tx<T>) -> (PacketReceiver<'a, P>, PacketSender<P>, impl Future<Output=Reconnect<'a>> + 'a)
    where
        T: AsyncReadExt + AsyncWriteExt + Unpin + Send + 'a
{
    let (out_s, out_r) = unbounded_channel::<Result<Frame<'static>, WebSocketError>>();
    let (in_s, in_r) = unbounded_channel::<Result<Frame<'a>, WebSocketError>>();

    let recv = PacketReceiver {
        channel: in_r,
        close_code: None,
        _ty: PhantomData,
    };
    let send = PacketSender {
        channel: out_s.clone(),
        _ty: PhantomData,
    };

    let reconnect = Reconnect { in_s, out_r, out_s };

    let worker = create_workers(rx, tx, reconnect);

    (recv, send, worker)
}