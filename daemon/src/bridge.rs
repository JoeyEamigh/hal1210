use std::{
  collections::{HashMap, VecDeque},
  time::Instant,
};

use daemoncomm::LedCommand;
use tokio::{sync::oneshot, task::JoinHandle};

use crate::{
  com, gpu, led, monitoring,
  wayland::{self, idle::IdleEvent},
};

type FrameStageTx = tokio::sync::mpsc::UnboundedSender<FrameStage>;
type FrameStageRx = tokio::sync::mpsc::UnboundedReceiver<FrameStage>;

#[derive(Debug)]
enum FrameStage {
  LedDispatched { frame_id: u64 },
  Skipped { frame_id: u64, reason: &'static str },
  Aborted { frame_id: u64, reason: String },
}

pub struct Handler {
  compute: gpu::Compute,

  // communication channels
  wayland_tx: wayland::CommandTx,
  wayland_rx: wayland::EventRx,
  led_tx: led::CommandTx,
  led_rx: led::EventRx,
  com_man_tx: com::ServerResTx,
  com_man_rx: com::ClientReqRx,

  // state
  idle: bool,
  manual_mode: bool,

  // frame lifetime
  frame_stage_tx: FrameStageTx,
  frame_stage_rx: FrameStageRx,
  frame_counter: u64,
  frame_starts: HashMap<u64, Instant>,
  frames_inflight: VecDeque<(u64, Instant)>,

  // control
  loop_signal: calloop::LoopSignal,
  cancel_token: tokio_util::sync::CancellationToken,
}

#[allow(clippy::too_many_arguments)]
impl Handler {
  pub fn new(
    compute: gpu::Compute,

    wayland_tx: wayland::CommandTx,
    wayland_rx: wayland::EventRx,
    led_tx: led::CommandTx,
    led_rx: led::EventRx,
    com_man_tx: com::ServerResTx,
    com_man_rx: com::ClientReqRx,

    loop_signal: calloop::LoopSignal,
    cancel_token: tokio_util::sync::CancellationToken,
  ) -> Self {
    let (frame_stage_tx, frame_stage_rx) = tokio::sync::mpsc::unbounded_channel();

    Self {
      compute,

      wayland_tx,
      wayland_rx,
      led_tx,
      led_rx,
      com_man_tx,
      com_man_rx,

      idle: false,
      manual_mode: false,

      frame_stage_tx,
      frame_stage_rx,
      frame_counter: 0,
      frame_starts: HashMap::new(),
      frames_inflight: VecDeque::new(),

      loop_signal,
      cancel_token,
    }
  }

  async fn handle_wayland_event(&mut self, event: wayland::Event) {
    match event {
      wayland::Event::Idle(idle_state) => match idle_state {
        IdleEvent::Idle => {
          self.idle = true;
          tracing::debug!("turning off leds since seat is idle");

          if let Err(err) = self.led_tx.send(LedCommand::SetStaticColor([0, 0, 0])) {
            tracing::error!("failed to send turn off command to LED manager: {}", err);
          }
        }
        IdleEvent::Active => {
          self.idle = false;
          tracing::debug!("seat is active again, restarting frame loop");
          if let Err(err) = self.wayland_tx.send(wayland::Command::ComputeDone) {
            tracing::error!("failed to restart frame loop: {}", err);
          }
        }
      },
      wayland::Event::DmabufCreated(dmabuf) => {
        tracing::debug!("setting DMA-BUF in compute module");

        if let Err(err) = self.compute.set_screen_dmabuf(dmabuf) {
          tracing::error!("failed to set DMA-BUF: {}", err);
        }
      }
      wayland::Event::FrameReady => {
        if self.idle {
          tracing::debug!("skipping frame processing because seat is idle");
          return;
        }

        // TODO: remember to send ComputeDone when manual mode is disabled
        if self.manual_mode {
          tracing::debug!("skipping frame processing because manual mode is enabled");
          return;
        }

        tracing::debug!("wayland frame is ready for processing");
        let frame_id = self.frame_counter;
        self.frame_counter = self.frame_counter.wrapping_add(1);
        let started_at = Instant::now();

        match self.compute.dispatch() {
          Ok(compute_rx) => {
            self.frame_starts.insert(frame_id, started_at);

            let wayland_tx = self.wayland_tx.clone();
            let led_tx = self.led_tx.clone();
            let frame_stage_tx = self.frame_stage_tx.clone();
            let idle = self.idle;
            let manual_mode = self.manual_mode;

            tokio::spawn(async move {
              await_compute_dispatch(
                compute_rx,
                wayland_tx,
                led_tx,
                frame_id,
                frame_stage_tx,
                idle,
                manual_mode,
              )
              .await;
            });
          }
          Err(gpu::ComputeError::DispatchInFlight) => {
            tracing::warn!(frame_id, "compute dispatch already in flight, skipping frame");
          }
          Err(gpu::ComputeError::NoDmabuf) => {
            tracing::error!(frame_id, "compute module has no DMA-BUF to process yet");
          }
          Err(gpu::ComputeError::EmptyDmabuf) => {
            tracing::error!(frame_id, "DMA-BUF had zero size, skipping frame");
          }
          Err(err) => {
            tracing::error!(frame_id, "failed to dispatch compute shader: {}", err);
          }
        }
      }
    }
  }

  async fn handle_led_event(&mut self, event: led::Event) {
    match event {
      led::Event::Done => {
        tracing::debug!("LED manager reported done processing");
        self.log_frame_pipeline();
      }
      led::Event::Error(err) => {
        tracing::error!("LED manager reported error: {}", err);
      }
    };
  }

  async fn run(&mut self) {
    loop {
      tokio::select! {
        biased;

        Some(stage) = self.frame_stage_rx.recv() => {
          self.handle_frame_stage(stage);
        }
        Some(event) = self.led_rx.recv() => {
          tracing::trace!("received event from LED manager: {:?}", event);
          self.handle_led_event(event).await;
        }
        Some(event) = self.wayland_rx.recv() => {
          tracing::trace!("received event from wayland: {:?}", event);
          self.handle_wayland_event(event).await;
        }
        _ = monitoring::wait_for_signal() => {
          tracing::info!("shutdown signal received, stopping...");
          break;
        }
      }
    }

    self.cancel_token.cancel();
    self.loop_signal.stop();
    self.compute.wait_for_idle();

    tracing::debug!("scheduled event loops for shutdown");
  }

  pub fn spawn(mut self) -> JoinHandle<()> {
    tokio::spawn(async move { self.run().await })
  }

  fn handle_frame_stage(&mut self, stage: FrameStage) {
    match stage {
      FrameStage::LedDispatched { frame_id } => match self.frame_starts.remove(&frame_id) {
        Some(started_at) => {
          self.frames_inflight.push_back((frame_id, started_at));
        }
        None => {
          tracing::warn!(frame_id, "received LED dispatch stage without matching frame start");
        }
      },
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
  fn log_frame_pipeline(&mut self) {
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
        tracing::warn!("LED manager reported completion with no frame timing in flight");
      }
    }
  }
}

async fn await_compute_dispatch(
  compute_rx: oneshot::Receiver<gpu::ComputeOutput>,
  wayland_tx: wayland::CommandTx,
  led_tx: led::CommandTx,
  frame_id: u64,
  frame_stage_tx: FrameStageTx,
  idle: bool,
  manual_mode: bool,
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
            tracing::error!("error setting idle led strip state: {}", err);
          }
        } else if manual_mode {
          tracing::debug!("ignoring shader compute result while in manual mode");
        } else {
          match led_tx.send(LedCommand::SetStripState(
            color
              .as_slice()
              .array_chunks::<3>()
              .copied()
              .collect::<Vec<[u8; 3]>>()
              .try_into()
              .expect("Slice length mismatch"),
          )) {
            Ok(()) => {
              if frame_stage_tx.send(FrameStage::LedDispatched { frame_id }).is_err() {
                tracing::error!(frame_id, "failed to record LED dispatch stage");
              }
            }
            Err(err) => {
              tracing::error!("failed to send compute result to LED manager: {}", err);
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

      // if let Err(err) = led_tx.send(led::Command::SetStripState(result)) {
      //   tracing::error!("failed to send compute result to LED manager: {}", err);
      // }

      if let Err(err) = wayland_tx.send(wayland::Command::ComputeDone) {
        tracing::error!("failed to send compute done command: {}", err);
      }
    }
    Err(err) => {
      tracing::error!("compute result channel closed: {}", err);
      let _ = frame_stage_tx.send(FrameStage::Aborted {
        frame_id,
        reason: format!("compute channel closed: {err}"),
      });
    }
  };
}
