use tokio_util::sync::CancellationToken;

use crate::gpu;

pub type CommandTx = tokio::sync::mpsc::UnboundedSender<Command>;
pub type CommandRx = tokio::sync::mpsc::UnboundedReceiver<Command>;
pub type EventTx = tokio::sync::mpsc::UnboundedSender<Event>;
pub type EventRx = tokio::sync::mpsc::UnboundedReceiver<Event>;

#[derive(Debug)]
pub enum Command {
  HandleColors(gpu::ComputeOutput),
}

#[derive(Debug)]
pub enum Event {
  Done,
  Error(LedError),
}

pub struct LedManager {
  tx: EventTx,
  rx: CommandRx,

  cancel: CancellationToken,
}

impl LedManager {
  pub fn new(tx: EventTx, rx: CommandRx, cancel: CancellationToken) -> Self {
    Self { tx, rx, cancel }
  }

  #[tracing::instrument(skip_all)]
  fn handle_frame(&mut self, colors: gpu::ComputeOutput) {
    tracing::debug!("LED manager received compute output");
  }

  async fn run(&mut self) {
    tracing::info!("starting LED manager run loop");

    loop {
      tokio::select! {
        Some(cmd) = self.rx.recv() => {
          tracing::trace!("handling LED command: {:?}", cmd);

          match cmd {
            Command::HandleColors(colors) => self.handle_frame(colors),
          }
        }
        _ = self.cancel.cancelled() => {
          tracing::debug!("LED manager received cancellation signal; exiting run loop");
          break;
        }
      }
    }
  }

  pub fn spawn(mut self) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move { self.run().await })
  }
}

#[derive(thiserror::Error, Debug)]
pub enum LedError {
  #[error("LED error: {0}")]
  Generic(String),
}
