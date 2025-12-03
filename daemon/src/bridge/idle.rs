use std::time::{Duration, Instant};

use tokio::{task::JoinHandle, time};

pub type IdleTimeoutTx = tokio::sync::mpsc::UnboundedSender<()>;
pub type IdleTimeoutRx = tokio::sync::mpsc::UnboundedReceiver<()>;

pub fn channel() -> (IdleTimeoutTx, IdleTimeoutRx) {
  tokio::sync::mpsc::unbounded_channel()
}

pub struct IdleState {
  pub seat_idle: bool,
  pub idle_inhibit: bool,
  idle_inhibit_requested_timeout: Option<u64>,
  idle_inhibit_deadline: Option<Instant>,
  idle_inhibit_task: Option<JoinHandle<()>>,
  idle_timeout_tx: IdleTimeoutTx,
  cancel_token: tokio_util::sync::CancellationToken,
}

impl IdleState {
  pub fn new(idle_timeout_tx: IdleTimeoutTx, cancel_token: tokio_util::sync::CancellationToken) -> Self {
    Self {
      seat_idle: false,
      idle_inhibit: false,
      idle_inhibit_requested_timeout: None,
      idle_inhibit_deadline: None,
      idle_inhibit_task: None,
      idle_timeout_tx,
      cancel_token,
    }
  }

  pub fn effective_idle(&self) -> bool {
    self.seat_idle && !self.idle_inhibit
  }

  pub fn apply_inhibit(&mut self, enabled: bool, timeout_ms: Option<u64>) -> bool {
    let prev_enabled = self.idle_inhibit;
    let prev_timeout = self.idle_inhibit_requested_timeout;

    if enabled {
      self.idle_inhibit_requested_timeout = timeout_ms;
      if let Some(ms) = timeout_ms {
        self.start_timer(ms);
      } else {
        self.cancel_timer();
        self.idle_inhibit_deadline = None;
      }
    } else {
      self.idle_inhibit_requested_timeout = None;
      self.idle_inhibit_deadline = None;
      self.cancel_timer();
    }

    self.idle_inhibit = enabled;

    prev_enabled != enabled || (enabled && prev_timeout != timeout_ms)
  }

  pub fn remaining_inhibit_ms(&self) -> Option<u64> {
    if !self.idle_inhibit {
      return None;
    }

    self
      .idle_inhibit_deadline
      .map(|deadline| deadline.saturating_duration_since(Instant::now()).as_millis() as u64)
  }

  pub fn on_timeout(&mut self) {
    self.idle_inhibit_task = None;
    if !self.idle_inhibit {
      return;
    }

    tracing::info!("idle inhibit timeout expired; disabling idle inhibit");
    self.idle_inhibit = false;
    self.idle_inhibit_requested_timeout = None;
    self.idle_inhibit_deadline = None;
  }

  pub fn cancel_timer(&mut self) {
    if let Some(handle) = self.idle_inhibit_task.take() {
      handle.abort();
    }
  }

  fn start_timer(&mut self, duration_ms: u64) {
    self.cancel_timer();

    if duration_ms == 0 {
      self.idle_inhibit_deadline = Some(Instant::now());
      if let Err(err) = self.idle_timeout_tx.send(()) {
        tracing::error!("failed to dispatch immediate idle inhibit timeout: {err}");
      }
      return;
    }

    let tx = self.idle_timeout_tx.clone();
    let cancel = self.cancel_token.child_token();
    let duration = Duration::from_millis(duration_ms);
    let handle = tokio::spawn(async move {
      tokio::select! {
        _ = time::sleep(duration) => {
          let _ = tx.send(());
        }
        _ = cancel.cancelled() => {}
      }
    });

    self.idle_inhibit_task = Some(handle);
    self.idle_inhibit_deadline = Some(Instant::now() + duration);
  }
}
