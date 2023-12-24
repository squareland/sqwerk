use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub enum Packet<M, R> {
    Request(M),
    Response(R)
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
                        Ok(match self {
                            $(Message::[<$command:camel>] { $($arg),* } => {
                                let response = handler.$command($($arg),*).await.expect("command cancelled");
                                chan.send(Response::[<$command:camel>](response)).await.expect("outbound channel closed");
                            })*
                        })
                    }
                }

                #[derive(Clone)]
                pub struct $name {
                    chan: Sender<Message, Response>
                }

                impl $name {
                    $vis fn new(chan: Sender<Message, Response>) -> Self {
                        Self { chan }
                    }

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
            }
        }
    };
}