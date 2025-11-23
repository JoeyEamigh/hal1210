use std::sync::Arc;

use daemoncomm::{client::Hal1210Client, MessageToClient, MessageToServerData};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub use daemoncomm::{LedCommand, LedStripState};

#[derive(Clone)]
pub struct ClientHandle {
  inner: Arc<ClientInner>,
}

struct ClientInner {
  client: Hal1210Client,
  dispatcher: broadcast::Sender<MessageToClient>,
}

impl ClientHandle {
  pub async fn connect() -> Result<Self, BindingError> {
    let (incoming_tx, mut incoming_rx) = mpsc::unbounded_channel();
    let cancel_token = CancellationToken::new();
    let client = Hal1210Client::connect(incoming_tx, cancel_token.clone()).await?;
    let (dispatcher, _) = broadcast::channel(64);

    let forwarder = dispatcher.clone();
    tokio::spawn(async move {
      while let Some(message) = incoming_rx.recv().await {
        let _ = forwarder.send(message);
      }
    });

    Ok(Self {
      inner: Arc::new(ClientInner { client, dispatcher }),
    })
  }

  pub fn send(&self, data: MessageToServerData) -> Result<Uuid, BindingError> {
    let id = self.inner.client.send(data)?;
    Ok(id)
  }

  pub fn send_json(&self, payload: Value) -> Result<Uuid, BindingError> {
    let message: MessageToServerData = serde_json::from_value(payload)?;
    self.send(message)
  }

  pub fn subscribe(&self) -> broadcast::Receiver<MessageToClient> {
    self.inner.dispatcher.subscribe()
  }

  pub async fn next_message(&self) -> Option<MessageToClient> {
    let mut rx = self.subscribe();
    rx.recv().await.ok()
  }

  pub async fn next_message_json(&self) -> Result<Option<Value>, BindingError> {
    let message = self.next_message().await;
    match message {
      Some(msg) => Ok(Some(serde_json::to_value(msg)?)),
      None => Ok(None),
    }
  }

  pub fn cancel(&self) {
    self.inner.client.clone().cancel();
  }
}

impl Drop for ClientInner {
  fn drop(&mut self) {
    self.client.clone().cancel();
  }
}

#[derive(Debug, Error)]
pub enum BindingError {
  #[error(transparent)]
  Client(#[from] daemoncomm::client::ClientError),
  #[error(transparent)]
  Json(#[from] serde_json::Error),
}

pub type BindingResult<T> = Result<T, BindingError>;
