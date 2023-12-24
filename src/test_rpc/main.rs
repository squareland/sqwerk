use std::future::{Future};
use futures::TryFutureExt;
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;
use sqwerk::{rpc, serve};

const BUFFER_SIZE: usize = 10;

pub trait RPC {
    // type ClientPacket = Packet<server::Message, client::Response>;
    // type ServerPacket = Packet<client::Message, server::Response>;
}

rpc! {
    pub trait Client {
        fn notify(text: String) -> ();
    }
}

rpc! {
    pub trait Server {
        fn connect() -> ();
        fn login(login: String, password: String) -> ();
    }
}

#[tokio::main]
async fn main() {
    let (c_tx, c_rx) = response_channel::channel::<client::Message, client::Response>(BUFFER_SIZE, None);
    let (s_tx, s_rx) = response_channel::channel::<server::Message, server::Response>(BUFFER_SIZE, None);

    let server = server::Server::new(s_tx);
    let client = client::Client::new(c_tx);

    let st = async move {
        let server = server;
        let mut s_rx = s_rx;

        struct Handler {

        }

        impl server::Handler for Handler {
            fn connect(&mut self) -> JoinHandle<()> {
                tokio::spawn(async {})
            }

            fn login(&mut self, login: String, password: String) -> JoinHandle<String> {
                tokio::spawn(async {
                    String::from("TOKEN")
                })
            }
        }

        loop {
            if let Some((message, listener)) = s_rx.recv().await {
                message.handle_by(&mut Handler {}, listener).await?;
            } else {
                break;
            }
        }
    };
    let ct = async move {
        let client = client;
        let mut c_rx = c_rx;
    };

    tokio::join!(st, ct);
}

