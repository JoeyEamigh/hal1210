use std::{cell::OnceCell, os::fd::AsFd};

use calloop::{LoopHandle, channel};
use calloop_wayland_source::WaylandSource;
use idle::IdleEvent;
use screencopy::{Dmabuf, DmabufPlane};
use wayland_client::{
  Connection, Dispatch, Proxy, QueueHandle, delegate_noop,
  protocol::{
    wl_buffer::WlBuffer,
    wl_output::WlOutput,
    wl_registry::{self, WlRegistry},
    wl_seat::WlSeat,
  },
};
use wayland_protocols::{
  ext::idle_notify::v1::client::ext_idle_notifier_v1::ExtIdleNotifierV1,
  wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::{self},
    zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
  },
};
use wayland_protocols_wlr::screencopy::v1::client::{
  zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

use crate::gpu;

pub mod idle;
pub mod screencopy;

#[cfg(test)]
mod test;

pub type CommandTx = calloop::channel::Sender<Command>;
pub type CommandRx = calloop::channel::Channel<Command>;
pub type EventTx = tokio::sync::mpsc::UnboundedSender<Event>;
pub type EventRx = tokio::sync::mpsc::UnboundedReceiver<Event>;

#[derive(Debug)]
pub enum Command {
  ComputeDone,
}

pub const MAX_DMABUF_PLANES: usize = 4;

#[cfg(not(test))]
const IDLE_NOTIFY_TIMEOUT_MS: u32 = 300_000; // 5 minutes
#[cfg(test)]
const IDLE_NOTIFY_TIMEOUT_MS: u32 = 1_000; // 1 second

#[derive(Debug)]
pub enum Event {
  DmabufCreated(Dmabuf),
  FrameReady,
  Idle(IdleEvent),
}

pub struct Wayland {
  tx: EventTx,
  qh: QueueHandle<Wayland>,

  // screencopy & dmabuf globals
  gbm_device: OnceCell<gbm::Device<gpu::drm::Device>>,
  seat: OnceCell<WlSeat>,
  output: OnceCell<WlOutput>,
  screencopy: OnceCell<ZwlrScreencopyManagerV1>,
  dmabuf: OnceCell<ZwpLinuxDmabufV1>,
  buffer: OnceCell<WlBuffer>,

  // idle globals
  idle: OnceCell<ExtIdleNotifierV1>,

  state: State,
}

/// Wayland connection and event loop
impl Wayland {
  pub fn init(tx: EventTx, rx: CommandRx, handle: LoopHandle<'_, Wayland>) -> Result<Self, WaylandError> {
    tracing::info!("connecting to wayland server");
    let conn = Connection::connect_to_env()?;

    let queue = conn.new_event_queue::<Self>();
    let qh = queue.handle();

    let display = conn.display();
    let _registry = display.get_registry(&qh, ());

    WaylandSource::new(conn, queue).insert(handle.clone())?;

    let callback_handle = handle.clone();
    handle.insert_source(rx, move |event, _, wayland: &mut Wayland| {
      let handle = callback_handle.clone();
      let channel::Event::Msg(event) = event else {
        // this seems to fire when task loop is gracefully shutting down
        // tracing::error!("command channel closed!! this should never happen");
        return;
      };

      if let Err(err) = wayland.handle_command(event, handle) {
        tracing::error!("failed to handle wayland command: {}", err);
      }
    })?;

    Ok(Self {
      tx,
      qh,

      // screencopy & dmabuf globals
      gbm_device: Default::default(),
      seat: Default::default(),
      output: Default::default(),
      screencopy: Default::default(),
      dmabuf: Default::default(),
      buffer: Default::default(),

      // idle globals
      idle: Default::default(),

      state: Default::default(),
    })
  }

  fn handle_command(&mut self, cmd: Command, _handle: LoopHandle<'_, Wayland>) -> Result<(), WaylandError> {
    tracing::trace!("handling wayland command: {:?}", cmd);

    match cmd {
      Command::ComputeDone => {
        tracing::debug!("shader compute done; requesting next frame");
        self.request_screencopy_frame();
      }
    };

    Ok(())
  }
}

// - Dispatch implementations & state

#[derive(Default)]
struct State {
  frame: Option<ZwlrScreencopyFrameV1>,
  buffer_created: bool,
  frame_ready_for_copy: bool,
}

delegate_noop!(Wayland: ignore WlSeat);
delegate_noop!(Wayland: ignore WlOutput);
delegate_noop!(Wayland: ignore WlBuffer);

impl Dispatch<WlRegistry, ()> for Wayland {
  #[tracing::instrument(level = "trace", target = "registry", skip_all)]
  fn event(
    data: &mut Self,
    registry: &WlRegistry,
    event: wl_registry::Event,
    _: &(),
    _: &Connection,
    qh: &QueueHandle<Wayland>,
  ) {
    if let wl_registry::Event::Global {
      name,
      interface,
      version,
    } = event
    {
      tracing::trace!("[{name}] {interface} (v{version})");

      match &interface[..] {
        "wl_seat" => {
          if data.seat.get().is_some_and(|s| s.is_alive()) {
            tracing::warn!("received duplicate wl_seat global, ignoring");
            return;
          };

          let seat = registry.bind::<WlSeat, _, _>(name, version, qh, ());
          tracing::debug!("wl_seat global bound");

          if let Err(err) = data.seat.set(seat) {
            tracing::error!("failed to store wl_seat global: {:?}", err);
          }
        }
        "wl_output" => {
          if data.output.get().is_some_and(|s| s.is_alive()) {
            tracing::warn!("received duplicate wl_output global, ignoring");
            return;
          };

          let output = registry.bind::<WlOutput, _, _>(name, version, qh, ());
          tracing::debug!("wl_output global bound");

          if let Some(manager) = data.screencopy.get() {
            tracing::debug!("requesting screencopy on output");
            let frame = manager.capture_output(0, &output, qh, ());
            data.state.frame.replace(frame);
          }

          if let Err(err) = data.output.set(output) {
            tracing::error!("failed to store wl_output global: {:?}", err);
          }
        }
        "zwp_linux_dmabuf_v1" => {
          if data.dmabuf.get().is_some_and(|s| s.is_alive()) {
            tracing::warn!("received duplicate zwp_linux_dmabuf_v1 global, ignoring");
            return;
          };

          let dmabuf = registry.bind::<ZwpLinuxDmabufV1, _, _>(name, version, qh, ());
          tracing::debug!("zwp_linux_dmabuf_v1 global bound");

          tracing::debug!("requesting dmabuf default feedback");
          dmabuf.get_default_feedback(qh, ());

          if let Err(err) = data.dmabuf.set(dmabuf) {
            tracing::error!("failed to store zwp_linux_dmabuf_v1 global: {:?}", err);
          }
        }
        "zwlr_screencopy_manager_v1" => {
          if data.screencopy.get().is_some_and(|s| s.is_alive()) {
            tracing::warn!("received duplicate wlr_screencopy_manager global, ignoring");
            return;
          };

          let manager = registry.bind::<ZwlrScreencopyManagerV1, _, _>(name, version, qh, ());
          tracing::debug!("zwlr_screencopy_manager_v1 global bound");

          if let Some(output) = data.output.get() {
            tracing::debug!("requesting screencopy on output");
            let frame = manager.capture_output(0, output, qh, ());
            data.state.frame.replace(frame);
          }

          if let Err(err) = data.screencopy.set(manager) {
            tracing::error!("failed to store wlr_screencopy_manager_v1 global: {:?}", err);
          }
        }
        "ext_idle_notifier_v1" => {
          if data.idle.get().is_some_and(|s| s.is_alive()) {
            tracing::warn!("received duplicate ext_idle_notifier_v1 global, ignoring");
            return;
          };

          let idle = registry.bind::<ExtIdleNotifierV1, _, _>(name, version, qh, ());
          tracing::debug!("ext_idle_notifier_v1 global bound");

          let Some(seat) = data.seat.get() else {
            tracing::error!("wl_seat is not available to request idle notification. this should not happen");
            return;
          };

          idle.get_idle_notification(IDLE_NOTIFY_TIMEOUT_MS, seat, qh, ());

          if let Err(err) = data.idle.set(idle) {
            tracing::error!("failed to store ext_idle_notifier_v1 global: {:?}", err);
          }
        }
        _ => {}
      }
    }
  }
}

impl Wayland {
  fn request_screencopy_frame(&mut self) {
    let Some(manager) = self.screencopy.get() else {
      tracing::error!("screencopy manager is not available to request screencopy frame");
      return;
    };

    let Some(output) = self.output.get() else {
      tracing::error!("wl_output is not available to request screencopy frame");
      return;
    };

    tracing::debug!("requesting screencopy frame on output");
    let frame = manager.capture_output(0, output, &self.qh, ());
    self.state.frame.replace(frame);
  }

  fn screencopy_frame(&mut self) {
    let Some(frame) = &self.state.frame else {
      tracing::error!("no screencopy frame available to request buffer");
      return;
    };

    let Some(buffer) = self.buffer.get() else {
      // this happens on the first frame request, as the buffer is created asynchronously
      // tracing::error!("no dmabuf buffer available for screencopy frame");
      return;
    };

    tracing::debug!("requesting screencopy frame to dmabuf buffer");
    frame.copy_with_damage(buffer);
  }

  fn request_create_dmabuf_buffer(&mut self, format: u32, width: u32, height: u32, qh: &QueueHandle<Wayland>) {
    let dmabuf = match self.dmabuf.get() {
      Some(dmabuf) => dmabuf,
      None => {
        tracing::error!("received screencopy buffer event but dmabuf global is not available");
        return;
      }
    };

    tracing::trace!("creating gbm buffer object");
    let gbm_device = match self.gbm_device.get() {
      Some(dev) => dev,
      None => {
        tracing::error!("gbm device is not available, cannot create gbm buffer object");
        return;
      }
    };
    let bo = match gbm_device.create_buffer_object::<()>(
      width,
      height,
      format.try_into().expect("this should be infallible"),
      gbm::BufferObjectFlags::from_bits(0).expect("this should be infallible"),
    ) {
      Ok(bo) => bo,
      Err(err) => {
        tracing::error!("failed to create gbm buffer object: {}", err);
        return;
      }
    };

    let bo_fd = match bo.fd() {
      Ok(fd) => fd,
      Err(err) => {
        tracing::error!("failed to export gbm buffer object as dmabuf fd: {}", err);
        return;
      }
    };

    let bo_modifier: u64 = bo.modifier().into();
    let reported_planes = bo.plane_count();
    let plane_count = reported_planes.min(MAX_DMABUF_PLANES as u32);

    if plane_count == 0 {
      tracing::error!(reported_planes, "gbm buffer reported zero planes");
      return;
    }
    if plane_count != reported_planes {
      tracing::warn!(reported_planes, supported = plane_count, "clamping dmabuf plane count");
    }

    let mut planes = [DmabufPlane::default(); MAX_DMABUF_PLANES];
    for (i, plane) in planes.iter_mut().take(plane_count as usize).enumerate() {
      *plane = DmabufPlane {
        stride: bo.stride_for_plane(i as i32),
        offset: bo.offset(i as i32),
      };
    }

    if let Err(err) = self.tx.send(Event::DmabufCreated(Dmabuf {
      fd: bo_fd.try_clone().expect("failed to clone gbm buffer object fd"),
      width,
      height,
      stride: bo.stride(),
      format,
      modifier: bo_modifier,
      num_planes: plane_count,
      planes,
    })) {
      tracing::error!("failed to send dmabuf created event: {}", err);
    }
    tracing::trace!("requesting dmabuf buffer creation");
    let params = dmabuf.create_params(qh, ());
    for (i, plane) in planes.iter().take(plane_count as usize).enumerate() {
      params.add(
        bo_fd.as_fd(),
        i as u32,
        plane.offset,
        plane.stride,
        (bo_modifier >> 32) as u32,
        (bo_modifier & 0xffffffff) as u32,
      );
    }

    params.create(
      width as i32,
      height as i32,
      format,
      zwp_linux_buffer_params_v1::Flags::empty(),
    );
    tracing::debug!("requested creation of dmabuf buffer");
  }
}

#[derive(thiserror::Error, Debug)]
pub enum WaylandError {
  #[error(transparent)]
  Connection(#[from] wayland_client::ConnectError),
  #[error(transparent)]
  Dispatch(#[from] wayland_client::DispatchError),
  #[error(transparent)]
  Wayland(#[from] wayland_client::backend::WaylandError),
  #[error(transparent)]
  CalloopWaylandSource(#[from] calloop::InsertError<calloop_wayland_source::WaylandSource<Wayland>>),
  #[error(transparent)]
  CalloopChannelCommand(#[from] calloop::InsertError<channel::Channel<Command>>),
}
