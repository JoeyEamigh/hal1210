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
const STATUS_POLL_INTERVAL_MS: u64 = 1_000;
const POWER_ON_ACTIVE_SOURCE_DELAY_MS: u64 = 1_500;
const POWER_ON_VERIFY_ATTEMPTS: usize = 10;
const POWER_ON_VERIFY_INTERVAL_MS: u64 = 1_000;
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
        tracing::debug!("received CEC power_on request");
        let _ = self.power_on().await;
      }
      ClientCecCommand::PowerOff => {
        tracing::debug!("received CEC power_off request");
        if self.power_off().await {
          let status = if let Some(runtime) = self.runtime.as_mut() {
            runtime.state = CecSnapshot::powered_off();
            Some(runtime.state)
          } else {
            None
          };

          if let Some(status) = status {
            self.publish_status(status.into());
          }
        }
      }
      ClientCecCommand::RequestActiveSource => {
        tracing::trace!("received CEC request_active_source");
        if self.request_active_source().await {
          self.refresh_status().await;
        }
      }
      ClientCecCommand::RequestStatus => {
        tracing::trace!("received CEC request_status");
        self.refresh_status().await;
      }
    }
  }

  async fn power_on(&mut self) -> bool {
    // for attempt in 1..=COMMAND_RETRY_COUNT {
    //   if !self.ensure_connection().await {
    //     time::sleep(Duration::from_millis(COMMAND_RETRY_DELAY_MS)).await;
    //     continue;
    //   }

    //   tracing::trace!(attempt, "issuing CEC power_on");

    //   let status = match self.runtime.as_mut() {
    //     None => None,
    //     Some(runtime) => {
    //       if let Err(err) = runtime.power_on() {
    //         tracing::warn!(attempt, "CEC power_on failed: {err}");
    //         None
    //       } else {
    //         time::sleep(Duration::from_millis(POWER_ON_ACTIVE_SOURCE_DELAY_MS)).await;

    //         if REQUEST_ACTIVE_SOURCE_ON_POWER {
    //           let _ = runtime.set_active_source();
    //         }

    //         if Self::verify_power_on(runtime).await {
    //           tracing::debug!(attempt, "CEC power_on verified");
    //           Some(runtime.state)
    //         } else {
    //           tracing::debug!(attempt, "CEC power_on verification failed; will retry");
    //           None
    //         }
    //       }
    //     }
    //   };

    //   if let Some(status) = status {
    //     self.publish_status(status.into());
    //     return true;
    //   }

    //   tracing::trace!(attempt, "dropping CEC runtime after failed power_on path");
    //   self.runtime = None;
    //   time::sleep(Duration::from_millis(COMMAND_RETRY_DELAY_MS)).await;
    // }

    // self.publish_error(format!(
    //   "cec command power_on failed after {COMMAND_RETRY_COUNT} attempts"
    // ));
    // false

    for attempt in 1..=COMMAND_RETRY_COUNT {
      if !self.ensure_connection().await {
        time::sleep(Duration::from_millis(COMMAND_RETRY_DELAY_MS)).await;
        continue;
      }

      tracing::trace!(
        attempt,
        "issuing CEC power_on via power_off->set_active_source workaround for Samsung TVs"
      );

      let status = match self.runtime.as_mut() {
        None => None,
        Some(runtime) => {
          if let Err(err) = runtime.power_off() {
            tracing::warn!(attempt, "CEC power_off (workaround) failed: {err}");
            None
          } else {
            time::sleep(Duration::from_millis(POWER_ON_ACTIVE_SOURCE_DELAY_MS)).await;

            if let Err(err) = runtime.set_active_source() {
              tracing::warn!(attempt, "CEC set_active_source (workaround) failed: {err}");
              None
            } else {
              time::sleep(Duration::from_millis(POWER_ON_ACTIVE_SOURCE_DELAY_MS)).await;

              if Self::verify_power_on(runtime).await {
                tracing::debug!(attempt, "CEC power_on workaround verified");
                Some(runtime.state)
              } else {
                tracing::debug!(attempt, "CEC power_on workaround verification failed; will retry");
                None
              }
            }
          }
        }
      };

      if let Some(status) = status {
        self.publish_status(status.into());
        return true;
      }

      tracing::trace!(attempt, "dropping CEC runtime after failed power_on workaround");
      self.runtime = None;
      time::sleep(Duration::from_millis(COMMAND_RETRY_DELAY_MS)).await;
    }

    self.publish_error(format!(
      "cec command power_on failed after {COMMAND_RETRY_COUNT} attempts"
    ));
    false
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

    let status = match self.runtime.as_mut() {
      Some(runtime) => {
        if runtime.state.powered_on {
          runtime.refresh_state().unwrap_or(runtime.state)
        } else {
          tracing::trace!("skipping CEC refresh_state because TV is off/unknown");
          runtime.state
        }
      }
      None => return,
    };

    tracing::trace!(
      powered_on = status.powered_on,
      active_source = status.active_source,
      "publishing refreshed CEC status"
    );
    self.publish_status(status.into());
  }

  async fn poll_status(&mut self) {
    if !self.ensure_connection().await {
      return;
    }

    let status = match self.runtime.as_mut() {
      Some(runtime) if runtime.state.powered_on => runtime.refresh_state(),
      _ => None,
    };

    if let Some(snapshot) = status {
      tracing::trace!(
        powered_on = snapshot.powered_on,
        active_source = snapshot.active_source,
        "publishing polled CEC status change"
      );
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
        self.runtime = Some(runtime);
        tracing::debug!("connected CEC runtime; publishing initial status");
        self.publish_status(status);
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

      tracing::trace!(label, attempt, "issuing CEC command with retry");

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

  async fn verify_power_on(runtime: &mut CecRuntime) -> bool {
    for attempt in 0..POWER_ON_VERIFY_ATTEMPTS {
      tracing::trace!(attempt, "verifying CEC power_on state");
      time::sleep(Duration::from_millis(POWER_ON_VERIFY_INTERVAL_MS)).await;

      let snapshot = runtime.refresh_state().unwrap_or(runtime.state);
      if snapshot.powered_on {
        if REQUEST_ACTIVE_SOURCE_ON_POWER {
          let _ = runtime.set_active_source();
          let _ = runtime.refresh_state();
        }
        tracing::trace!(
          attempt,
          powered_on = snapshot.powered_on,
          active_source = snapshot.active_source,
          "CEC power_on verified by snapshot"
        );
        return true;
      }

      if let Err(err) = runtime.power_on() {
        tracing::warn!("power_on verification resend failed: {err}");
        break;
      }

      if REQUEST_ACTIVE_SOURCE_ON_POWER {
        let _ = runtime.set_active_source();
      }
    }

    tracing::debug!("CEC power_on verification exhausted attempts");
    false
  }

  fn publish_status(&self, status: ClientCecStatus) {
    tracing::trace!(
      powered_on = status.powered_on,
      active_source = status.active_source,
      "forwarding CEC status event"
    );
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
    // let state = CecSnapshot::powered_off();
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
  fn powered_off() -> Self {
    Self {
      powered_on: false,
      active_source: false,
    }
  }

  fn query(connection: &CecConnection) -> Self {
    let power_status = connection.get_device_power_status(TV_LOGICAL_ADDRESS);
    let powered_on = matches!(power_status, CecPowerStatus::On);

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
