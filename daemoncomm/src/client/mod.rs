use futures::{SinkExt, StreamExt};
use tokio::{net::TcpStream, sync::mpsc};
use tokio_util::{codec::Framed, sync::CancellationToken};
use uuid::Uuid;

use crate::{MessageToClient, MessageToServer, MessageToServerData};
use crate::{SOCKET_ADDR, client::codec::Hal1210ClientCodec};

mod codec;

#[derive(Clone)]
pub struct Hal1210Client {
  tx: mpsc::UnboundedSender<MessageToServer>,
  cancel: CancellationToken,
}

impl Hal1210Client {
  pub async fn connect(
    incoming_tx: mpsc::UnboundedSender<MessageToClient>,
    cancel_token: CancellationToken,
  ) -> Result<Self, ClientError> {
    let stream = TcpStream::connect(SOCKET_ADDR).await?;
    let framed = Framed::new(stream, Hal1210ClientCodec);
    let (mut sink, mut stream) = framed.split();

    let (tx, mut rx) = mpsc::unbounded_channel();

    let reader_token = cancel_token.clone();
    tokio::spawn(async move {
      loop {
        tokio::select! {
          result = stream.next() => {
            match result {
              Some(Ok(msg)) => {
                if let Err(err) = incoming_tx.send(msg) {
                  tracing::debug!("failed to send incoming message to handler: {err}");
                  break;
                }
              }
              Some(Err(e)) => {
                tracing::error!("Error decoding message: {:?}", e);
                break;
              }
              None => {
                tracing::debug!("Stream closed");
                break;
              }
            }
          }
          _ = reader_token.cancelled() => {
            tracing::debug!("Reader task cancelled");
            break;
          }
        }
      }
    });

    let writer_token = cancel_token.clone();
    tokio::spawn(async move {
      loop {
        tokio::select! {
          msg = rx.recv() => {
            match msg {
              Some(msg) => {
                if let Err(e) = sink.send(msg).await {
                  tracing::error!("Error sending message: {:?}", e);
                  break;
                }
              }
              None => {
                tracing::debug!("Message sender dropped");
                break;
              }
            }
          }
          _ = writer_token.cancelled() => {
            tracing::debug!("Writer task cancelled");
            break;
          }
        }
      }
    });

    Ok(Self {
      tx,
      cancel: cancel_token,
    })
  }

  pub fn send(&self, data: MessageToServerData) -> Result<Uuid, ClientError> {
    let id = Uuid::new_v4();
    let msg = MessageToServer { id, data };
    self.tx.send(msg).map_err(|_| ClientError::ConnectionClosed)?;

    Ok(id)
  }

  pub fn cancel(self) {
    self.cancel.cancel();
  }
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
  #[error(transparent)]
  Io(#[from] std::io::Error),
  #[error("Connection closed")]
  ConnectionClosed,
  #[error("Client is cancelled")]
  Cancelled,
}
