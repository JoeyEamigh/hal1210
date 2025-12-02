use std::{
  collections::{HashMap, VecDeque},
  net::SocketAddr,
  sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
  },
  time::{Duration, Instant},
};

use daemoncomm::{
  LedCommand, LedStripState, MessageToClient, MessageToClientData, MessageToServer, MessageToServerData,
};
use ledcomm::BYTES_PER_LED;
use tokio::{sync::oneshot, task::JoinHandle, time};

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
type IdleTimeoutTx = tokio::sync::mpsc::UnboundedSender<()>;
type IdleTimeoutRx = tokio::sync::mpsc::UnboundedReceiver<()>;

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
  idle_timeout_tx: IdleTimeoutTx,
  idle_timeout_rx: IdleTimeoutRx,

  // state
  idle: bool,
  seat_idle: bool,
  idle_inhibit: bool,
  idle_inhibit_requested_timeout: Option<u64>,
  idle_inhibit_deadline: Option<Instant>,
  idle_inhibit_task: Option<JoinHandle<()>>,
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
    let (idle_timeout_tx, idle_timeout_rx) = tokio::sync::mpsc::unbounded_channel();

    Self {
      compute,

      wayland_tx,
      wayland_rx,
      led_tx,
      led_rx,
      client_man_tx,
      client_man_rx,
      idle_timeout_tx,
      idle_timeout_rx,

      idle: false,
      seat_idle: false,
      idle_inhibit: false,
      idle_inhibit_requested_timeout: None,
      idle_inhibit_deadline: None,
      idle_inhibit_task: None,
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
          self.seat_idle = true;
          self.update_effective_idle_state();
        }
        IdleEvent::Active => {
          self.seat_idle = false;
          self.update_effective_idle_state();
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
      MessageToServerData::SetIdleInhibit { enabled, timeout_ms } => {
        self.handle_idle_inhibit_request(addr, msg.id, enabled, timeout_ms);
      }
      MessageToServerData::GetIdleInhibit => {
        self.send_idle_inhibit_state(addr, msg.id, self.idle_inhibit, self.remaining_idle_inhibit_ms());
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
        Some(()) = self.idle_timeout_rx.recv() => {
          tracing::trace!("idle inhibit timer fired");
          self.on_idle_inhibit_timeout();
        }
        _ = monitoring::wait_for_signal() => {
          tracing::info!("shutdown signal received, stopping...");
          break;
        }
      }
    }

    self.cancel_idle_inhibit_timer();
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

  fn handle_idle_inhibit_request(&mut self, addr: SocketAddr, id: Uuid, enabled: bool, timeout_ms: Option<u64>) {
    let changed = self.apply_idle_inhibit_state(enabled, timeout_ms);

    if changed {
      tracing::info!(addr = %addr, enabled, timeout_ms, "idle inhibit updated via client request");
    } else {
      tracing::debug!(addr = %addr, enabled, timeout_ms, "idle inhibit already in requested state");
    }

    let remaining = self.remaining_idle_inhibit_ms();
    self.send_idle_inhibit_state(addr, id, self.idle_inhibit, remaining);
    self.ack(addr, id);
  }

  fn apply_idle_inhibit_state(&mut self, enabled: bool, timeout_ms: Option<u64>) -> bool {
    let prev_enabled = self.idle_inhibit;
    let prev_timeout = self.idle_inhibit_requested_timeout;

    if enabled {
      self.idle_inhibit_requested_timeout = timeout_ms;
      if let Some(ms) = timeout_ms {
        self.start_idle_inhibit_timer(ms);
      } else {
        self.cancel_idle_inhibit_timer();
        self.idle_inhibit_deadline = None;
      }
    } else {
      self.idle_inhibit_requested_timeout = None;
      self.idle_inhibit_deadline = None;
      self.cancel_idle_inhibit_timer();
    }

    self.idle_inhibit = enabled;
    self.update_effective_idle_state();

    prev_enabled != enabled || (enabled && prev_timeout != timeout_ms)
  }

  fn remaining_idle_inhibit_ms(&self) -> Option<u64> {
    if !self.idle_inhibit {
      return None;
    }

    self
      .idle_inhibit_deadline
      .map(|deadline| deadline.saturating_duration_since(Instant::now()).as_millis() as u64)
  }

  fn start_idle_inhibit_timer(&mut self, duration_ms: u64) {
    self.cancel_idle_inhibit_timer();

    if duration_ms == 0 {
      self.idle_inhibit_deadline = Some(Instant::now());
      if let Err(err) = self.idle_timeout_tx.send(()) {
        tracing::error!("failed to dispatch immediate idle inhibit timeout: {}", err);
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

  fn cancel_idle_inhibit_timer(&mut self) {
    if let Some(handle) = self.idle_inhibit_task.take() {
      handle.abort();
    }
  }

  fn on_idle_inhibit_timeout(&mut self) {
    self.idle_inhibit_task = None;
    if !self.idle_inhibit {
      return;
    }

    tracing::info!("idle inhibit timeout expired; disabling idle inhibit");
    self.idle_inhibit = false;
    self.idle_inhibit_requested_timeout = None;
    self.idle_inhibit_deadline = None;
    self.update_effective_idle_state();
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
        self.send_led_command(LedCommand::FadeOut { duration_ms: None });
      } else {
        tracing::debug!("manual mode disabled while idle; restoring last frame before fade out");
        self.send_led_command(LedCommand::SetStripState(self.last_auto_frame));
        self.send_led_command(LedCommand::FadeOut { duration_ms: None });
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

  fn send_idle_inhibit_state(&self, addr: SocketAddr, id: Uuid, enabled: bool, timeout_ms: Option<u64>) {
    self.send_reply(addr, id, MessageToClientData::IdleInhibit { enabled, timeout_ms });
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

  fn update_effective_idle_state(&mut self) {
    let effective_idle = self.seat_idle && !self.idle_inhibit;

    if effective_idle {
      if !self.idle {
        self.idle = true;
        self.on_enter_idle();
      }
      return;
    }

    if self.seat_idle && self.idle_inhibit {
      tracing::debug!("idle inhibit active; suppressing idle fade out");
    }

    if self.idle {
      self.idle = false;
      self.on_exit_idle();
    }
  }

  fn on_enter_idle(&mut self) {
    tracing::debug!("effective idle detected; pausing frame pipeline");
    if self.clients.manual_enabled() {
      tracing::trace!("manual mode active; suppressing idle fade out");
    } else {
      tracing::debug!("turning off leds since seat is idle");
      self.send_led_command(LedCommand::FadeOut { duration_ms: None });
    }
  }

  fn on_exit_idle(&mut self) {
    if self.clients.manual_enabled() {
      self.manual_started_while_idle = false;
    } else {
      self.pending_fade_in = true;
    }

    tracing::debug!("effective idle cleared; restarting frame loop");
    if let Err(err) = self.wayland_tx.send(wayland::Command::ComputeDone) {
      tracing::error!("failed to restart frame loop: {}", err);
    }
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
