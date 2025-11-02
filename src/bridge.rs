use tokio::{sync::oneshot, task::JoinHandle};

use crate::{gpu, led, monitoring, wayland};

pub struct Handler {
  compute: gpu::Compute,

  wayland_tx: wayland::CommandTx,
  wayland_rx: wayland::EventRx,
  led_tx: led::CommandTx,
  led_rx: led::EventRx,

  loop_signal: calloop::LoopSignal,
  cancel_token: tokio_util::sync::CancellationToken,
}

impl Handler {
  pub fn new(
    compute: gpu::Compute,

    wayland_tx: wayland::CommandTx,
    wayland_rx: wayland::EventRx,
    led_tx: led::CommandTx,
    led_rx: led::EventRx,

    loop_signal: calloop::LoopSignal,
    cancel_token: tokio_util::sync::CancellationToken,
  ) -> Self {
    Self {
      compute,

      wayland_tx,
      wayland_rx,
      led_tx,
      led_rx,

      loop_signal,
      cancel_token,
    }
  }

  async fn handle_wayland_event(&mut self, event: wayland::Event) {
    match event {
      wayland::Event::DmabufCreated {
        fd,
        width,
        height,
        stride,
        format,
        modifier,
      } => {
        tracing::info!("setting DMA-BUF in compute module");
        let dmabuf = gpu::Dmabuf {
          fd,
          width,
          height,
          stride,
          format,
          modifier,
        };
        if let Err(err) = self.compute.set_dmabuf(dmabuf) {
          tracing::error!("failed to set DMA-BUF: {}", err);
        }
      }
      wayland::Event::FrameReady => {
        tracing::debug!("wayland frame is ready for processing");
        match self.compute.dispatch() {
          Ok(compute_rx) => {
            let wayland_tx = self.wayland_tx.clone();
            let led_tx = self.led_tx.clone();

            tokio::spawn(async move { await_compute_dispatch(compute_rx, wayland_tx, led_tx).await });
          }
          Err(gpu::ComputeError::DispatchInFlight) => {
            tracing::warn!("compute dispatch already in flight, skipping frame");
          }
          Err(gpu::ComputeError::NoDmabuf) => {
            tracing::error!("compute module has no DMA-BUF to process yet");
          }
          Err(gpu::ComputeError::EmptyDmabuf) => {
            tracing::error!("DMA-BUF had zero size, skipping frame");
          }
          Err(err) => {
            tracing::error!("failed to dispatch compute shader: {}", err);
          }
        }
      }
    }
  }

  async fn handle_led_event(&mut self, event: led::Event) {
    match event {
      led::Event::Done => {
        tracing::debug!("LED manager reported done processing");
      }
      led::Event::Error(err) => {
        tracing::error!("LED manager reported error: {}", err);
      }
    };
  }

  async fn run(&mut self) {
    loop {
      tokio::select! {
        Some(event) = self.wayland_rx.recv() => {
          tracing::trace!("received event from wayland: {:?}", event);
          self.handle_wayland_event(event).await;
        }
        Some(event) = self.led_rx.recv() => {
          tracing::trace!("received event from LED manager: {:?}", event);
          self.handle_led_event(event).await;
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
}

async fn await_compute_dispatch(
  compute_rx: oneshot::Receiver<gpu::ComputeOutput>,
  wayland_tx: wayland::CommandTx,
  led_tx: led::CommandTx,
) {
  match compute_rx.await {
    Ok(result) => {
      tracing::debug!("compute shader finished successfully");

      if let Some([r, g, b]) = result.average_rgb_u8() {
        tracing::trace!(r, g, b, "computed average color");
      } else {
        tracing::warn!("compute result contained no pixels");
      }

      if let Err(err) = led_tx.send(led::Command::HandleColors(result)) {
        tracing::error!("failed to send compute result to LED manager: {}", err);
      }
      if let Err(err) = wayland_tx.send(wayland::Command::ComputeDone) {
        tracing::error!("failed to send compute done command: {}", err);
      }
    }
    Err(err) => {
      tracing::error!("compute result channel closed: {}", err);
    }
  };
}
