use std::fmt::Debug;
use std::future::Future;
use std::marker::PhantomData;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use base64::Engine;

use serde::Serialize;
use serde::de::DeserializeOwned;

use fastwebsockets::{FragmentCollectorRead, Frame, OpCode, Payload, TokioIo, WebSocket, WebSocketError, WebSocketRead, WebSocketWrite};
use fastwebsockets::body::Empty;

use thiserror::Error;

use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender, UnboundedReceiver};
use tokio::sync::mpsc::error::TryRecvError;
use tokio::net::TcpListener;

use hyper::{HeaderMap, Request, Response};
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;

pub use tokio;
pub use hyper;
pub use bincode;

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
    out_r: Re<'a>,
    out_s: Se<'a>,
}

#[derive(Error, Debug)]
pub enum SendError {
    #[error("serialization failed")]
    Serialize(#[from] bincode::Error),
    #[error("channel closed")]
    ChannelClosed,
}

#[derive(Error, Debug)]
pub enum RecvError {
    #[error("deserialization failed")]
    Deserialize(#[from] bincode::Error),
    #[error("channel closed")]
    ChannelClosed,
    #[error("websocket error")]
    WebSocket(#[from] WebSocketError),
}

pub struct PacketSender<'a, P> {
    channel: Se<'a>,
    _ty: PhantomData<P>
}

impl<'a, P> Clone for PacketSender<'a, P> {
    fn clone(&self) -> Self {
        Self {
            channel: self.channel.clone(),
            _ty: PhantomData
        }
    }
}

impl<'a, P> PacketSender<'a, P> where P: Serialize + Debug + Send {
    pub fn ping(&self) -> Result<(), SendError> {
        self.channel.send(Ok(Frame::new(true, OpCode::Ping, None, Payload::Borrowed(&[])))).map_err(|_| SendError::ChannelClosed)
    }

    pub fn send(&self, msg: &P) -> Result<(), SendError> {
        let serialized = bincode::serialize(msg)?;
        let payload = Payload::Owned(serialized);
        self.channel.send(Ok(Frame::binary(payload))).map_err(|_| SendError::ChannelClosed)
    }

    pub fn close(&self, code: u16, msg: &P) -> Result<(), SendError> {
        let serialized = bincode::serialize(msg)?;
        self.channel.send(Ok(Frame::close(code, &serialized))).map_err(|_| SendError::ChannelClosed)
    }
}

pub struct PacketReceiver<'a, P> {
    channel: Re<'a>,
    closed: bool,
    _ty: PhantomData<P>
}

impl<'a, P> PacketReceiver<'a, P> where P: DeserializeOwned + Debug + Send {
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    pub async fn recv(&mut self) -> Result<Option<P>, RecvError> {
        if self.closed {
            return Err(RecvError::ChannelClosed);
        }
        if let Some(result) = self.channel.recv().await {
            self.process(result)
        } else {
            Ok(None)
        }
    }

    pub fn try_recv(&mut self) -> Result<Option<P>, RecvError> {
        if self.closed {
            return Err(RecvError::ChannelClosed);
        }
        match self.channel.try_recv() {
            Ok(command) => {
                self.process(command)
            }
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(RecvError::ChannelClosed)
        }
    }

    fn process(&mut self, result: Result<Frame<'a>, WebSocketError>) -> Result<Option<P>, RecvError> {
        match result {
            Ok(frame) => {
                Ok(match frame.opcode {
                    OpCode::Text | OpCode::Binary | OpCode::Close => {
                        let message = bincode::deserialize::<P>(&*frame.payload)?;
                        if frame.opcode == OpCode::Close {
                            self.closed = true
                        }
                        Some(message)
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
            if let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(s) {
                if raw.len() == 16 {
                    let mut token = [0; 16];
                    token.copy_from_slice(&raw);
                    return Ok(u128::from_le_bytes(token));
                }
            }
        }
    }
    Err(WebSocketError::MissingSecWebSocketKey)
}

pub struct Peer<'a, P> {
    pub ip: IpAddr,
    pub token: u128,
    pub rx: PacketReceiver<'a, P>,
    pub tx: PacketSender<'a, P>
}

pub enum Connection {
    Established,
    Reconnecting
}

async fn upgrade<'a, P>(
    mut req: Request<Incoming>,
    peer: SocketAddr,
    callback: UnboundedSender<Peer<'static, P>>
) -> Result<Response<Empty<Bytes>>, WebSocketError>
    where
        P: Send + 'static
{
    let headers = req.headers();
    let ip = get_peer_ip(headers, peer.ip());
    let token = get_peer_token(headers)?;
    let (response, fut) = fastwebsockets::upgrade::upgrade(&mut req)?;

    tokio::spawn(async move {
        let ws = fut.await.expect("fail");
        let (rx, tx, worker) = split::<_, P>(ws);
        callback.send(Peer {
            ip, token, rx, tx
        }).unwrap();
        worker.await;
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

        let r = fut.await;
        if let Err(e) = r {
            eprintln!("An error occurred: {:?}", e);
        }
    }

}

pub async fn connect<'a, P>(url: &'a str, max_tries: u32, reconnect_in: Duration, callback: UnboundedSender<Connection>) -> Result<(PacketReceiver<P>, PacketSender<P>, impl Future<Output=WebSocketError> + 'a), WebSocketError> {
    let request = ConnectionRequest::new(url);
    let conn = fastwebsockets::handshake::connect(&request).await;
    conn.map(|ws| {
        let (rx, tx) = ws.split(|s| tokio::io::split(s));
        let (receiver, sender, worker) = create_channels::<'a, _, P>(rx, tx);

        let worker = async move {
            let _ = callback.send(Connection::Established);
            let mut reconnect = worker.await;
            let mut reconnect_timeout = TimedInterval::from(tokio::time::interval(reconnect_in), max_tries);

            loop {
                let _ = callback.send(Connection::Reconnecting);
                match fastwebsockets::handshake::connect(&request).await {
                    Err(e) => {
                        //Timeout
                        if reconnect_timeout.check_expired().await {
                            break e;
                        }
                    }
                    Ok(ws) => {
                        let (rx, tx) = ws.split(|s| tokio::io::split(s));
                        reconnect_timeout.reset();
                        let worker = create_workers(rx, tx, reconnect);
                        println!("Reconnect successful");
                        let _ = callback.send(Connection::Established);
                        reconnect = worker.await;
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

async fn outbound<'a, T>(mut tx: Tx<T>, mut out_r: Re<'a>) -> Re<'a>
    where T: AsyncReadExt + AsyncWriteExt + Unpin + Send + 'a
{
    loop {
        match out_r.recv().await {
            Some(command) => {
                match command {
                    Ok(frame) => {
                        if let Err(e) = tx.write_frame(frame).await {
                            eprintln!("Send error: {:?}", e);
                            break out_r;
                        }
                    }
                    Err(_) => {
                        break out_r;
                    }
                }
            }
            None => {
                eprintln!("Packet outbound channel closed");
                break out_r;
            }
        }
    }
}

async fn inbound<'a, T>(rx: Rx<T>, in_s: Se<'a>, out_s: Se<'a>) -> (Se<'a>, Se<'a>)
    where T: AsyncReadExt + AsyncWriteExt + Unpin + Send + 'a
{
    let mut rx = FragmentCollectorRead::new(rx);

    let mut obligated_send = |f| async {
        if let Err(_) = out_s.send(Ok(f)) {
            Err(SendError::ChannelClosed)
        } else {
            Ok(())
        }
    };

    loop {
        match rx.read_frame(&mut obligated_send).await {
            Ok(frame) => {
                if let Err(_) = in_s.send(Ok(frame)) {
                    eprintln!("Packet inbound channel closed");
                    break (in_s, out_s);
                }
            }
            Err(e) => {
                eprintln!("Recv error: {:?}", e);
                out_s.send(Err(e)).unwrap();
                break (in_s, out_s);
            }
        }
    }
}

pub fn split<'a, T, P>(ws: WebSocket<T>) -> (PacketReceiver<'a, P>, PacketSender<'a, P>, impl Future<Output=Reconnect<'a>> + 'a)
    where
        T: AsyncReadExt + AsyncWriteExt + Unpin + Send + 'a
{
    let (rx, tx) = ws.split(|s| tokio::io::split(s));
    create_channels(rx, tx)
}

pub fn create_channels<'a, T, P>(rx: Rx<T>, tx: Tx<T>) -> (PacketReceiver<'a, P>, PacketSender<'a, P>, impl Future<Output=Reconnect<'a>> + 'a)
    where
        T: AsyncReadExt + AsyncWriteExt + Unpin + Send + 'a
{
    let (out_s, out_r) = unbounded_channel::<Result<Frame<'a>, WebSocketError>>();
    let (in_s, in_r) = unbounded_channel::<Result<Frame<'a>, WebSocketError>>();

    let recv = PacketReceiver {
        channel: in_r,
        closed: false,
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