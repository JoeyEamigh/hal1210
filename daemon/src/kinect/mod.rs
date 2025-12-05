use std::time::{Duration, Instant};

use daemoncomm::{KinectCommand, KinectEvent, KinectStatus};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use self::{
  freenect::{libfreenect, Kinect as FreenectDevice},
  opencv::summarize_rgb_frame,
};

mod freenect;
mod opencv;

pub type CommandTx = tokio::sync::mpsc::UnboundedSender<KinectCommand>;
pub type CommandRx = tokio::sync::mpsc::UnboundedReceiver<KinectCommand>;
pub type EventTx = tokio::sync::mpsc::UnboundedSender<KinectEvent>;
pub type EventRx = tokio::sync::mpsc::UnboundedReceiver<KinectEvent>;

const KINECT_VENDOR_ID: &str = "045e";
const KINECT_PRODUCT_IDS: &[&str] = &["02ae", "02ad", "02b0"];

const RGB_WIDTH: i32 = 1280;
const RGB_HEIGHT: i32 = 1024;
const RGB_MONITOR_INTERVAL: Duration = Duration::from_secs(2);
const RGB_FORMAT: libfreenect::freenect_video_format = libfreenect::freenect_video_format::FREENECT_VIDEO_RGB;
const RGB_RESOLUTION: libfreenect::freenect_resolution = libfreenect::freenect_resolution::FREENECT_RESOLUTION_HIGH;

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
  rgb_task: Option<JoinHandle<()>>,
  cancel: CancellationToken,
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

        if let Some(evt) = udev_event
          && tx.send(evt).is_err()
        {
          tracing::debug!("udev monitor channel closed; exiting");
          break;
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
    if !Self::kinect_present() {
      tracing::debug!("skipping Kinect init; no device currently connected");
      self.status = "not connected".to_string();
      self.publish_status();
      return;
    }

    match FreenectDevice::new(0) {
      Ok(device) => {
        tracing::info!("Kinect device connected successfully");
        let mut runtime = DeviceRuntime::new(device, self.cancel.child_token());
        if runtime.start_rgb_stream() {
          self.status = format!("rgb streaming {RGB_WIDTH}x{RGB_HEIGHT}");
        } else {
          self.status = "connected (rgb unavailable)".to_string();
        }
        self.runtime = Some(runtime);
        self.publish_status();
      }
      Err(err) => {
        tracing::warn!("failed to connect to Kinect device: {err}");
        self.status = err.to_string();
        self.publish_status();
      }
    }
  }

  fn kinect_present() -> bool {
    let mut enumerator = match udev::Enumerator::new() {
      Ok(enumerator) => enumerator,
      Err(err) => {
        tracing::warn!("failed to create udev enumerator: {err}");
        return true;
      }
    };

    if let Err(err) = enumerator.match_subsystem("usb") {
      tracing::warn!("failed to set udev enumerator filter: {err}");
      return true;
    }

    let devices = match enumerator.scan_devices() {
      Ok(devices) => devices,
      Err(err) => {
        tracing::warn!("failed to scan udev devices: {err}");
        return true;
      }
    };

    devices.into_iter().any(|device| {
      let vendor = device.property_value("ID_VENDOR_ID");
      let product = device.property_value("ID_MODEL_ID");

      match (vendor, product) {
        (Some(v), Some(p)) => {
          let v_str = v.to_string_lossy();
          let p_str = p.to_string_lossy();
          v_str == KINECT_VENDOR_ID && KINECT_PRODUCT_IDS.contains(&p_str.as_ref())
        }
        _ => false,
      }
    })
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

impl DeviceRuntime {
  fn new(device: FreenectDevice, cancel: CancellationToken) -> Self {
    Self {
      device,
      depth_active: false,
      rgb_active: false,
      rgb_task: None,
      cancel,
    }
  }

  fn start_rgb_stream(&mut self) -> bool {
    if self.rgb_active {
      return true;
    }

    let Some(video_rx) = self.device.take_video_stream() else {
      tracing::error!("RGB stream channel already claimed; cannot start video");
      return false;
    };

    tracing::info!(width = RGB_WIDTH, height = RGB_HEIGHT, "starting Kinect RGB stream");
    self.device.set_video_format(RGB_FORMAT, RGB_RESOLUTION);
    self.device.start_video();

    let processor_handle = Self::spawn_rgb_processor(video_rx, self.cancel.clone());
    self.rgb_task = Some(processor_handle);
    self.rgb_active = true;
    true
  }

  fn spawn_rgb_processor(
    mut video_rx: tokio::sync::mpsc::UnboundedReceiver<freenect::VideoFrame>,
    cancel: CancellationToken,
  ) -> JoinHandle<()> {
    tokio::spawn(async move {
      let mut total_frames: u64 = 0;
      let mut window_frames: u64 = 0;
      let mut last_report = Instant::now();
      let mut latest_mean: Option<[f64; 3]> = None;

      loop {
        tokio::select! {
          _ = cancel.cancelled() => {
            tracing::debug!("RGB processor received cancellation signal");
            break;
          }
          frame = video_rx.recv() => {
            match frame {
              Some(frame) => {
                total_frames += 1;
                window_frames += 1;

                match tokio::task::spawn_blocking(move || summarize_rgb_frame(frame, RGB_WIDTH, RGB_HEIGHT)).await {
                  Ok(Ok(summary)) => {
                    latest_mean = Some(summary.mean_rgb);
                  }
                  Ok(Err(err)) => {
                    tracing::warn!(?err, "failed to summarize RGB frame");
                  }
                  Err(err) => {
                    tracing::warn!(?err, "RGB summary worker aborted");
                  }
                }
              }
              None => {
                tracing::debug!("RGB video channel closed");
                break;
              }
            }
          }
        }

        let elapsed = last_report.elapsed();
        if elapsed >= RGB_MONITOR_INTERVAL {
          let fps = if elapsed.as_secs_f64() > 0.0 {
            window_frames as f64 / elapsed.as_secs_f64()
          } else {
            0.0
          };

          if let Some(mean) = latest_mean.take() {
            tracing::debug!(
              fps,
              mean_r = mean[0],
              mean_g = mean[1],
              mean_b = mean[2],
              "kinect RGB stream active"
            );
          } else {
            tracing::debug!(fps, "kinect RGB stream active");
          }

          window_frames = 0;
          last_report = Instant::now();
        }
      }

      tracing::info!(total_frames, "kinect RGB processor exiting");
    })
  }
}

impl Drop for DeviceRuntime {
  fn drop(&mut self) {
    self.cancel.cancel();
    if let Some(handle) = self.rgb_task.take() {
      handle.abort();
    }

    if self.rgb_active {
      tracing::info!("stopping Kinect RGB stream");
      self.device.stop_video();
      self.rgb_active = false;
    }
  }
}
