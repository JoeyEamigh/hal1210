use std::net::SocketAddr;

use daemoncomm::{KinectCommand, KinectEvent, KinectStatus, MessageToClientData};
use uuid::Uuid;

use crate::kinect;

use super::messaging::Messenger;

pub struct KinectHandler {
  tx: kinect::CommandTx,
  last_status: Option<KinectStatus>,
  pending_requests: Vec<(SocketAddr, Uuid)>,
}

impl KinectHandler {
  pub fn new(tx: kinect::CommandTx) -> Self {
    Self {
      tx,
      last_status: None,
      pending_requests: Vec::new(),
    }
  }

  pub fn handle_command(&mut self, addr: SocketAddr, id: Uuid, command: KinectCommand, messenger: &Messenger) {
    match command {
      KinectCommand::RequestStatus => {
        if let Some(status) = &self.last_status {
          messenger.send_kinect_status(addr, id, status.clone());
          return;
        }

        self.pending_requests.push((addr, id));

        if let Err(err) = self.tx.send(KinectCommand::RequestStatus) {
          tracing::warn!("kinect command channel closed during send: {err}");
          self.pending_requests.retain(|(a, i)| !(*a == addr && *i == id));
          messenger.send_kinect_unavailable(addr, id, Some("kinect command channel closed".to_string()));
        }
      }
    }
  }

  pub fn handle_event(&mut self, event: KinectEvent, clients: impl Iterator<Item = SocketAddr>, messenger: &Messenger) {
    match event {
      KinectEvent::Status(status) => {
        self.last_status = Some(status.clone());
        tracing::debug!(connected = status.connected, "received Kinect status update");

        let pending: Vec<_> = self.pending_requests.drain(..).collect();
        for (addr, id) in pending {
          messenger.send_kinect_status(addr, id, status.clone());
        }

        messenger.broadcast(clients, MessageToClientData::Kinect(KinectEvent::Status(status)));
      }
    }
  }
}
