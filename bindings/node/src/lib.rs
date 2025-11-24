#![deny(clippy::all)]

use std::sync::{Arc, Mutex};

use hal1210client_core::{init_tracing, BindingError, ClientHandle};
use napi::bindgen_prelude::{Error, Result};
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi::Status;
use napi_derive::napi;
use serde_json::Value;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace};

#[napi]
pub struct Hal1210Client {
  inner: ClientHandle,
  listeners: Arc<Mutex<Vec<Arc<ListenerHandle>>>>,
  runtime: tokio::runtime::Handle,
}

#[napi]
impl Hal1210Client {
  #[napi(factory)]
  pub async fn connect(enable_tracing: Option<bool>) -> Result<Self> {
    if enable_tracing.unwrap_or(false) {
      init_tracing();
      debug!("nodehal1210client: tracing initialized");
    }
    debug!("nodehal1210client: connecting to daemon");
    let inner = ClientHandle::connect().await.map_err(to_napi_error)?;
    let runtime = tokio::runtime::Handle::current();
    Ok(Self {
      inner,
      listeners: Arc::new(Mutex::new(Vec::new())),
      runtime,
    })
  }

  #[napi(ts_args_type = "payload: MessageToServerData", ts_return_type = "string")]
  pub fn send(&self, payload: Value) -> Result<String> {
    let id = self.inner.send_json(payload).map_err(to_napi_error)?;
    trace!(%id, "nodehal1210client: dispatched message");
    Ok(id.to_string())
  }

  #[napi(ts_return_type = "Promise<MessageToClient | null>")]
  pub async fn next_message(&self) -> Result<Option<Value>> {
    let result = self.inner.next_message_json().await.map_err(to_napi_error);
    trace!("nodehal1210client: next_message resolved");
    result
  }

  #[napi]
  pub fn cancel(&self) {
    self.stop_all_listeners();
    self.inner.cancel();
    debug!("nodehal1210client: cancel requested");
  }

  #[napi(
    ts_type = "(callback: (error: null, message: MessageToClient) => void): void;\nonMessage(callback: (error: Error, message: null) => void): void;"
  )]
  pub fn on_message(&self, callback: ThreadsafeFunction<Value>) -> Result<()> {
    let mut rx = self.inner.subscribe();
    let tsfn = Arc::new(callback);
    let worker_tsfn = tsfn.clone();
    let cancel = CancellationToken::new();
    let stopper = cancel.clone();

    let handle: JoinHandle<()> = self.runtime.spawn(async move {
      loop {
        tokio::select! {
          _ = stopper.cancelled() => break,
          result = rx.recv() => {
            match result {
              Ok(msg) => {
                trace!(%msg.id, "nodehal1210client: received broadcast message");
                let payload = match serde_json::to_value(msg) {
                  Ok(value) => value,
                  Err(err) => {
                    let _ = worker_tsfn.call(
                      Err(Error::from_reason(format!("failed to serialize message: {err}"))),
                      ThreadsafeFunctionCallMode::NonBlocking,
                    );
                    break;
                  }
                };
                trace!(payload = %payload, "nodehal1210client: forwarding payload to callback");
                let status = worker_tsfn.call(Ok(payload), ThreadsafeFunctionCallMode::NonBlocking);
                if status != Status::Ok {
                  debug!("nodehal1210client: on_message callback returned error status");
                  break;
                }
              }
              Err(broadcast::error::RecvError::Closed) => break,
              Err(broadcast::error::RecvError::Lagged(_)) => continue,
            }
          }
        }
      }
    });

    let listener = Arc::new(ListenerHandle::new(cancel, handle, tsfn));
    let mut guard = self.listeners.lock().expect("listeners mutex poisoned");
    guard.push(listener);
    Ok(())
  }
}

struct ListenerHandle {
  cancel: CancellationToken,
  handle: Mutex<Option<JoinHandle<()>>>,
  #[allow(dead_code)]
  callback: Arc<ThreadsafeFunction<Value>>,
}

impl ListenerHandle {
  fn new(cancel: CancellationToken, handle: JoinHandle<()>, callback: Arc<ThreadsafeFunction<Value>>) -> Self {
    Self {
      cancel,
      handle: Mutex::new(Some(handle)),
      callback,
    }
  }

  fn stop(&self) {
    self.cancel.cancel();
    if let Some(handle) = self.handle.lock().expect("subscription mutex poisoned").take() {
      handle.abort();
    }
  }
}

impl Drop for ListenerHandle {
  fn drop(&mut self) {
    self.cancel.cancel();
    if let Ok(inner) = self.handle.get_mut() {
      if let Some(handle) = inner.take() {
        handle.abort();
      }
    }
  }
}

impl Hal1210Client {
  fn stop_all_listeners(&self) {
    let listeners = {
      let mut guard = self.listeners.lock().expect("listeners mutex poisoned");
      guard.drain(..).collect::<Vec<_>>()
    };
    for listener in listeners {
      listener.stop();
    }
  }
}

impl Drop for Hal1210Client {
  fn drop(&mut self) {
    self.stop_all_listeners();
  }
}

fn to_napi_error(err: BindingError) -> Error {
  Error::from_reason(err.to_string())
}
