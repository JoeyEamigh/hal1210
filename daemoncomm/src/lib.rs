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
  FadeIn(LedStripState),
  FadeOut,
  Rainbow,
  Breathing(Color),
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
  Nack { reason: String },
  ManualMode { enabled: bool },
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
  SetManualMode { enabled: bool },
  GetManualMode,
}
