use std::{collections::HashSet, net::SocketAddr};

#[derive(Debug, Default)]
pub struct ClientSessions {
  clients: HashSet<SocketAddr>,
  manual_enabled: bool,
}

impl ClientSessions {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn register(&mut self, addr: SocketAddr) -> bool {
    self.clients.insert(addr)
  }

  pub fn remove(&mut self, addr: SocketAddr) -> DisconnectOutcome {
    if !self.clients.remove(&addr) {
      return DisconnectOutcome::Unknown;
    }

    if self.clients.is_empty() && self.manual_enabled {
      self.manual_enabled = false;
      DisconnectOutcome::ManualDisabled
    } else {
      DisconnectOutcome::Removed
    }
  }

  pub fn manual_enabled(&self) -> bool {
    self.manual_enabled
  }

  pub fn set_manual_enabled(&mut self, enabled: bool) -> ManualTransition {
    if self.manual_enabled == enabled {
      ManualTransition::Unchanged(enabled)
    } else {
      self.manual_enabled = enabled;
      ManualTransition::Changed(enabled)
    }
  }

  pub fn client_count(&self) -> usize {
    self.clients.len()
  }

  pub fn iter(&self) -> impl Iterator<Item = SocketAddr> + '_ {
    self.clients.iter().copied()
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisconnectOutcome {
  Unknown,
  Removed,
  ManualDisabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManualTransition {
  Changed(bool),
  Unchanged(bool),
}

impl ManualTransition {
  pub fn changed(&self) -> bool {
    matches!(self, ManualTransition::Changed(_))
  }

  pub fn enabled(&self) -> bool {
    match self {
      ManualTransition::Changed(state) | ManualTransition::Unchanged(state) => *state,
    }
  }
}
