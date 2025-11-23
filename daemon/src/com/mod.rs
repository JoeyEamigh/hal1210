use core::net::SocketAddr;
use std::{collections::HashMap, io};

use daemoncomm::{server::codec::Hal1210ServerCodec, MessageToClient, MessageToServer, SOCKET_ADDR};

use futures::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::{codec::Framed, sync::CancellationToken};

#[derive(Debug, Clone)]
pub struct ClientReq {
  addr: SocketAddr,
  msg: MessageToServer,
}

impl ClientReq {
  pub fn new(addr: SocketAddr, msg: MessageToServer) -> Self {
    Self { addr, msg }
  }
}

#[derive(Debug, Clone)]
pub struct ServerRes {
  addr: SocketAddr,
  msg: MessageToClient,
}

impl ServerRes {
  pub fn new(addr: SocketAddr, msg: MessageToClient) -> Self {
    Self { addr, msg }
  }
}

pub type ClientReqTx = tokio::sync::mpsc::UnboundedSender<ClientReq>;
pub type ClientReqRx = tokio::sync::mpsc::UnboundedReceiver<ClientReq>;
pub type ServerResTx = tokio::sync::mpsc::UnboundedSender<ServerRes>;
pub type ServerResRx = tokio::sync::mpsc::UnboundedReceiver<ServerRes>;

struct ClientConnEvent {
  addr: SocketAddr,
  data: ClientConnEventData,
}

enum ClientConnEventData {
  Message(MessageToServer),
  Disconnect,
}

impl From<MessageToServer> for ClientConnEventData {
  fn from(msg: MessageToServer) -> Self {
    ClientConnEventData::Message(msg)
  }
}

type ClientConnTx = tokio::sync::mpsc::UnboundedSender<MessageToClient>;
type ClientConnRx = tokio::sync::mpsc::UnboundedReceiver<MessageToClient>;
type ClientConnManTx = tokio::sync::mpsc::UnboundedSender<ClientConnEvent>;
type ClientConnManRx = tokio::sync::mpsc::UnboundedReceiver<ClientConnEvent>;

struct ClientConnHandle {
  tx: ClientConnTx,
  handle: tokio::task::JoinHandle<()>,
  cancel: CancellationToken,
}

pub struct ComMan {
  listener: TcpListener,

  tx: ClientReqTx,
  rx: ServerResRx,

  client_tx: ClientConnManTx,
  client_rx: ClientConnManRx,
  clients: HashMap<SocketAddr, ClientConnHandle>,

  cancel: CancellationToken,
}

impl ComMan {
  pub async fn init(
    client_req_tx: ClientReqTx,
    server_res_rx: ServerResRx,
    cancel: CancellationToken,
  ) -> Result<Self, ComError> {
    let listener = TcpListener::bind(SOCKET_ADDR).await?;
    let (client_tx, client_rx) = tokio::sync::mpsc::unbounded_channel();

    Ok(Self {
      listener,

      tx: client_req_tx,
      rx: server_res_rx,

      client_tx,
      client_rx,
      clients: Default::default(),

      cancel,
    })
  }

  async fn run(&mut self) {
    loop {
      tokio::select! {
        conn = self.listener.accept() => {
          let (stream, addr) = match conn {
            Ok(conn) => conn,
            Err(err) => {
              tracing::error!("failed to accept incoming connection: {}", err);
              continue;
            }
          };

          tracing::info!("accepting client controller connection from {addr}");
          let (client_req_tx, client_req_rx) = tokio::sync::mpsc::unbounded_channel();
          let client_cancel = self.cancel.child_token();
          let mut client_conn = ClientConn::new(
            addr,
            stream,
            self.client_tx.clone(),
            client_req_rx,
            client_cancel.clone(),
          );
          let handle = tokio::spawn(async move { client_conn.run().await });

          self.clients.insert(
            addr,
            ClientConnHandle {
              tx: client_req_tx,
              handle,
              cancel: client_cancel,
            },
          );
        },
        Some(event) = self.client_rx.recv() => {
          match event.data {
            ClientConnEventData::Message(msg) => {
              tracing::trace!("forwarding message from client {}: {:?}", event.addr, msg);
              if let Err(err) = self.tx.send(ClientReq::new(event.addr, msg)) {
                tracing::error!("failed to forward message from client {} to manager: {}", event.addr, err);
              }
            }
            ClientConnEventData::Disconnect => {
              tracing::info!("client {} disconnected; removing connection", event.addr);
              if let Some(client) = self.clients.remove(&event.addr) {
                client.cancel.cancel();
                match client.handle.await {
                  Ok(_) => tracing::debug!("client connection to {} exited successfully", event.addr),
                  Err(err) => tracing::error!("client connection to {} panicked: {}", event.addr, err),
                }
              }
            }
          }
        },
        Some(res) = self.rx.recv() => {
          if let Some(client) = self.clients.get(&res.addr) {
            tracing::trace!("sending message to client {}: {:?}", res.addr, res.msg);
            if let Err(err) = client.tx.send(res.msg) {
              tracing::error!("failed to send message to client {}: {}", res.addr, err);
            }
          } else {
            tracing::warn!("attempted to send message to unknown client {}; ignoring", res.addr);
          }
        },
        _ = self.cancel.cancelled() => {
          tracing::debug!("communication manager received cancellation signal; exiting run loop");
          break;
        }
      };
    }

    // drain the clients; start by cancelling each of them then awaiting their handles
    for (addr, client) in &self.clients {
      tracing::debug!("cancelling client connection to {addr}");
      client.cancel.cancel();
    }

    for (addr, client) in self.clients.drain() {
      tracing::trace!("awaiting client handle to {addr}");
      match client.handle.await {
        Ok(_) => tracing::debug!("client connection to {addr} exited successfully"),
        Err(err) => tracing::error!("client connection to {addr} panicked: {}", err),
      }
    }

    tracing::info!("communication manager exited");
  }

  pub fn spawn(mut self) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move { self.run().await })
  }
}

struct ClientConn {
  addr: SocketAddr,
  stream: Framed<TcpStream, Hal1210ServerCodec>,

  tx: ClientConnManTx,
  rx: ClientConnRx,

  cancel: CancellationToken,
}

impl ClientConn {
  pub fn new(
    addr: SocketAddr,
    stream: TcpStream,

    server_res_tx: ClientConnManTx,
    client_req_rx: ClientConnRx,

    cancel: CancellationToken,
  ) -> Self {
    Self {
      addr,
      stream: Framed::new(stream, Hal1210ServerCodec),

      tx: server_res_tx,
      rx: client_req_rx,

      cancel,
    }
  }

  async fn run(&mut self) {
    loop {
      tokio::select! {
        Some(req) = self.rx.recv() => {
          tracing::trace!("{}: sending message to client: {:?}", self.addr, req);
          if let Err(err) = self.stream.send(req).await {
            tracing::error!("{}: error sending message to client: {}", self.addr, err);
            continue;
          }
        },
        res = self.stream.next() => {
          match res {
            Some(Ok(msg)) => {
              tracing::trace!("{}: received message from client: {:?}", self.addr, msg);
              if let Err(err) = self.tx.send(self.event_from_msg(msg)) {
                tracing::error!("{}: failed to forward message to manager: {}", self.addr, err);
              }
            }
            Some(Err(err)) => {
              tracing::error!("{}: error decoding client message: {}", self.addr, err);
              continue;
            }
            None => {
              tracing::debug!("{}: client disconnected; notifying manager", self.addr);
              if let Err(err) = self.tx.send(self.disconnect_event()) {
                tracing::error!("{}: failed to notify manager of client disconnection: {}", self.addr, err);
              }
              break;
            }
          }
        }
        _ = self.cancel.cancelled() => {
          tracing::debug!("{}: client connection received cancellation signal; exiting run loop", self.addr);
          break;
        }
      }
    }
  }

  fn event_from_msg(&self, data: MessageToServer) -> ClientConnEvent {
    ClientConnEvent {
      addr: self.addr,
      data: data.into(),
    }
  }

  fn disconnect_event(&self) -> ClientConnEvent {
    ClientConnEvent {
      addr: self.addr,
      data: ClientConnEventData::Disconnect,
    }
  }
}

#[derive(Debug, thiserror::Error)]
pub enum ComError {
  #[error(transparent)]
  Io(#[from] io::Error),
}
