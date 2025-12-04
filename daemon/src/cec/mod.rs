use std::time::Duration;

use cec_rs::{
  CecAdapterType, CecCommand, CecConnection, CecConnectionCfgBuilder, CecConnectionResultError, CecDeviceType,
  CecDeviceTypeVec, CecKeypress, CecLogMessage, CecLogicalAddress, CecPowerStatus,
};
use daemoncomm::{CecCommand as ClientCecCommand, CecEvent as ClientCecEvent, CecStatus as ClientCecStatus};
use tokio::{task::JoinHandle, time};
use tokio_util::sync::CancellationToken;

pub type CommandTx = tokio::sync::mpsc::UnboundedSender<ClientCecCommand>;
pub type CommandRx = tokio::sync::mpsc::UnboundedReceiver<ClientCecCommand>;
pub type EventTx = tokio::sync::mpsc::UnboundedSender<ClientCecEvent>;
pub type EventRx = tokio::sync::mpsc::UnboundedReceiver<ClientCecEvent>;

const DEVICE_NAME: &str = "PC";
const LOGICAL_ADDRESS: CecLogicalAddress = CecLogicalAddress::Playbackdevice1;
const TV_LOGICAL_ADDRESS: CecLogicalAddress = CecLogicalAddress::Tv;
const COMMAND_RETRY_COUNT: usize = 10;
const COMMAND_RETRY_DELAY_MS: u64 = 500;
const STATUS_POLL_INTERVAL_MS: u64 = 1000;
const REQUEST_ACTIVE_SOURCE_ON_POWER: bool = true;

pub struct CecManager {
  tx: EventTx,
  rx: CommandRx,

  runtime: Option<CecRuntime>,

  cancel: CancellationToken,
}

impl CecManager {
  pub async fn init(tx: EventTx, rx: CommandRx, cancel: CancellationToken) -> Self {
    let runtime = CecRuntime::connect()
      .await
      .map_err(|err| {
        tracing::error!("failed to initialize CEC connection: {err}");
        err
      })
      .ok();

    Self {
      tx,
      rx,
      runtime,
      cancel,
    }
  }

  pub fn spawn(self) -> JoinHandle<()> {
    tokio::spawn(async move { self.run().await })
  }

  async fn run(mut self) {
    tracing::info!("starting CEC manager run loop");
    let mut poll = time::interval(Duration::from_millis(STATUS_POLL_INTERVAL_MS));
    poll.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

    loop {
      tokio::select! {
        Some(cmd) = self.rx.recv() => {
          self.handle_command(cmd).await;
        }
        _ = poll.tick() => {
          self.poll_status().await;
        }
        _ = self.cancel.cancelled() => {
          tracing::debug!("CEC manager received cancellation signal");
          break;
        }
        else => break,
      }
    }

    tracing::info!("CEC manager exited");
  }

  async fn handle_command(&mut self, command: ClientCecCommand) {
    match command {
      ClientCecCommand::PowerOn => {
        if self.power_on().await {
          if REQUEST_ACTIVE_SOURCE_ON_POWER {
            let _ = self.request_active_source().await;
          }
          self.refresh_status().await;
        }
      }
      ClientCecCommand::PowerOff => {
        if self.power_off().await {
          self.refresh_status().await;
        }
      }
      ClientCecCommand::RequestActiveSource => {
        if self.request_active_source().await {
          self.refresh_status().await;
        }
      }
      ClientCecCommand::RequestStatus => {
        self.refresh_status().await;
      }
    }
  }

  async fn power_on(&mut self) -> bool {
    self.send_with_retry("power_on", |runtime| runtime.power_on()).await
  }

  async fn power_off(&mut self) -> bool {
    self.send_with_retry("power_off", |runtime| runtime.power_off()).await
  }

  async fn request_active_source(&mut self) -> bool {
    self
      .send_with_retry("request_active_source", |runtime| runtime.set_active_source())
      .await
  }

  async fn refresh_status(&mut self) {
    if !self.ensure_connection().await {
      return;
    }

    if let Some(runtime) = self.runtime.as_mut() {
      let snapshot = runtime.refresh_state().unwrap_or(runtime.state);
      self.publish_status(snapshot.into());
    }
  }

  async fn poll_status(&mut self) {
    if !self.ensure_connection().await {
      return;
    }

    if let Some(runtime) = self.runtime.as_mut()
      && let Some(snapshot) = runtime.refresh_state()
    {
      self.publish_status(snapshot.into());
    }
  }

  async fn ensure_connection(&mut self) -> bool {
    if self.runtime.is_some() {
      return true;
    }

    match CecRuntime::connect().await {
      Ok(runtime) => {
        let status = runtime.state.into();
        self.publish_status(status);
        self.runtime = Some(runtime);
        true
      }
      Err(err) => {
        tracing::warn!("failed to reconnect CEC adapter: {err}");
        self.publish_error(format!("cec reconnection failed: {err}"));
        false
      }
    }
  }

  async fn send_with_retry<F>(&mut self, label: &str, op: F) -> bool
  where
    F: Fn(&CecRuntime) -> Result<(), CecManagerError> + Copy,
  {
    for attempt in 1..=COMMAND_RETRY_COUNT {
      if !self.ensure_connection().await {
        time::sleep(Duration::from_millis(COMMAND_RETRY_DELAY_MS)).await;
        continue;
      }

      let success = {
        let runtime = self.runtime.as_ref().expect("runtime available");
        op(runtime)
      };

      match success {
        Ok(()) => {
          return true;
        }
        Err(err) => {
          tracing::warn!(attempt, label, "CEC command failed: {err}");
          self.runtime = None;
        }
      }

      time::sleep(Duration::from_millis(COMMAND_RETRY_DELAY_MS)).await;
    }

    self.publish_error(format!(
      "cec command {label} failed after {COMMAND_RETRY_COUNT} attempts"
    ));
    false
  }

  fn publish_status(&self, status: ClientCecStatus) {
    if let Err(err) = self.tx.send(ClientCecEvent::Status(status)) {
      tracing::debug!("failed to forward CEC status event: {err}");
    }
  }

  fn publish_error<S: Into<String>>(&self, message: S) {
    if let Err(err) = self.tx.send(ClientCecEvent::Error {
      message: message.into(),
    }) {
      tracing::debug!("failed to forward CEC error event: {err}");
    }
  }
}

struct CecRuntime {
  connection: CecConnection,
  state: CecSnapshot,
}

impl CecRuntime {
  async fn connect() -> Result<Self, CecInitError> {
    tokio::task::spawn_blocking(Self::open_connection)
      .await
      .map_err(|err| CecInitError::Task(err.to_string()))?
  }

  fn open_connection() -> Result<Self, CecInitError> {
    let cfg = CecConnectionCfgBuilder::default()
      .get_settings_from_rom(true)
      .device_name(DEVICE_NAME.to_string())
      .device_types(CecDeviceTypeVec::new(CecDeviceType::PlaybackDevice))
      .base_device(LOGICAL_ADDRESS)
      .key_press_callback(Box::new(on_key_press))
      .command_received_callback(Box::new(on_command_received))
      .log_message_callback(Box::new(on_log_level))
      .adapter_type(CecAdapterType::P8External)
      .open_timeout(Duration::from_secs(5))
      .build()
      .map_err(|err| CecInitError::Config(err.to_string()))?;

    let connection = cfg.open().map_err(CecInitError::Connection)?;
    let state = CecSnapshot::query(&connection);
    Ok(Self { connection, state })
  }

  fn refresh_state(&mut self) -> Option<CecSnapshot> {
    let snapshot = CecSnapshot::query(&self.connection);
    if snapshot != self.state {
      self.state = snapshot;
      Some(snapshot)
    } else {
      None
    }
  }

  fn power_on(&self) -> Result<(), CecManagerError> {
    self
      .connection
      .send_power_on_devices(TV_LOGICAL_ADDRESS)
      .map_err(CecManagerError::from)
  }

  fn power_off(&self) -> Result<(), CecManagerError> {
    self
      .connection
      .send_standby_devices(TV_LOGICAL_ADDRESS)
      .map_err(CecManagerError::from)
  }

  fn set_active_source(&self) -> Result<(), CecManagerError> {
    self
      .connection
      .set_active_source(CecDeviceType::PlaybackDevice)
      .map_err(CecManagerError::from)
  }
}

fn on_key_press(keypress: CecKeypress) {
  tracing::trace!(
    "onKeyPress: {:?}, keycode: {:?}, duration: {:?}",
    keypress,
    keypress.keycode,
    keypress.duration
  );
}

fn on_command_received(command: CecCommand) {
  tracing::trace!(
    "onCommandReceived:  opcode: {:?}, initiator: {:?}",
    command.opcode,
    command.initiator
  );
}

fn on_log_level(log_message: CecLogMessage) {
  tracing::trace!(
    "logMessageRecieved:  time: {}, level: {}, message: {}",
    log_message.time.as_secs(),
    log_message.level,
    log_message.message
  );
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CecSnapshot {
  powered_on: bool,
  active_source: bool,
}

impl CecSnapshot {
  fn query(connection: &CecConnection) -> Self {
    let power_status = connection.get_device_power_status(TV_LOGICAL_ADDRESS);
    let powered_on = matches!(
      power_status,
      CecPowerStatus::On | CecPowerStatus::InTransitionStandbyToOn
    );

    let active_source = connection.is_active_source(LOGICAL_ADDRESS);

    Self {
      powered_on,
      active_source,
    }
  }
}

impl From<CecSnapshot> for ClientCecStatus {
  fn from(value: CecSnapshot) -> Self {
    Self {
      powered_on: value.powered_on,
      active_source: value.active_source,
    }
  }
}

#[derive(Debug, thiserror::Error)]
enum CecInitError {
  #[error("cec configuration error: {0}")]
  Config(String),
  #[error("cec connection error: {0:?}")]
  Connection(CecConnectionResultError),
  #[error("cec connection task failed: {0}")]
  Task(String),
}

#[derive(Debug, thiserror::Error)]
enum CecManagerError {
  #[error("cec transmit failed: {0:?}")]
  Transmit(CecConnectionResultError),
}

impl From<CecConnectionResultError> for CecManagerError {
  fn from(value: CecConnectionResultError) -> Self {
    CecManagerError::Transmit(value)
  }
}
