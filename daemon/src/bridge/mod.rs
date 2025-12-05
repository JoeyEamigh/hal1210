use std::{
  net::SocketAddr,
  sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
  },
};

use daemoncomm::{CecEvent, CecStatus, LedCommand, MessageToServer, MessageToServerData};
use tokio::task::JoinHandle;

use crate::{
  cec,
  client::{
    self,
    session::{ClientSessions, DisconnectOutcome},
  },
  gpu, led, monitoring,
  wayland::{self, idle::IdleEvent},
};
use uuid::Uuid;

mod cec_control;
mod frame;
mod idle;
mod kinect;
mod messaging;

use self::{
  cec_control::CecController,
  frame::{FramePipeline, FrameStageRx, FrameStageTx},
  idle::{IdleState, IdleTimeoutRx},
  kinect::KinectHandler,
  messaging::Messenger,
};

pub struct Handler {
  compute: gpu::Compute,

  wayland_tx: wayland::CommandTx,
  wayland_rx: wayland::EventRx,
  led_rx: led::EventRx,
  client_man_rx: client::ClientReqRx,
  kinect_rx: crate::kinect::EventRx,
  cec: CecController,
  cec_rx: cec::EventRx,

  idle_state: IdleState,
  idle_timeout_rx: IdleTimeoutRx,
  effective_idle_cached: bool,

  clients: ClientSessions,
  manual_flag: Arc<AtomicBool>,
  manual_started_while_idle: bool,
  tv_powered: Option<bool>,
  tv_forced_dark: bool,

  frame_pipeline: FramePipeline,
  frame_stage_tx: FrameStageTx,
  frame_stage_rx: FrameStageRx,

  kinect: KinectHandler,
  messenger: Messenger,

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
    kinect_tx: crate::kinect::CommandTx,
    kinect_rx: crate::kinect::EventRx,
    cec_tx: cec::CommandTx,
    cec_rx: cec::EventRx,

    loop_signal: calloop::LoopSignal,
    cancel_token: tokio_util::sync::CancellationToken,
  ) -> Self {
    let (frame_stage_tx, frame_stage_rx) = frame::channel();
    let (idle_timeout_tx, idle_timeout_rx) = idle::channel();

    let cec = CecController::new(cec_tx);

    let handler = Self {
      compute,

      wayland_tx,
      wayland_rx,
      led_rx,
      client_man_rx,
      kinect_rx,
      cec,
      cec_rx,

      idle_state: IdleState::new(idle_timeout_tx, cancel_token.clone()),
      idle_timeout_rx,
      effective_idle_cached: false,

      clients: ClientSessions::new(),
      manual_flag: Arc::new(AtomicBool::new(false)),
      manual_started_while_idle: false,
      tv_powered: None,
      tv_forced_dark: false,

      frame_pipeline: FramePipeline::new(),
      frame_stage_tx,
      frame_stage_rx,

      kinect: KinectHandler::new(kinect_tx),
      messenger: Messenger::new(led_tx, client_man_tx),

      loop_signal,
      cancel_token,
    };

    handler.cec.request_status();
    handler
  }

  pub fn spawn(mut self) -> JoinHandle<()> {
    tokio::spawn(async move { self.run().await })
  }

  async fn run(&mut self) {
    loop {
      tokio::select! {
        biased;

        Some(stage) = self.frame_stage_rx.recv() => {
          self.frame_pipeline.handle_stage(stage);
        }
        Some(event) = self.led_rx.recv() => {
          tracing::trace!("received event from LED manager: {:?}", event);
          self.handle_led_event(event);
        }
        Some(event) = self.kinect_rx.recv() => {
          tracing::trace!("received event from Kinect manager: {:?}", event);
          self.kinect.handle_event(event, self.clients.iter(), &self.messenger);
        }
        Some(event) = self.cec_rx.recv() => {
          tracing::trace!("received event from CEC manager: {:?}", event);
          self.handle_cec_event(event);
        }
        Some(req) = self.client_man_rx.recv() => {
          tracing::trace!("received command from client: {:?}", req);
          self.handle_client_command(req);
        }
        Some(event) = self.wayland_rx.recv() => {
          tracing::trace!("received event from wayland: {:?}", event);
          self.handle_wayland_event(event);
        }
        Some(()) = self.idle_timeout_rx.recv() => {
          tracing::trace!("idle inhibit timer fired");
          self.idle_state.on_timeout();
          self.sync_effective_idle();
        }
        _ = monitoring::wait_for_signal() => {
          tracing::info!("shutdown signal received, stopping...");
          break;
        }
      }
    }

    self.idle_state.cancel_timer();
    self.cancel_token.cancel();
    self.loop_signal.stop();
    self.compute.wait_for_idle();

    tracing::debug!("scheduled event loops for shutdown");
  }

  fn handle_cec_event(&mut self, event: CecEvent) {
    if let Some(status) = self.cec.handle_event(event, self.clients.iter(), &self.messenger) {
      self.on_cec_status(status);
    }
  }

  fn on_cec_status(&mut self, status: CecStatus) {
    let previous = self.tv_powered.replace(status.powered_on);
    if previous == Some(status.powered_on) {
      return;
    }

    if status.powered_on {
      self.on_tv_power_restored();
    } else {
      self.on_tv_power_lost();
    }
  }

  fn on_tv_power_lost(&mut self) {
    if self.tv_forced_dark {
      return;
    }

    tracing::info!("TV powered off detected; fading LEDs");
    self.tv_forced_dark = true;
    self
      .messenger
      .send_led_command(LedCommand::FadeOut { duration_ms: None });
  }

  fn on_tv_power_restored(&mut self) {
    if !self.tv_forced_dark {
      return;
    }

    tracing::info!("TV powered on detected; resuming LEDs");
    self.tv_forced_dark = false;

    if self.clients.manual_enabled() || self.idle_state.effective_idle() {
      return;
    }

    if !self.idle_state.effective_idle() {
      self.start_fade_in_from_last_frame();
    }
    self.resume_frame_pipeline();
  }

  fn handle_wayland_event(&mut self, event: wayland::Event) {
    match event {
      wayland::Event::Idle(idle_event) => match idle_event {
        IdleEvent::Idle => {
          self.idle_state.seat_idle = true;
          self.sync_effective_idle();
        }
        IdleEvent::Active => {
          self.idle_state.seat_idle = false;
          self.sync_effective_idle();
        }
      },
      wayland::Event::DmabufCreated(dmabuf) => {
        tracing::debug!("setting DMA-BUF in compute module");
        if let Err(err) = self.compute.set_screen_dmabuf(dmabuf) {
          tracing::error!("failed to set DMA-BUF: {err}");
        }
      }
      wayland::Event::FrameReady => {
        self.dispatch_frame();
      }
    }
  }

  fn dispatch_frame(&mut self) {
    let idle = self.idle_state.effective_idle();

    if idle {
      tracing::debug!("skipping frame processing because seat is idle");
      return;
    }

    if self.clients.manual_enabled() {
      tracing::debug!("skipping frame processing because manual mode is enabled");
      return;
    }

    if self.tv_forced_dark {
      tracing::debug!("skipping frame processing because TV reported off");
      return;
    }

    tracing::debug!("wayland frame is ready for processing");
    let frame_id = self.frame_pipeline.next_frame_id();

    match self.compute.dispatch() {
      Ok(compute_rx) => {
        self.frame_pipeline.record_start(frame_id);

        let wayland_tx = self.wayland_tx.clone();
        let led_tx = self.messenger.led_tx();
        let frame_stage_tx = self.frame_stage_tx.clone();
        let manual_flag = self.manual_flag.clone();

        tokio::spawn(async move {
          frame::await_compute_dispatch(
            compute_rx,
            wayland_tx,
            led_tx,
            frame_id,
            frame_stage_tx,
            idle,
            manual_flag,
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
        tracing::error!(frame_id, "failed to dispatch compute shader: {err}");
      }
    }
  }

  fn handle_led_event(&mut self, event: led::Event) {
    match event {
      led::Event::Done => {
        if self.clients.manual_enabled() {
          tracing::trace!("LED manager completed manual command");
        } else {
          tracing::debug!("LED manager reported done processing");
          self.frame_pipeline.log_completion();
        }
      }
      led::Event::Error(err) => {
        tracing::error!("LED manager reported error: {err}");
      }
    };
  }

  fn handle_client_command(&mut self, req: client::ClientReq) {
    match req.data {
      client::ClientReqData::Message(msg) => {
        self.register_client(req.addr);
        self.handle_client_message(req.addr, msg);
      }
      client::ClientReqData::Disconnected => {
        self.handle_client_disconnect(req.addr);
      }
    }
  }

  fn handle_client_message(&mut self, addr: SocketAddr, msg: MessageToServer) {
    match msg.data {
      MessageToServerData::Led(command) => {
        self.handle_client_led(addr, msg.id, command);
      }
      MessageToServerData::SetManualMode { enabled } => {
        self.handle_manual_mode_request(addr, msg.id, enabled);
      }
      MessageToServerData::GetManualMode => {
        self
          .messenger
          .send_manual_state(addr, msg.id, self.clients.manual_enabled());
        self.messenger.ack(addr, msg.id);
      }
      MessageToServerData::SetIdleInhibit { enabled, timeout_ms } => {
        self.handle_idle_inhibit_request(addr, msg.id, enabled, timeout_ms);
      }
      MessageToServerData::GetIdleInhibit => {
        self.messenger.send_idle_inhibit_state(
          addr,
          msg.id,
          self.idle_state.idle_inhibit,
          self.idle_state.remaining_inhibit_ms(),
        );
        self.messenger.ack(addr, msg.id);
      }
      MessageToServerData::Kinect(command) => {
        self.kinect.handle_command(addr, msg.id, command, &self.messenger);
      }
      MessageToServerData::Cec(command) => {
        self.cec.handle_client_command(addr, msg.id, command, &self.messenger);
      }
    }
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
      self.messenger.nack(addr, id, "manual_mode_disabled");
      return;
    }

    self.messenger.ack(addr, id);
    tracing::debug!(?command, "forwarding manual LED command to manager");
    self.messenger.send_led_command(command);
  }

  fn handle_manual_mode_request(&mut self, addr: SocketAddr, id: Uuid, enabled: bool) {
    let transition = self.clients.set_manual_enabled(enabled);

    if transition.changed() {
      tracing::info!(addr = %addr, enabled, "manual mode updated via client request");
      self.on_manual_state_changed(transition.enabled());
    } else {
      tracing::debug!(addr = %addr, enabled, "manual mode already in requested state");
    }

    self.messenger.send_manual_state(addr, id, transition.enabled());
    self.messenger.ack(addr, id);
  }

  fn handle_idle_inhibit_request(&mut self, addr: SocketAddr, id: Uuid, enabled: bool, timeout_ms: Option<u64>) {
    let changed = self.idle_state.apply_inhibit(enabled, timeout_ms);
    self.sync_effective_idle();

    if changed {
      tracing::info!(addr = %addr, enabled, timeout_ms, "idle inhibit updated via client request");
    } else {
      tracing::debug!(addr = %addr, enabled, timeout_ms, "idle inhibit already in requested state");
    }

    let remaining = self.idle_state.remaining_inhibit_ms();
    self
      .messenger
      .send_idle_inhibit_state(addr, id, self.idle_state.idle_inhibit, remaining);
    self.messenger.ack(addr, id);
  }

  fn sync_effective_idle(&mut self) {
    let was_idle = self.effective_idle_cached;
    let is_idle = self.idle_state.effective_idle();

    if is_idle && !was_idle {
      self.on_enter_idle();
    } else if !is_idle && was_idle {
      self.on_exit_idle();
    } else if self.idle_state.seat_idle && self.idle_state.idle_inhibit {
      tracing::debug!("idle inhibit active; suppressing idle fade out");
    }

    self.effective_idle_cached = is_idle;
  }

  fn on_enter_idle(&mut self) {
    tracing::debug!("effective idle detected; pausing frame pipeline");
    self.cec.power_off();
    if self.clients.manual_enabled() {
      tracing::trace!("manual mode active; suppressing idle fade out");
    } else {
      tracing::debug!("turning off leds since seat is idle");
      self
        .messenger
        .send_led_command(LedCommand::FadeOut { duration_ms: None });
    }
  }

  fn on_exit_idle(&mut self) {
    if self.cec.powered_on() != Some(true) {
      self.cec.power_on();
    }

    if self.clients.manual_enabled() {
      self.manual_started_while_idle = false;
    } else {
      self.start_fade_in_from_last_frame();
    }

    tracing::debug!("effective idle cleared; restarting frame loop");
    self.resume_frame_pipeline();
  }

  // --- Manual Mode ---

  fn on_manual_state_changed(&mut self, enabled: bool) {
    self.manual_flag.store(enabled, Ordering::Release);
    let idle = self.idle_state.effective_idle();

    if enabled {
      tracing::debug!("manual mode enabled; pausing automatic frame dispatch");
      self.manual_started_while_idle = idle;
      return;
    }

    if idle {
      if self.manual_started_while_idle {
        tracing::debug!("manual mode disabled while idle; fading out without restoring frame");
        self
          .messenger
          .send_led_command(LedCommand::FadeOut { duration_ms: None });
      } else {
        tracing::debug!("manual mode disabled while idle; restoring last frame before fade out");
        self
          .messenger
          .send_led_command(LedCommand::SetStripState(self.frame_pipeline.last_auto_frame));
        self
          .messenger
          .send_led_command(LedCommand::FadeOut { duration_ms: None });
      }
    } else {
      tracing::debug!("manual mode disabled; resuming automatic frame dispatch");
      self
        .messenger
        .send_led_command(LedCommand::SetStripState(self.frame_pipeline.last_auto_frame));
      self.resume_frame_pipeline();
    }

    self.manual_started_while_idle = false;
  }

  fn start_fade_in_from_last_frame(&mut self) {
    self.messenger.send_led_command(LedCommand::FadeIn {
      state: self.frame_pipeline.last_auto_frame,
      duration_ms: None,
    });
  }

  fn resume_frame_pipeline(&self) {
    if let Err(err) = self.wayland_tx.send(wayland::Command::ComputeDone) {
      tracing::error!("failed to resume frame pipeline: {err}");
    }
  }
}
