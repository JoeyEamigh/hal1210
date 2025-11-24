use std::{
  collections::{HashMap, VecDeque},
  net::SocketAddr,
  sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
  },
  time::Instant,
};

use daemoncomm::{
  LedCommand, LedStripState, MessageToClient, MessageToClientData, MessageToServer, MessageToServerData,
};
use ledcomm::BYTES_PER_LED;
use tokio::{sync::oneshot, task::JoinHandle};

use crate::{
  client::{
    self,
    session::{ClientSessions, DisconnectOutcome},
  },
  gpu, led, monitoring,
  wayland::{self, idle::IdleEvent},
};
use uuid::Uuid;

type FrameStageTx = tokio::sync::mpsc::UnboundedSender<FrameStage>;
type FrameStageRx = tokio::sync::mpsc::UnboundedReceiver<FrameStage>;

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum FrameStage {
  LedDispatched { frame_id: u64, state: LedStripState },
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
  client_man_tx: client::ServerResTx,
  client_man_rx: client::ClientReqRx,

  // state
  idle: bool,
  clients: ClientSessions,
  manual_flag: Arc<AtomicBool>,
  pending_fade_in: bool,
  manual_started_while_idle: bool,

  // frame lifetime
  frame_stage_tx: FrameStageTx,
  frame_stage_rx: FrameStageRx,
  last_auto_frame: LedStripState,
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
    client_man_tx: client::ServerResTx,
    client_man_rx: client::ClientReqRx,

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
      client_man_tx,
      client_man_rx,

      idle: false,
      clients: ClientSessions::new(),
      manual_flag: Arc::new(AtomicBool::new(false)),
      pending_fade_in: false,
      manual_started_while_idle: false,

      frame_stage_tx,
      frame_stage_rx,
      last_auto_frame: ledcomm::zero_state_frame(),
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

          if self.clients.manual_enabled() {
            tracing::trace!("manual mode active; suppressing idle fade out");
          } else {
            tracing::debug!("turning off leds since seat is idle");
            self.send_led_command(LedCommand::FadeOut);
          }
        }
        IdleEvent::Active => {
          self.idle = false;
          if self.clients.manual_enabled() {
            self.manual_started_while_idle = false;
          } else {
            self.pending_fade_in = true;
          }
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

        if self.clients.manual_enabled() {
          tracing::debug!("skipping frame processing because manual mode is enabled");
          return;
        }

        tracing::debug!("wayland frame is ready for processing");
        let pending_fade = self.pending_fade_in;
        let frame_id = self.frame_counter;
        self.frame_counter = self.frame_counter.wrapping_add(1);
        let started_at = Instant::now();

        match self.compute.dispatch() {
          Ok(compute_rx) => {
            if pending_fade {
              self.pending_fade_in = false;
            }
            self.frame_starts.insert(frame_id, started_at);

            let wayland_tx = self.wayland_tx.clone();
            let led_tx = self.led_tx.clone();
            let frame_stage_tx = self.frame_stage_tx.clone();
            let idle = self.idle;
            let manual_flag = self.manual_flag.clone();

            tokio::spawn(async move {
              await_compute_dispatch(
                compute_rx,
                wayland_tx,
                led_tx,
                frame_id,
                frame_stage_tx,
                idle,
                manual_flag,
                pending_fade,
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
        if self.clients.manual_enabled() {
          tracing::trace!("LED manager completed manual command");
        } else {
          tracing::debug!("LED manager reported done processing");
          self.log_frame_pipeline();
        }
      }
      led::Event::Error(err) => {
        tracing::error!("LED manager reported error: {}", err);
      }
    };
  }

  async fn handle_client_command(&mut self, req: client::ClientReq) {
    match req.data {
      client::ClientReqData::Message(msg) => {
        self.register_client(req.addr);
        self.handle_client_message(req.addr, msg).await;
      }
      client::ClientReqData::Disconnected => {
        self.handle_client_disconnect(req.addr);
      }
    }
  }

  async fn handle_client_message(&mut self, addr: SocketAddr, msg: MessageToServer) {
    match msg.data {
      MessageToServerData::Led(command) => {
        self.handle_client_led(addr, msg.id, command);
      }
      MessageToServerData::SetManualMode { enabled } => {
        self.handle_manual_mode_request(addr, msg.id, enabled);
      }
      MessageToServerData::GetManualMode => {
        self.send_manual_state(addr, msg.id, self.clients.manual_enabled());
        self.ack(addr, msg.id);
      }
    }
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
        Some(req) = self.client_man_rx.recv() => {
          tracing::trace!("received command from client: {:?}", req);
          self.handle_client_command(req).await;
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

  fn register_client(&mut self, addr: SocketAddr) {
    if self.clients.register(addr) {
      tracing::info!(addr = %addr, total_clients = self.clients.client_count(), "client registered");
    }
  }

  fn handle_client_disconnect(&mut self, addr: SocketAddr) {
    match self.clients.remove(addr) {
      DisconnectOutcome::Unknown => {
        tracing::debug!(addr = %addr, "received disconnect for unknown client");
      }
      DisconnectOutcome::Removed => {
        tracing::info!(addr = %addr, total_clients = self.clients.client_count(), "client disconnected");
      }
      DisconnectOutcome::ManualDisabled => {
        tracing::info!(addr = %addr, "last client disconnected; disabling manual mode");
        self.on_manual_state_changed(false);
      }
    }
  }

  fn handle_client_led(&mut self, addr: SocketAddr, id: Uuid, command: LedCommand) {
    if !self.clients.manual_enabled() {
      self.nack(addr, id, "manual_mode_disabled");
      return;
    }

    self.ack(addr, id);
    self.process_manual_led_command(command);
  }

  fn handle_manual_mode_request(&mut self, addr: SocketAddr, id: Uuid, enabled: bool) {
    let transition = self.clients.set_manual_enabled(enabled);

    if transition.changed() {
      tracing::info!(addr = %addr, enabled, "manual mode updated via client request");
      self.on_manual_state_changed(transition.enabled());
    } else {
      tracing::debug!(addr = %addr, enabled, "manual mode already in requested state");
    }

    self.send_manual_state(addr, id, transition.enabled());
    self.ack(addr, id);
  }

  fn on_manual_state_changed(&mut self, enabled: bool) {
    self.manual_flag.store(enabled, Ordering::Release);
    if enabled {
      tracing::debug!("manual mode enabled; pausing automatic frame dispatch");
      self.pending_fade_in = false;
      self.manual_started_while_idle = self.idle;
      return;
    }

    if self.idle {
      self.pending_fade_in = true;
      if self.manual_started_while_idle {
        tracing::debug!("manual mode disabled while idle; fading out without restoring frame");
        self.send_led_command(LedCommand::FadeOut);
      } else {
        tracing::debug!("manual mode disabled while idle; restoring last frame before fade out");
        self.send_led_command(LedCommand::SetStripState(self.last_auto_frame));
        self.send_led_command(LedCommand::FadeOut);
      }
    } else {
      tracing::debug!("manual mode disabled; resuming automatic frame dispatch");
      self.pending_fade_in = false;
      self.send_led_command(LedCommand::SetStripState(self.last_auto_frame));
      self.resume_frame_pipeline();
    }

    self.manual_started_while_idle = false;
  }

  fn resume_frame_pipeline(&self) {
    if let Err(err) = self.wayland_tx.send(wayland::Command::ComputeDone) {
      tracing::error!("failed to resume frame pipeline: {}", err);
    }
  }

  fn send_manual_state(&self, addr: SocketAddr, id: Uuid, enabled: bool) {
    self.send_reply(addr, id, MessageToClientData::ManualMode { enabled });
  }

  fn ack(&self, addr: SocketAddr, id: Uuid) {
    self.send_reply(addr, id, MessageToClientData::Ack);
  }

  fn nack<S: Into<String>>(&self, addr: SocketAddr, id: Uuid, reason: S) {
    self.send_reply(addr, id, MessageToClientData::Nack { reason: reason.into() });
  }

  fn send_led_command(&self, command: LedCommand) {
    if let Err(err) = self.led_tx.send(command) {
      tracing::error!("failed to send LED command: {}", err);
    }
  }

  fn send_reply(&self, addr: SocketAddr, id: Uuid, data: MessageToClientData) {
    let message = MessageToClient { id, data };
    if let Err(err) = self.client_man_tx.send(client::ServerRes::new(addr, message)) {
      tracing::error!(addr = %addr, "failed to send response to client: {}", err);
    }
  }

  fn process_manual_led_command(&self, command: LedCommand) {
    tracing::debug!(?command, "forwarding manual LED command to manager");
    self.send_led_command(command);
  }

  fn handle_frame_stage(&mut self, stage: FrameStage) {
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
        tracing::trace!("LED manager reported completion with no frame timing in flight");
      }
    }
  }
}

#[allow(clippy::too_many_arguments)]
async fn await_compute_dispatch(
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
            tracing::error!("error setting idle led strip state: {}", err);
          }
        } else if manual_flag.load(Ordering::Acquire) {
          tracing::debug!("ignoring shader compute result while in manual mode");
        } else {
          let strip_state = std::array::from_fn(|i| {
            let start = i * BYTES_PER_LED;
            color.as_slice()[start..start + BYTES_PER_LED].try_into().unwrap()
          });

          let command = if fade_in {
            LedCommand::FadeIn(strip_state)
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
