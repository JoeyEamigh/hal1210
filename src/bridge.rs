use tokio::task::JoinHandle;

use crate::{gpu, monitoring, wayland};

pub struct Handler {
  compute: gpu::Compute,

  cmd_tx: wayland::CommandTx,
  event_rx: wayland::EventRx,

  loop_signal: calloop::LoopSignal,
}

impl Handler {
  pub fn new(
    compute: gpu::Compute,
    cmd_tx: wayland::CommandTx,
    event_rx: wayland::EventRx,
    loop_signal: calloop::LoopSignal,
  ) -> Self {
    Self {
      compute,

      cmd_tx,
      event_rx,

      loop_signal,
    }
  }

  async fn handle_event(&mut self, event: wayland::Event) {
    match event {
      wayland::Event::DmabufCreated(fd) => {
        tracing::info!("setting DMA-BUF in compute module");
        if let Err(err) = self.compute.set_dmabuf(fd) {
          tracing::error!("failed to set DMA-BUF: {}", err);
        }
      }
      wayland::Event::FrameReady => {
        tracing::debug!("wayland frame is ready for processing");
      }
    }
  }

  async fn run(&mut self) {
    loop {
      tokio::select! {
        Some(event) = self.event_rx.recv() => {
          tracing::trace!("received event from wayland: {:?}", event);
          self.handle_event(event).await;
        }
        _ = monitoring::wait_for_signal() => {
          tracing::info!("shutdown signal received, stopping...");
          break;
        }
      }
    }

    tracing::debug!("shutting down wayland event loop");
    self.loop_signal.stop();
  }

  pub fn spawn(mut self) -> JoinHandle<()> {
    tokio::spawn(async move { self.run().await })
  }
}
