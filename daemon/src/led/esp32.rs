use std::{env, path::PathBuf, time::Duration};

use ledcomm::{BYTES_PER_LED, NUM_LEDS, WRITE_FEEDBACK_LEN, parse_write_feedback};
use tokio::io::{self, AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio_serial::{SerialPortBuilderExt, SerialPortType, SerialStream, UsbPortInfo};
use tokio_util::sync::CancellationToken;

use super::{Event, EventTx, LedStripState};

const DEFAULT_BAUD: u32 = 5_000_000;
const CONNECT_TIMEOUT: Duration = Duration::from_millis(100);
const ENV_PORT: &str = "WAYLED_ESP32_PORT";
const RX_BUFFER_LEN: usize = 512;

pub struct Esp32Device {
  tx: WriteHalf<SerialStream>,
  frame: ledcomm::Frame,
}

impl Esp32Device {
  pub async fn connect(event_tx: EventTx, cancel: CancellationToken) -> Result<Self, Esp32Error> {
    let port_path = resolve_port()?;
    let port_string = port_path.to_string_lossy().into_owned();

    let builder = tokio_serial::new(port_string.clone(), DEFAULT_BAUD)
      .data_bits(tokio_serial::DataBits::Eight)
      .flow_control(tokio_serial::FlowControl::None)
      .parity(tokio_serial::Parity::None)
      .stop_bits(tokio_serial::StopBits::One)
      .timeout(CONNECT_TIMEOUT);

    let stream = builder.open_native_async().map_err(|source| Esp32Error::Open {
      path: port_path.clone(),
      source,
    })?;

    let (reader, writer) = tokio::io::split(stream);
    spawn_feedback_listener(reader, cancel, event_tx);

    let frame = ledcomm::Frame::new(ledcomm::MAGIC, (NUM_LEDS * BYTES_PER_LED) as u16);

    tracing::info!("connected to ESP32 LED controller: {port_path:?} at {DEFAULT_BAUD} baud");

    Ok(Self { tx: writer, frame })
  }

  pub async fn close(mut self) -> Result<(), Esp32Error> {
    if let Err(err) = self.send_static_color([0, 0, 0]).await {
      tracing::warn!("failed to send ESP32 blackout frame during shutdown: {err}");
    } else {
      tracing::trace!("ESP32 blackout frame sent before shutdown");
    }

    self.tx.shutdown().await.map_err(Esp32Error::Write)
  }

  pub async fn handle_command(&mut self, command: super::Command) -> Result<(), Esp32Error> {
    match command {
      super::Command::SetStaticColor(color) => {
        tracing::debug!(r = color[0], g = color[1], b = color[2], "ESP32 static color command");
        self.send_static_color(color).await
      }
      super::Command::SetStripState(state) => {
        tracing::debug!("ESP32 strip state command");
        self.send_strip_state(&state).await
      }
    }
  }

  async fn send_static_color(&mut self, color: [u8; 3]) -> Result<(), Esp32Error> {
    self.frame.magic = ledcomm::MAGIC;
    self.frame.len = (NUM_LEDS * BYTES_PER_LED) as u16;
    for pixel in self.frame.data.iter_mut() {
      *pixel = color;
    }
    tracing::trace!("prepared static color frame for ESP32");
    self.write_frame().await
  }

  async fn send_strip_state(&mut self, state: &LedStripState) -> Result<(), Esp32Error> {
    self.frame.magic = ledcomm::MAGIC;
    self.frame.len = (state.len() * BYTES_PER_LED) as u16;
    self.frame.data.copy_from_slice(state);
    tracing::trace!(payload_bytes = self.frame.len, "prepared strip state frame for ESP32");
    self.write_frame().await
  }

  #[tracing::instrument(level = "trace", skip_all)]
  async fn write_frame(&mut self) -> Result<(), Esp32Error> {
    let packet = self.frame.packet();
    tracing::trace!("ESP32 serial write: {} bytes", packet.len());
    self.tx.write_all(packet).await?;
    self.tx.flush().await?;
    Ok(())
  }
}

fn resolve_port() -> Result<PathBuf, Esp32Error> {
  if let Ok(path) = env::var(ENV_PORT) {
    return Ok(PathBuf::from(path));
  }

  let ports = tokio_serial::available_ports()?;
  let mut candidates = ports.iter().filter_map(|info| match &info.port_type {
    SerialPortType::UsbPort(usb) if is_esp32_usb(usb) => Some(PathBuf::from(&info.port_name)),
    _ => None,
  });

  if let Some(path) = candidates.next() {
    return Ok(path);
  }

  ports
    .into_iter()
    .find(|info| {
      info.port_name.contains("ttyACM") || info.port_name.contains("ttyUSB") || info.port_name.contains("cu.usb")
    })
    .map(|info| PathBuf::from(info.port_name))
    .ok_or(Esp32Error::NotFound)
}

fn is_esp32_usb(usb: &UsbPortInfo) -> bool {
  if usb.vid == 0x303A || usb.vid == 0x10C4 {
    return true;
  }

  usb
    .product
    .as_deref()
    .map(|name| name.to_ascii_uppercase().contains("ESP"))
    .unwrap_or(false)
}

fn spawn_feedback_listener(mut reader: ReadHalf<SerialStream>, cancel: CancellationToken, event_tx: EventTx) {
  tokio::spawn(async move {
    let mut buf = vec![0u8; RX_BUFFER_LEN];
    let mut backlog = Vec::with_capacity(RX_BUFFER_LEN * 2);

    loop {
      tokio::select! {
        read = reader.read(&mut buf) => {
          match read {
            Ok(0) => {
              if !backlog.is_empty() {
                let text = String::from_utf8_lossy(&backlog);
                tracing::debug!("ESP32 RX trailing data: {text}");
                backlog.clear();
              }
              tracing::warn!("ESP32 serial stream closed");
              break;
            }
            Ok(n) => {
              backlog.extend_from_slice(&buf[..n]);

              loop {
                match parse_write_feedback(&backlog) {
                  Some((feedback, consumed)) => {
                    backlog.drain(..consumed);

                    let ingest_ms = feedback.ingest_us as f32 / 1000.0;
                    let copy_ms = feedback.copy_us as f32 / 1000.0;
                    let spi_ms = feedback.spi_us as f32 / 1000.0;
                    let total_ms = feedback.total_us as f32 / 1000.0;
                    tracing::trace!("ESP32 frame complete: ingest={ingest_ms}ms copy={copy_ms}ms spi={spi_ms}ms total={total_ms}ms");

                    if let Err(err) = event_tx.send(Event::Done) {
                      tracing::warn!("ESP32 feedback listener failed to send done event: {err}");
                      return;
                    }
                  }
                  None => {
                    if backlog.len() > WRITE_FEEDBACK_LEN * 4 {
                      let drop = backlog.len().saturating_sub(WRITE_FEEDBACK_LEN * 2);
                      backlog.drain(..drop);
                    }
                    break;
                  }
                }
              }
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => {
              tracing::error!("ESP32 serial read error: {err}");
              break;
            }
          }
        }
        _ = cancel.cancelled() => {
          tracing::debug!("ESP32 RX logger received cancellation signal; exiting");
          break;
        }
      }
    }
  });
}

#[derive(thiserror::Error, Debug)]
pub enum Esp32Error {
  #[error("no ESP32 serial devices were found; set {ENV_PORT} to override")]
  NotFound,
  #[error("failed to enumerate serial ports: {0}")]
  Enumeration(#[from] tokio_serial::Error),
  #[error("failed to open serial port {path}: {source}")]
  Open {
    path: PathBuf,
    #[source]
    source: tokio_serial::Error,
  },
  #[error("serial write failed: {0}")]
  Write(#[from] std::io::Error),
}

// #[cfg(test)]
// mod test {
//   use super::*;
//   use tokio_util::sync::CancellationToken;
//   use tracing::metadata::LevelFilter;
//   use tracing_subscriber::{
//     filter::Directive,
//     fmt::{self, format::FmtSpan},
//     prelude::__tracing_subscriber_SubscriberExt,
//     util::SubscriberInitExt,
//     EnvFilter, Layer,
//   };

//   #[tokio::test]
//   async fn test_open_serial_port() {
//     let filter = EnvFilter::builder()
//       .with_default_directive(Directive::from(LevelFilter::TRACE))
//       .parse_lossy("wayled=trace");

//     tracing_subscriber::registry()
//       .with(fmt::layer().with_span_events(FmtSpan::CLOSE).with_filter(filter))
//       .init();

//     let cancel = CancellationToken::new();
//     let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
//     let result = Esp32Device::connect(event_tx, cancel.child_token()).await;

//     let mut port = match result {
//       Ok(port) => port,
//       Err(Esp32Error::NotFound) => panic!("No ESP32 device found"),
//       Err(e) => panic!("Unexpected error: {e}"),
//     };

//     println!("Serial port opened successfully");

//     let color = [255, 0, 0];
//     port
//       .send_static_color(color)
//       .await
//       .expect("failed to send static color");

//     // wait for a single feedback packet to verify the loop is running
//     let _ = timeout(Duration::from_secs(1), event_rx.recv()).await;

//     // sleep for a bit to allow any RX logging to occur
//     tokio::time::sleep(Duration::from_secs(2)).await;

//     cancel.cancel();
//     port.tx.shutdown().await.expect("shutdown failed");
//   }
// }
