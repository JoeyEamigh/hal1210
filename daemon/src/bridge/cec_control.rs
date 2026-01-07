use std::net::SocketAddr;

use daemoncomm::{CecCommand, CecEvent, CecStatus, MessageToClientData};
use uuid::Uuid;

use crate::cec;

use super::messaging::Messenger;

pub struct CecController {
  tx: cec::CommandTx,
  last_status: Option<CecStatus>,
  pending_status: Vec<(SocketAddr, Uuid)>,
}

impl CecController {
  pub fn new(tx: cec::CommandTx) -> Self {
    Self {
      tx,
      last_status: None,
      pending_status: Vec::new(),
    }
  }

  pub fn handle_event(
    &mut self,
    event: CecEvent,
    clients: impl Iterator<Item = SocketAddr>,
    messenger: &Messenger,
  ) -> Option<CecStatus> {
    match event {
      CecEvent::Status(status) => {
        self.last_status = Some(status.clone());
        let pending = std::mem::take(&mut self.pending_status);
        for (addr, id) in pending {
          messenger.send_cec_status(addr, id, status.clone());
          messenger.ack(addr, id);
        }

        messenger.broadcast(clients, MessageToClientData::Cec(CecEvent::Status(status.clone())));
        Some(status)
      }
      CecEvent::Error { message } => {
        tracing::warn!(message = %message, "CEC manager reported error");
        messenger.broadcast(clients, MessageToClientData::Cec(CecEvent::Error { message }));
        None
      }
    }
  }

  pub fn handle_client_command(&mut self, addr: SocketAddr, id: Uuid, command: CecCommand, messenger: &Messenger) {
    match command {
      CecCommand::RequestStatus => {
        if let Some(status) = &self.last_status {
          messenger.send_cec_status(addr, id, status.clone());
          messenger.ack(addr, id);
          return;
        }

        self.pending_status.push((addr, id));
        if !self.send(CecCommand::RequestStatus) {
          let _ = self.pending_status.pop();
          messenger.nack(addr, id, "cec_unavailable");
        }
      }
      other => {
        if !self.send(other) {
          messenger.nack(addr, id, "cec_unavailable");
          return;
        }
        messenger.ack(addr, id);
      }
    }
  }

  pub fn request_status(&self) {
    let _ = self.send(CecCommand::RequestStatus);
  }

  pub fn power_on(&self) {
    let _ = self.send(CecCommand::PowerOn);
  }

  pub fn power_off(&self) {
    let _ = self.send(CecCommand::PowerOff);
  }

  // pub fn request_active_source(&self) {
  //   let _ = self.send(CecCommand::RequestActiveSource);
  // }

  fn send(&self, command: CecCommand) -> bool {
    if let Err(err) = self.tx.send(command) {
      tracing::warn!("failed to send CEC command: {err}");
      false
    } else {
      true
    }
  }
}
