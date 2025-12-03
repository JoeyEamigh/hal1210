use daemoncomm::{KinectCommand, KinectEvent, KinectStatus};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use self::freenect::Kinect as FreenectDevice;

mod freenect;
mod opencv;

pub type CommandTx = tokio::sync::mpsc::UnboundedSender<KinectCommand>;
pub type CommandRx = tokio::sync::mpsc::UnboundedReceiver<KinectCommand>;
pub type EventTx = tokio::sync::mpsc::UnboundedSender<KinectEvent>;
pub type EventRx = tokio::sync::mpsc::UnboundedReceiver<KinectEvent>;

const KINECT_VENDOR_ID: &str = "045e";
const KINECT_PRODUCT_IDS: &[&str] = &["02ae", "02ad", "02b0"];

#[derive(Debug)]
enum UdevEvent {
  DeviceAdded,
  DeviceRemoved,
}

type UdevTx = tokio::sync::mpsc::UnboundedSender<UdevEvent>;
type UdevRx = tokio::sync::mpsc::UnboundedReceiver<UdevEvent>;

pub struct KinectManager {
  tx: EventTx,
  rx: CommandRx,
  udev_rx: UdevRx,

  status: String,
  runtime: Option<DeviceRuntime>,

  cancel: CancellationToken,
  _udev_thread: JoinHandle<()>,
}

struct DeviceRuntime {
  device: FreenectDevice,
  depth_active: bool,
  rgb_active: bool,
}

impl KinectManager {
  pub fn init(tx: EventTx, rx: CommandRx, cancel: CancellationToken) -> Self {
    let (udev_tx, udev_rx) = tokio::sync::mpsc::unbounded_channel();
    let _udev_thread = Self::spawn_udev_monitor(udev_tx);

    let mut manager = Self {
      tx,
      rx,
      udev_rx,

      status: "disconnected".to_string(),
      runtime: None,

      cancel,
      _udev_thread,
    };
    manager.try_connect_device();

    manager
  }

  fn spawn_udev_monitor(tx: UdevTx) -> JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
      tracing::debug!("udev monitor thread started");

      let monitor = match udev::MonitorBuilder::new() {
        Ok(builder) => match builder.match_subsystem("usb") {
          Ok(builder) => match builder.listen() {
            Ok(socket) => socket,
            Err(err) => {
              tracing::error!("failed to start udev listener: {err}");
              return;
            }
          },
          Err(err) => {
            tracing::error!("failed to set udev subsystem filter: {err}");
            return;
          }
        },
        Err(err) => {
          tracing::error!("failed to create udev monitor: {err}");
          return;
        }
      };

      tracing::debug!("udev monitor listening for USB events");

      for event in monitor.iter() {
        let device = event.device();

        let vendor = device.property_value("ID_VENDOR_ID");
        let product = device.property_value("ID_MODEL_ID");

        let is_kinect = match (vendor, product) {
          (Some(v), Some(p)) => {
            let v_str = v.to_string_lossy();
            let p_str = p.to_string_lossy();
            v_str == KINECT_VENDOR_ID && KINECT_PRODUCT_IDS.contains(&p_str.as_ref())
          }
          _ => false,
        };

        if !is_kinect {
          continue;
        }

        let event_type = event.event_type();
        tracing::debug!("udev kinect event: {:?}", event_type);

        let udev_event = match event_type {
          udev::EventType::Add | udev::EventType::Bind => Some(UdevEvent::DeviceAdded),
          udev::EventType::Remove | udev::EventType::Unbind => Some(UdevEvent::DeviceRemoved),
          _ => None,
        };

        if let Some(evt) = udev_event {
          if tx.send(evt).is_err() {
            tracing::debug!("udev monitor channel closed; exiting");
            break;
          }
        }
      }

      tracing::debug!("udev monitor thread exiting");
    })
  }

  pub fn spawn(self) -> JoinHandle<()> {
    tokio::spawn(async move { self.run().await })
  }

  async fn run(mut self) {
    tracing::info!("starting Kinect manager run loop");

    loop {
      self.poll_udev_events();

      tokio::select! {
        command = self.rx.recv() => {
          match command {
            Some(cmd) => self.handle_command(cmd).await,
            None => {
              tracing::warn!("Kinect command channel closed; shutting down");
              break;
            }
          }
        }
        _ = self.cancel.cancelled() => {
          tracing::debug!("Kinect manager received cancellation signal");
          break;
        }
        _ = tokio::time::sleep(tokio::time::Duration::from_millis(100)) => {
          // periodic wake-up to check udev events
        }
      }
    }

    tracing::info!("Kinect manager run loop exited");
  }

  fn poll_udev_events(&mut self) {
    while let Ok(event) = self.udev_rx.try_recv() {
      match event {
        UdevEvent::DeviceAdded => {
          if self.runtime.is_none() {
            tracing::info!("Kinect device hotplugged; attempting to initialize");
            std::thread::sleep(std::time::Duration::from_millis(500));
            self.try_connect_device();
          }
        }
        UdevEvent::DeviceRemoved => {
          if self.runtime.is_some() {
            tracing::info!("Kinect device removed");
            self.runtime = None;
            self.status = "removed".to_string();
            self.publish_status();
          }
        }
      }
    }
  }

  fn try_connect_device(&mut self) {
    match FreenectDevice::new(0) {
      Ok(device) => {
        tracing::info!("Kinect device connected successfully");
        self.runtime = Some(DeviceRuntime {
          device,
          depth_active: false,
          rgb_active: false,
        });
        self.status = "connected".to_string();
        self.publish_status();
      }
      Err(err) => {
        tracing::warn!("failed to connect to Kinect device: {err}");
        self.status = err.to_string();
        self.publish_status();
      }
    }
  }

  async fn handle_command(&mut self, command: KinectCommand) {
    match command {
      KinectCommand::RequestStatus => self.publish_status(),
    }
  }

  fn status_snapshot(&self) -> KinectStatus {
    match &self.runtime {
      Some(runtime) => KinectStatus {
        connected: true,
        status: self.status.clone(),
        device_serial: None,
        firmware_version: None,
        depth_active: runtime.depth_active,
        rgb_active: runtime.rgb_active,
      },
      None => KinectStatus {
        connected: false,
        status: self.status.clone(),
        device_serial: None,
        firmware_version: None,
        depth_active: false,
        rgb_active: false,
      },
    }
  }

  fn publish_status(&self) {
    let status = self.status_snapshot();

    if let Err(err) = self.tx.send(KinectEvent::Status(status)) {
      tracing::warn!("failed to publish Kinect status event: {err}");
    }
  }
}

#[derive(Debug, thiserror::Error)]
pub enum KinectError {
  #[error("device unavailable: {reason}")]
  DeviceUnavailable { reason: String },
}
