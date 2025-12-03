use std::{
  collections::{HashMap, VecDeque},
  sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
  },
  time::Instant,
};

use daemoncomm::{LedCommand, LedStripState};
use ledcomm::BYTES_PER_LED;
use tokio::sync::oneshot;

use crate::{gpu, led, wayland};

pub type FrameStageTx = tokio::sync::mpsc::UnboundedSender<FrameStage>;
pub type FrameStageRx = tokio::sync::mpsc::UnboundedReceiver<FrameStage>;

pub fn channel() -> (FrameStageTx, FrameStageRx) {
  tokio::sync::mpsc::unbounded_channel()
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum FrameStage {
  LedDispatched { frame_id: u64, state: LedStripState },
  Skipped { frame_id: u64, reason: &'static str },
  Aborted { frame_id: u64, reason: String },
}

pub struct FramePipeline {
  pub last_auto_frame: LedStripState,
  frame_counter: u64,
  frame_starts: HashMap<u64, Instant>,
  frames_inflight: VecDeque<(u64, Instant)>,
}

impl FramePipeline {
  pub fn new() -> Self {
    Self {
      last_auto_frame: ledcomm::zero_state_frame(),
      frame_counter: 0,
      frame_starts: HashMap::new(),
      frames_inflight: VecDeque::new(),
    }
  }

  pub fn next_frame_id(&mut self) -> u64 {
    let id = self.frame_counter;
    self.frame_counter = self.frame_counter.wrapping_add(1);
    id
  }

  pub fn record_start(&mut self, frame_id: u64) {
    self.frame_starts.insert(frame_id, Instant::now());
  }

  pub fn handle_stage(&mut self, stage: FrameStage) {
    match stage {
      FrameStage::LedDispatched { frame_id, state } => {
        self.last_auto_frame = state;

        match self.frame_starts.remove(&frame_id) {
          Some(started_at) => self.frames_inflight.push_back((frame_id, started_at)),
          None => tracing::warn!(frame_id, "received LED dispatch stage without matching frame start"),
        }
      }
      FrameStage::Skipped { frame_id, reason } => {
        if let Some(started_at) = self.frame_starts.remove(&frame_id) {
          let elapsed = started_at.elapsed();
          let elapsed_ms = elapsed.as_secs_f64() * 1000.0;
          tracing::debug!(frame_id, elapsed_ms, reason, "frame skipped before LED dispatch");
        } else {
          tracing::debug!(
            frame_id,
            reason,
            "frame skipped before LED dispatch (no recorded start)"
          );
        }
      }
      FrameStage::Aborted { frame_id, reason } => {
        let mut removed = self.frame_starts.remove(&frame_id).is_some();

        if !removed && let Some(pos) = self.frames_inflight.iter().position(|(id, _)| *id == frame_id) {
          self.frames_inflight.remove(pos);
          removed = true;
        }

        tracing::warn!(
          frame_id,
          removed,
          reason = reason.as_str(),
          "frame aborted before completion"
        );
      }
    }
  }

  #[tracing::instrument(level = "debug", skip_all)]
  pub fn log_completion(&mut self) {
    match self.frames_inflight.pop_front() {
      Some((frame_id, started_at)) => {
        let elapsed = started_at.elapsed();
        let elapsed_ms = elapsed.as_secs_f64() * 1000.0;
        let fps = if elapsed.as_secs_f64() > 0.0 {
          1.0 / elapsed.as_secs_f64()
        } else {
          f64::INFINITY
        };

        tracing::debug!(frame_id, elapsed_ms, fps, "frame pipeline complete");
      }
      None => {
        tracing::trace!("LED manager reported completion with no frame timing in flight");
      }
    }
  }
}

impl Default for FramePipeline {
  fn default() -> Self {
    Self::new()
  }
}

#[allow(clippy::too_many_arguments)]
pub async fn await_compute_dispatch(
  compute_rx: oneshot::Receiver<gpu::ComputeOutput>,
  wayland_tx: wayland::CommandTx,
  led_tx: led::CommandTx,
  frame_id: u64,
  frame_stage_tx: FrameStageTx,
  idle: bool,
  manual_flag: Arc<AtomicBool>,
  fade_in: bool,
) {
  match compute_rx.await {
    Ok(result) => {
      tracing::debug!(frame_id, "compute shader finished successfully: {result:?}");

      if let Some(color) = result.average_rgb_u8() {
        tracing::trace!(
          frame_id,
          r = color[0],
          g = color[1],
          b = color[2],
          "computed average color"
        );

        if idle {
          tracing::debug!("ignoring shader compute result while idle");

          if let Err(err) = led_tx.send(LedCommand::SetStaticColor([0, 0, 0])) {
            tracing::error!("error setting idle led strip state: {err}");
          }
        } else if manual_flag.load(Ordering::Acquire) {
          tracing::debug!("ignoring shader compute result while in manual mode");
        } else {
          let strip_state = std::array::from_fn(|i| {
            let start = i * BYTES_PER_LED;
            color.as_slice()[start..start + BYTES_PER_LED].try_into().unwrap()
          });

          let command = if fade_in {
            LedCommand::FadeIn {
              state: strip_state,
              duration_ms: None,
            }
          } else {
            LedCommand::SetStripState(strip_state)
          };

          match led_tx.send(command) {
            Ok(()) => {
              if frame_stage_tx
                .send(FrameStage::LedDispatched {
                  frame_id,
                  state: strip_state,
                })
                .is_err()
              {
                tracing::error!(frame_id, "failed to record LED dispatch stage");
              }
            }
            Err(err) => {
              tracing::error!("failed to send compute result to LED manager: {err}");
              let _ = frame_stage_tx.send(FrameStage::Aborted {
                frame_id,
                reason: format!("failed to send LED command: {err}"),
              });
            }
          }
        }
      } else {
        tracing::warn!(frame_id, "compute result contained no pixels");
        let _ = frame_stage_tx.send(FrameStage::Skipped {
          frame_id,
          reason: "empty_compute_result",
        });
      }

      if let Err(err) = wayland_tx.send(wayland::Command::ComputeDone) {
        tracing::error!("failed to send compute done command: {err}");
      }
    }
    Err(err) => {
      tracing::error!("compute result channel closed: {err}");
      let _ = frame_stage_tx.send(FrameStage::Aborted {
        frame_id,
        reason: format!("compute channel closed: {err}"),
      });
    }
  };
}
