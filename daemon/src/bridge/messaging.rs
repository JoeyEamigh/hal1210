use std::net::SocketAddr;

use daemoncomm::{KinectEvent, KinectStatus, LedCommand, MessageToClient, MessageToClientData};
use uuid::Uuid;

use crate::{client, led};

pub struct Messenger {
  led_tx: led::CommandTx,
  client_tx: client::ServerResTx,
}

impl Messenger {
  pub fn new(led_tx: led::CommandTx, client_tx: client::ServerResTx) -> Self {
    Self { led_tx, client_tx }
  }

  pub fn led_tx(&self) -> led::CommandTx {
    self.led_tx.clone()
  }

  pub fn ack(&self, addr: SocketAddr, id: Uuid) {
    self.send_reply(addr, id, MessageToClientData::Ack);
  }

  pub fn nack<S: Into<String>>(&self, addr: SocketAddr, id: Uuid, reason: S) {
    self.send_reply(addr, id, MessageToClientData::Nack { reason: reason.into() });
  }

  pub fn send_manual_state(&self, addr: SocketAddr, id: Uuid, enabled: bool) {
    self.send_reply(addr, id, MessageToClientData::ManualMode { enabled });
  }

  pub fn send_idle_inhibit_state(&self, addr: SocketAddr, id: Uuid, enabled: bool, timeout_ms: Option<u64>) {
    self.send_reply(addr, id, MessageToClientData::IdleInhibit { enabled, timeout_ms });
  }

  pub fn send_kinect_status(&self, addr: SocketAddr, id: Uuid, status: KinectStatus) {
    self.send_reply(addr, id, MessageToClientData::Kinect(KinectEvent::Status(status)));
  }

  pub fn send_kinect_unavailable(&self, addr: SocketAddr, id: Uuid, reason: Option<String>) {
    let status = KinectStatus {
      connected: false,
      status: reason.unwrap_or_else(|| "kinect device unavailable".to_string()),
      device_serial: None,
      firmware_version: None,
      depth_active: false,
      rgb_active: false,
    };

    self.send_reply(addr, id, MessageToClientData::Kinect(KinectEvent::Status(status)));
  }

  pub fn send_led_command(&self, command: LedCommand) {
    if let Err(err) = self.led_tx.send(command) {
      tracing::error!("failed to send LED command: {err}");
    }
  }

  pub fn send_reply(&self, addr: SocketAddr, id: Uuid, data: MessageToClientData) {
    let message = MessageToClient { id, data };
    if let Err(err) = self.client_tx.send(client::ServerRes::new(addr, message)) {
      tracing::error!(addr = %addr, "failed to send response to client: {err}");
    }
  }

  pub fn broadcast<I: Iterator<Item = SocketAddr>>(&self, addrs: I, data: MessageToClientData) {
    for addr in addrs {
      self.send_reply(addr, Uuid::nil(), data.clone());
    }
  }
}
