use mystic::MysticDevice;
use tokio_util::sync::CancellationToken;

pub type CommandTx = tokio::sync::mpsc::UnboundedSender<Command>;
pub type CommandRx = tokio::sync::mpsc::UnboundedReceiver<Command>;
pub type EventTx = tokio::sync::mpsc::UnboundedSender<Event>;
pub type EventRx = tokio::sync::mpsc::UnboundedReceiver<Event>;

mod mystic;

pub type LedStripState = ledcomm::StateFrame;

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum Command {
  SetStaticColor([u8; 3]),
  SetStripState(LedStripState),
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
  mystic: MysticDevice,
}

impl LedManager {
  pub async fn init(tx: EventTx, rx: CommandRx, cancel: CancellationToken) -> Result<Self, LedError> {
    let mystic = mystic::MysticDevice::connect().await?;
    Ok(Self { tx, rx, cancel, mystic })
  }

  async fn run(mut self) {
    tracing::info!("starting LED manager run loop");

    loop {
      tokio::select! {
        cmd = self.rx.recv() => {
          match cmd {
            Some(command) => {
              if let Err(err) = self.mystic.handle_command(command).await {
                tracing::error!("failed to handle LED command with Mystic: {}", err);

                if let Err(send_err) = self.tx.send(Event::Error(LedError::Mystic(err))) {
                  tracing::error!("failed to propagate Mystic run error: {}", send_err);
                }
              } else if let Err(send_err) = self.tx.send(Event::Done) {
                tracing::error!("failed to send LED done event: {}", send_err);
              }
            }
            None => {
              tracing::debug!("LED command channel closed; exiting run loop");
              break;
            }
          }
        }
        _ = self.cancel.cancelled() => {
          tracing::debug!("LED manager received cancellation signal; exiting run loop");
          break;
        }
      }
    }
  }

  pub fn spawn(self) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move { self.run().await })
  }
}

#[derive(thiserror::Error, Debug)]
pub enum LedError {
  #[error("Mystic controller error: {0}")]
  Mystic(#[from] mystic::MysticError),
}
