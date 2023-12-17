use std::fmt::Debug;
use std::future::Future;
use std::marker::PhantomData;
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;

use fastwebsockets::{FragmentCollectorRead, Frame, OpCode, Payload, WebSocket, WebSocketError, WebSocketRead, WebSocketWrite};

use thiserror::Error;

use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender, UnboundedReceiver};
use tokio::sync::mpsc::error::TryRecvError;
use tokio::time::Interval;

pub use tokio;
pub use hyper;
pub use bincode;

pub mod http;
pub mod macros;

use http::ConnectionRequest;

enum FlowCommand<'a> {
    Frame(Frame<'a>),
    Drop,
}

type Re<'a> = UnboundedReceiver<FlowCommand<'a>>;
type Se<'a> = UnboundedSender<FlowCommand<'a>>;

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
}

#[derive(Clone)]
pub struct PacketSender<'a, P> {
    channel: Se<'a>,
    _ty: PhantomData<P>
}

impl<'a, P> PacketSender<'a, P> where P: Serialize + Debug + Send {
    pub fn ping(&self) -> Result<(), SendError> {
        self.channel.send(FlowCommand::Frame(Frame::new(true, OpCode::Ping, None, Payload::Borrowed(&[])))).map_err(|_| SendError::ChannelClosed)
    }

    pub fn send(&self, msg: &P) -> Result<(), SendError> {
        let serialized = bincode::serialize(msg)?;
        let payload = Payload::Owned(serialized);
        self.channel.send(FlowCommand::Frame(Frame::binary(payload))).map_err(|_| SendError::ChannelClosed)
    }

    pub fn close(&self, code: u16, msg: &P) -> Result<(), SendError> {
        let serialized = bincode::serialize(msg)?;
        self.channel.send(FlowCommand::Frame(Frame::close(code, &serialized))).map_err(|_| SendError::ChannelClosed)
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
        if let Some(command) = self.channel.recv().await {
            self.process_command(command)
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
                self.process_command(command)
            }
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(RecvError::ChannelClosed)
        }
    }

    fn process_command(&mut self, command: FlowCommand) -> Result<Option<P>, RecvError> {
        match command {
            FlowCommand::Frame(frame) => {
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
            FlowCommand::Drop => Err(RecvError::ChannelClosed)
        }
    }
}

struct TimedInterval {
    interval: Interval,
    times: u32,
    current_time: u32,
}

impl TimedInterval {
    fn from(interval: Interval, times: u32) -> Self {
        Self { interval, times, current_time: times }
    }

    async fn check_expired(&mut self) -> bool {
        self.interval.tick().await;
        self.current_time -= 1;
        self.current_time == 0
    }

    fn reset(&mut self) {
        self.current_time = self.times;
    }
}

pub async fn connect<'a, P>(url: &'a str, max_tries: u32, reconnect_in: Duration) -> Result<(PacketReceiver<P>, PacketSender<P>, impl Future<Output=WebSocketError> + 'a), WebSocketError> {
    let request = ConnectionRequest::new(url);
    let conn = fastwebsockets::handshake::connect(&request).await;
    conn.map(|ws| {
        let (rx, tx) = ws.split(|s| tokio::io::split(s));
        let (receiver, sender, worker) = create_channels::<'a, _, P>(rx, tx);

        let worker = async move {
            let mut reconnect = worker.await;
            let mut reconnect_timeout = TimedInterval::from(tokio::time::interval(reconnect_in), max_tries);

            loop {
                println!("Trying to reconnect...");
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
                    FlowCommand::Frame(frame) => {
                        if let Err(e) = tx.write_frame(frame).await {
                            eprintln!("Send error: {:?}", e);
                            break out_r;
                        }
                    }
                    FlowCommand::Drop => {
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
        if let Err(_) = out_s.send(FlowCommand::Frame(f)) {
            Err(SendError::ChannelClosed)
        } else {
            Ok(())
        }
    };

    loop {
        match rx.read_frame(&mut obligated_send).await {
            Ok(frame) => {
                if let Err(_) = in_s.send(FlowCommand::Frame(frame)) {
                    eprintln!("Packet inbound channel closed");
                    break (in_s, out_s);
                }
            }
            Err(e) => {
                eprintln!("Recv error: {:?}", e);
                out_s.send(FlowCommand::Drop).unwrap();
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
    let (out_s, out_r) = unbounded_channel::<FlowCommand<'_>>();
    let (in_s, in_r) = unbounded_channel::<FlowCommand<'_>>();

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