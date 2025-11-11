use tokio_util::sync::CancellationToken;

pub type CommandTx = tokio::sync::mpsc::UnboundedSender<Command>;
pub type CommandRx = tokio::sync::mpsc::UnboundedReceiver<Command>;
pub type EventTx = tokio::sync::mpsc::UnboundedSender<Event>;
pub type EventRx = tokio::sync::mpsc::UnboundedReceiver<Event>;

mod esp32;

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
  device: esp32::Esp32Device,
}

impl LedManager {
  pub async fn init(tx: EventTx, rx: CommandRx, cancel: CancellationToken) -> Result<Self, LedError> {
    let device = esp32::Esp32Device::connect(cancel.child_token()).await?;
    Ok(Self { tx, rx, cancel, device })
  }

  async fn run(mut self) {
    tracing::info!("starting LED manager run loop");

    loop {
      tokio::select! {
        cmd = self.rx.recv() => {
          match cmd {
            Some(command) => {
              tracing::trace!(?command, "LED manager received command");
              if let Err(err) = self.device.handle_command(command).await {
                tracing::error!("failed to handle LED command for ESP32: {}", err);

                if let Err(send_err) = self.tx.send(Event::Error(LedError::Esp32(err))) {
                  tracing::error!("failed to propagate LED error: {}", send_err);
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

    tracing::info!("LED manager run loop exited, closing serial port...");
    self.device.close().await.unwrap_or_else(|err| {
      tracing::error!("failed to close ESP32 serial port: {}", err);
    });

    tracing::info!("LED manager shut down");
  }

  pub fn spawn(self) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move { self.run().await })
  }
}

#[derive(thiserror::Error, Debug)]
pub enum LedError {
  #[error("ESP32 device error: {0}")]
  Esp32(#[from] esp32::Esp32Error),
}
