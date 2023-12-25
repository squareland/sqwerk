use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use fastwebsockets::{OpCode, TokioIo};
use hyper::ext::HashMap;
use hyper::service::service_fn;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::mpsc::{Receiver, UnboundedSender};
use crate::chan::Sender;
use crate::{create_channels, PacketReceiver, RecvError, upgrade};

const BUFFER_SIZE: usize = 10;
static PACKET_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Serialize, Deserialize)]
pub enum Packet<Local: RPC, Remote: RPC> {
    Request {
        id: u64,
        data: Local::Message
    },
    Response {
        id: u64,
        data: Remote::Response
    }
}

impl<Local: RPC, Remote: RPC> Packet<Local, Remote> {
    pub fn id(&self) -> u64 {
        match self {
            Packet::Request { id, .. } => *id,
            Packet::Response { id, .. } => *id
        }
    }

    pub fn respond(&self, response: Local::Response) -> Packet<Remote, Local> {
        let id = self.id();
        Packet {
            id,
            data: response
        }
    }
}

pub struct RpcPeer<S: RPC, C: RPC> {
    pub ip: IpAddr,
    pub token: u128,
    pub rx: Receiver<(S::Message, tokio::sync::mpsc::Sender<S::Response>)>,
    pub rpc: C,
}

pub trait RPC {
    type Message;
    type Response;

    fn new(chan: Sender<Self::Message, Self::Response>) -> Self;
}

#[macro_export]
macro_rules! rpc {
    ($vis: vis trait $name: ident {
        $(fn $command:ident ($($arg: ident: $ty: ty),*) -> $ret: ty;)*
    }) => {
        $crate::macros::paste! {
            $vis mod [<$name:snake>] {
                use $crate::chan::error::Error;
                use $crate::chan::Sender;
                use $crate::tokio::task::{JoinError, JoinHandle};
                use $crate::tokio::sync::mpsc::Receiver;

                #[derive(Debug, serde::Serialize, serde::Deserialize)]
                $vis enum Message {
                    $([<$command:camel>] { $($arg: $ty),* }),*
                }

                #[derive(Debug, serde::Serialize, serde::Deserialize)]
                $vis enum Response {
                    $([<$command:camel>] ($ret)),*
                }

                $vis trait Handler {
                    $(fn $command (&mut self, $($arg: $ty),*) -> JoinHandle<$ret>;)*
                }

                impl Message {
                    $vis async fn handle_by(self, mut handler: impl Handler, mut chan: tokio::sync::mpsc::Sender<Response>) {
                        match self {
                            $(Message::[<$command:camel>] { $($arg),* } => {
                                let response = handler.$command($($arg),*).await.expect("command cancelled");
                                chan.send(Response::[<$command:camel>](response)).await.expect("outbound channel closed");
                            })*
                        }
                    }
                }

                #[derive(Clone)]
                pub struct $name {
                    chan: Sender<Message, Response>
                }

                impl $name {
                    $($vis async fn $command (&self, $($arg: $ty),*) -> Result<JoinHandle<$ret>, Error<Message>> {
                        let message = Message::[<$command:camel>] { $($arg),* };
                        let mut chan = self.chan.clone();
                        let _ = chan.send_await(message).await?;
                        Ok($crate::tokio::spawn(async move {
                            let response = chan.recv().await.expect("inbound channel closed");
                            match response {
                                Response::[<$command:camel>](response) => response,
                                other => panic!(concat!("invalid response to `", stringify!($name), "::", stringify!($command), "` call: {:?}"), other)
                            }
                        }))
                    })*
                }

                impl $crate::rpc::RPC for $name {
                    type Message = Message;
                    type Response = Response;

                    fn new(chan: Sender<Message, Response>) -> Self {
                        Self { chan }
                    }
                }
            }
        }
    };
}

rpc! {
    pub trait Client {
        fn notify(text: String) -> ();
    }
}

rpc! {
    pub trait Server {
        fn connect() -> ();
        fn login(login: String, password: String) -> String;
    }
}
pub async fn serve<'a, S, C>(port: u16, callback: UnboundedSender<RpcPeer<S, C>>)
    where
        S: RPC, C: RPC,
        S::Message: Send + 'static,
        S::Response: Send + 'static,
        C::Message: Send + 'static,
        C::Response: Send + 'static,
{
    let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port);
    let listener = TcpListener::bind(&address).await.expect("Failed to bind to port");

    loop {
        let (stream, peer) = listener.accept().await.unwrap();
        let callback = callback.clone();

        let io = TokioIo::new(stream);
        let fut = hyper::server::conn::http1::Builder::new()
            .serve_connection(io, service_fn(move |req| {
                let callback = callback.clone();
                upgrade(req, peer, move |ws, ip, token| {
                    let (rx, tx) = ws.split(tokio::io::split);
                    let (mut rx, tx, worker) = create_channels::<'a, _, Packet<S, C>, Packet<C, S>>(rx, tx);
                    let (mut s_tx, s_rx) = response_channel::channel::<S::Message, S::Response>(BUFFER_SIZE, None);
                    let (rpc_tx, mut rpc_rx) = response_channel::channel::<C::Message, C::Response>(BUFFER_SIZE, None);

                    callback.send(RpcPeer {
                        ip,
                        token,
                        rx: s_rx,
                        rpc: C::new(rpc_tx)
                    }).unwrap();

                    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(BUFFER_SIZE);

                    tokio::spawn(async move {
                        let mut requests = HashMap::new();
                        let mut responses = HashMap::new();

                        loop {
                            match rx.recv().await {
                                Ok(packet) => {
                                    if let Some(packet) = packet {
                                        match packet {
                                            Packet::Request { id, data } => {
                                                if let Err(_) = s_tx.send((data, out_tx.clone())).await {
                                                    eprintln!("RPC response channel closed");
                                                    break;
                                                }
                                            }
                                            Packet::Response { id, data } => {

                                            }
                                        }
                                    }
                                }
                                Err(error) => {
                                    eprintln!("{:?}", error);
                                    break;
                                }
                            }
                        }
                    });

                    {
                        let tx = tx.clone();

                        tokio::spawn(async move {
                            loop {
                                if let Some(packet) = out_rx.recv().await {
                                    if let Err(_) = tx.send(&packet) {
                                        eprintln!("WS response channel closed");
                                        break;
                                    }
                                }
                            }
                        });
                    }

                    tokio::spawn(async move {
                        loop {
                            if let Some((packet, listener)) = rpc_rx.recv().await {
                                let id = PACKET_ID.fetch_add(1, Ordering::Relaxed);

                            }
                        }
                    });

                    worker
                })
            }))
            .with_upgrades();

        if let Err(e) = fut.await {
            eprintln!("An error occurred: {:?}", e);
        }
    }
}