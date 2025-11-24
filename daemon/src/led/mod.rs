use daemoncomm::{LedCommand, LedStripState};
use ledcomm::zero_state_frame;
use tokio::time;
use tokio_util::sync::CancellationToken;

pub type CommandTx = tokio::sync::mpsc::UnboundedSender<LedCommand>;
pub type CommandRx = tokio::sync::mpsc::UnboundedReceiver<LedCommand>;
pub type EventTx = tokio::sync::mpsc::UnboundedSender<Event>;
pub type EventRx = tokio::sync::mpsc::UnboundedReceiver<Event>;

mod effects;
mod esp32;

use self::effects::{AnimationState, AnimationStatus, DEFAULT_FADE_IN_DURATION, DEFAULT_FADE_OUT_DURATION};

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
  animation: Option<AnimationState>,
  current_frame: LedStripState,
}

impl LedManager {
  pub async fn init(tx: EventTx, rx: CommandRx, cancel: CancellationToken) -> Result<Self, LedError> {
    let device = esp32::Esp32Device::connect(tx.clone(), cancel.child_token()).await?;
    Ok(Self {
      tx,
      rx,
      cancel,
      device,
      animation: None,
      current_frame: ledcomm::zero_state_frame(),
    })
  }

  async fn run(mut self) {
    tracing::info!("starting LED manager run loop");

    loop {
      let animation_sleep = self
        .animation
        .as_ref()
        .map(|animation| time::sleep_until(animation.next_deadline()));
      let has_animation = animation_sleep.is_some();

      tokio::select! {
        cmd = self.rx.recv() => {
          match cmd {
            Some(command) => {
              tracing::trace!(?command, "LED manager received command");
              if let Err(err) = self.handle_led_command(command).await {
                tracing::error!("failed to handle LED command: {}", err);

                if let Err(send_err) = self.tx.send(Event::Error(err)) {
                  tracing::error!("failed to propagate LED error: {}", send_err);
                }
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
        _ = async move {
          if let Some(sleep) = animation_sleep {
            sleep.await;
          }
        }, if has_animation => {
          if let Err(err) = self.step_animation().await {
            tracing::error!("animation tick failed: {}", err);
            self.animation = None;
          }
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

  async fn handle_led_command(&mut self, command: LedCommand) -> Result<(), LedError> {
    match command {
      LedCommand::SetStaticColor(color) => {
        self.cancel_animation();
        self.fill_frame(color);
        self.device.send_strip_state(&self.current_frame).await?;
      }
      LedCommand::SetStripState(state) => {
        if self.try_update_active_fade(&state) {
          tracing::trace!("updated fade-in target with new strip state");
        } else {
          self.cancel_animation();
          self.current_frame = state;
          self.device.send_strip_state(&self.current_frame).await?;
        }
      }
      LedCommand::FadeIn(target) => {
        self.cancel_animation();
        self.current_frame = zero_state_frame();
        self.animation = Some(AnimationState::fade_in(target, DEFAULT_FADE_IN_DURATION));
      }
      LedCommand::FadeOut => {
        self.cancel_animation();
        if self.is_black() {
          tracing::debug!("fade out requested but frame already black; skipping animation");
        } else {
          self.animation = Some(AnimationState::fade_out(&self.current_frame, DEFAULT_FADE_OUT_DURATION));
        }
      }
      LedCommand::Rainbow => {
        self.cancel_animation();
        self.animation = Some(AnimationState::rainbow());
      }
      LedCommand::Breathing(color) => {
        self.cancel_animation();
        self.animation = Some(AnimationState::breathing(color));
      }
    }

    Ok(())
  }

  async fn step_animation(&mut self) -> Result<(), LedError> {
    if self.animation.is_none() {
      return Ok(());
    }

    let status = {
      let animation = self.animation.as_mut().expect("animation checked above");
      let status = animation.render(&mut self.current_frame);
      if matches!(status, AnimationStatus::Running) {
        animation.schedule_next_tick();
      }
      status
    };

    self.device.send_strip_state(&self.current_frame).await?;

    if matches!(status, AnimationStatus::Finished) {
      self.animation = None;
    }

    Ok(())
  }

  fn cancel_animation(&mut self) {
    if self.animation.is_some() {
      tracing::trace!("cancelling active LED animation");
      self.animation = None;
    }
  }

  fn fill_frame(&mut self, color: [u8; 3]) {
    for pixel in self.current_frame.iter_mut() {
      *pixel = color;
    }
  }

  fn try_update_active_fade(&mut self, target: &LedStripState) -> bool {
    let Some(animation) = self.animation.as_mut() else {
      return false;
    };

    animation.update_fade_target(&self.current_frame, target)
  }

  fn is_black(&self) -> bool {
    self
      .current_frame
      .iter()
      .all(|pixel| pixel[0] == 0 && pixel[1] == 0 && pixel[2] == 0)
  }
}

#[derive(thiserror::Error, Debug)]
pub enum LedError {
  #[error("ESP32 device error: {0}")]
  Esp32(#[from] esp32::Esp32Error),
}
