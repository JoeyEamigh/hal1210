use ledcomm::BYTES_PER_LED;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

mod codec;
mod ser_de;

#[cfg(feature = "server")]
pub mod server;

#[cfg(feature = "client")]
pub mod client;

pub const SOCKET_ADDR: &str = "127.0.0.1:1210";

pub type Color = [u8; BYTES_PER_LED];
pub type LedStripState = ledcomm::StateFrame;

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum LedCommand {
  SetStaticColor(Color),
  SetStripState(LedStripState),
  FadeIn {
    state: LedStripState,
    duration_ms: Option<u64>,
  },
  FadeOut {
    duration_ms: Option<u64>,
  },
  Rainbow,
  Breathing(Color),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KinectStatus {
  pub connected: bool,
  pub status: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub device_serial: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub firmware_version: Option<String>,
  pub depth_active: bool,
  pub rgb_active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "camelCase")]
pub enum KinectEvent {
  Status(KinectStatus),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "camelCase")]
pub enum KinectCommand {
  RequestStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageToClient {
  pub id: Uuid,
  #[serde(flatten)]
  pub data: MessageToClientData,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "camelCase")]
pub enum MessageToClientData {
  Ack,
  Nack {
    reason: String,
  },
  ManualMode {
    enabled: bool,
  },
  IdleInhibit {
    enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout_ms: Option<u64>,
  },
  Kinect(KinectEvent),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageToServer {
  pub id: Uuid,
  #[serde(flatten)]
  pub data: MessageToServerData,
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "camelCase")]
pub enum MessageToServerData {
  Led(LedCommand),
  SetManualMode {
    enabled: bool,
  },
  GetManualMode,
  SetIdleInhibit {
    enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout_ms: Option<u64>,
  },
  GetIdleInhibit,
  Kinect(KinectCommand),
}
