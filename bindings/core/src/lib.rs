use std::sync::{Arc, Once};

use daemoncomm::{client::Hal1210Client, MessageToClient, MessageToServerData};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::{broadcast, mpsc, Mutex as AsyncMutex};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub use daemoncomm::{LedCommand, LedStripState};

static TRACING_ONCE: Once = Once::new();

pub fn init_tracing() {
  TRACING_ONCE.call_once(|| {
    use tracing::metadata::LevelFilter;
    use tracing_subscriber::{fmt, prelude::__tracing_subscriber_SubscriberExt, util::SubscriberInitExt, EnvFilter};

    let filter_directives = std::env::var("RUST_LOG").unwrap_or_else(|_| {
      "pyhal1210client=trace,nodehal1210client=trace,hal1210client_core=trace,daemoncomm=trace".to_string()
    });

    let filter = EnvFilter::builder()
      .with_default_directive(LevelFilter::TRACE.into())
      .parse_lossy(filter_directives);

    if let Err(err) = tracing_subscriber::registry()
      .with(filter)
      .with(fmt::layer().with_target(true))
      .try_init()
    {
      eprintln!("hal1210client_core: failed to initialize tracing subscriber: {err}");
    }
  });
}

#[derive(Clone)]
pub struct ClientHandle {
  inner: Arc<ClientInner>,
}

struct ClientInner {
  client: Hal1210Client,
  dispatcher: broadcast::Sender<MessageToClient>,
  next_rx: AsyncMutex<broadcast::Receiver<MessageToClient>>,
}

impl ClientHandle {
  pub async fn connect() -> Result<Self, BindingError> {
    tracing::debug!("hal1210client_core: connecting to hal1210 daemon");
    let (incoming_tx, mut incoming_rx) = mpsc::unbounded_channel();
    let cancel_token = CancellationToken::new();
    let client = Hal1210Client::connect(incoming_tx, cancel_token.clone()).await?;
    let (dispatcher, _) = broadcast::channel(64);
    let next_rx = dispatcher.subscribe();

    let forwarder = dispatcher.clone();
    tokio::spawn(async move {
      while let Some(message) = incoming_rx.recv().await {
        let message_id = message.id;
        match forwarder.send(message) {
          Ok(_) => tracing::trace!(%message_id, "hal1210client_core: dispatched inbound message"),
          Err(err) => tracing::debug!(%message_id, %err, "hal1210client_core: failed to dispatch inbound message"),
        }
      }
    });

    Ok(Self {
      inner: Arc::new(ClientInner {
        client,
        dispatcher,
        next_rx: AsyncMutex::new(next_rx),
      }),
    })
  }

  pub fn send(&self, data: MessageToServerData) -> Result<Uuid, BindingError> {
    let id = self.inner.client.send(data)?;
    tracing::trace!(%id, "hal1210client_core: sent MessageToServerData");
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
    let mut rx = self.inner.next_rx.lock().await;
    match rx.recv().await {
      Ok(message) => {
        tracing::trace!(%message.id, "hal1210client_core: delivered message via next_message");
        Some(message)
      }
      Err(err) => {
        tracing::debug!(%err, "hal1210client_core: next_message receiver closed");
        None
      }
    }
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
  #[error("client not connected")]
  NotConnected,
}

pub type BindingResult<T> = Result<T, BindingError>;
