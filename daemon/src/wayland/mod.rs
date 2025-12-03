use std::os::fd::AsFd;

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

struct BoundGlobal<T> {
  name: u32,
  proxy: T,
}

impl<T> BoundGlobal<T> {
  fn new(name: u32, proxy: T) -> Self {
    Self { name, proxy }
  }

  fn matches(&self, name: u32) -> bool {
    self.name == name
  }

  fn proxy(&self) -> &T {
    &self.proxy
  }
}

pub struct Wayland {
  tx: EventTx,
  qh: QueueHandle<Wayland>,

  // screencopy & dmabuf globals
  gbm_device: Option<gbm::Device<gpu::drm::Device>>,
  seat: Option<BoundGlobal<WlSeat>>,
  output: Option<BoundGlobal<WlOutput>>,
  screencopy: Option<BoundGlobal<ZwlrScreencopyManagerV1>>,
  dmabuf: Option<BoundGlobal<ZwpLinuxDmabufV1>>,
  buffer: Option<WlBuffer>,

  // idle globals
  idle: Option<BoundGlobal<ExtIdleNotifierV1>>,

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
      gbm_device: None,
      seat: None,
      output: None,
      screencopy: None,
      dmabuf: None,
      buffer: None,

      // idle globals
      idle: None,

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

  fn handle_global_remove(&mut self, name: u32) {
    if self.output.as_ref().is_some_and(|global| global.matches(name)) {
      tracing::info!("wl_output global removed; clearing screencopy state");
      self.output.take();
      self.state.frame = None;
      self.state.frame_ready_for_copy = false;
      self.state.buffer_created = false;
      self.buffer = None;
    }

    if self.screencopy.as_ref().is_some_and(|global| global.matches(name)) {
      tracing::info!("zwlr_screencopy_manager_v1 global removed; dropping pending frames");
      self.screencopy.take();
      self.state.frame = None;
      self.state.frame_ready_for_copy = false;
    }

    if self.dmabuf.as_ref().is_some_and(|global| global.matches(name)) {
      tracing::info!("zwp_linux_dmabuf_v1 global removed; dropping gbm buffers");
      self.dmabuf.take();
      self.buffer = None;
      self.gbm_device = None;
      self.state.frame = None;
      self.state.buffer_created = false;
      self.state.frame_ready_for_copy = false;
    }

    if self.seat.as_ref().is_some_and(|global| global.matches(name)) {
      tracing::info!("wl_seat global removed; removing idle notifier");
      self.seat.take();
      self.idle.take();
    }

    if self.idle.as_ref().is_some_and(|global| global.matches(name)) {
      tracing::info!("ext_idle_notifier_v1 global removed");
      self.idle.take();
    }
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
    match event {
      wl_registry::Event::Global {
        name,
        interface,
        version,
      } => {
        tracing::trace!("[{name}] {interface} (v{version})");

        match &interface[..] {
          "wl_seat" => {
            if data.seat.as_ref().is_some_and(|s| s.proxy.is_alive()) {
              tracing::warn!("received duplicate wl_seat global, ignoring");
              return;
            }

            let seat = registry.bind::<WlSeat, _, _>(name, version, qh, ());
            tracing::debug!("wl_seat global bound");

            data.seat = Some(BoundGlobal::new(name, seat));
          }
          "wl_output" => {
            if data.output.as_ref().is_some_and(|s| s.proxy.is_alive()) {
              tracing::warn!("received duplicate wl_output global, ignoring");
              return;
            }

            let output = registry.bind::<WlOutput, _, _>(name, version, qh, ());
            tracing::debug!("wl_output global bound");

            if let Some(manager) = data.screencopy.as_ref().map(|global| global.proxy()) {
              tracing::debug!("requesting screencopy on output");
              let frame = manager.capture_output(0, &output, qh, ());
              data.state.frame.replace(frame);
            }

            data.output = Some(BoundGlobal::new(name, output));
          }
          "zwp_linux_dmabuf_v1" => {
            if data.dmabuf.as_ref().is_some_and(|s| s.proxy.is_alive()) {
              tracing::warn!("received duplicate zwp_linux_dmabuf_v1 global, ignoring");
              return;
            }

            let dmabuf = registry.bind::<ZwpLinuxDmabufV1, _, _>(name, version, qh, ());
            tracing::debug!("zwp_linux_dmabuf_v1 global bound");

            tracing::debug!("requesting dmabuf default feedback");
            dmabuf.get_default_feedback(qh, ());

            data.dmabuf = Some(BoundGlobal::new(name, dmabuf));
          }
          "zwlr_screencopy_manager_v1" => {
            if data.screencopy.as_ref().is_some_and(|s| s.proxy.is_alive()) {
              tracing::warn!("received duplicate wlr_screencopy_manager global, ignoring");
              return;
            }

            let manager = registry.bind::<ZwlrScreencopyManagerV1, _, _>(name, version, qh, ());
            tracing::debug!("zwlr_screencopy_manager_v1 global bound");

            if let Some(output) = data.output.as_ref().map(|global| global.proxy()) {
              tracing::debug!("requesting screencopy on output");
              let frame = manager.capture_output(0, output, qh, ());
              data.state.frame.replace(frame);
            }

            data.screencopy = Some(BoundGlobal::new(name, manager));
          }
          "ext_idle_notifier_v1" => {
            if data.idle.as_ref().is_some_and(|s| s.proxy.is_alive()) {
              tracing::warn!("received duplicate ext_idle_notifier_v1 global, ignoring");
              return;
            }

            let idle = registry.bind::<ExtIdleNotifierV1, _, _>(name, version, qh, ());
            tracing::debug!("ext_idle_notifier_v1 global bound");

            let Some(seat) = data.seat.as_ref().map(|global| global.proxy()) else {
              tracing::error!("wl_seat is not available to request idle notification. this should not happen");
              return;
            };

            idle.get_idle_notification(IDLE_NOTIFY_TIMEOUT_MS, seat, qh, ());

            data.idle = Some(BoundGlobal::new(name, idle));
          }
          _ => {}
        }
      }
      wl_registry::Event::GlobalRemove { name } => {
        tracing::trace!("[{name}] global removed");
        data.handle_global_remove(name);
      }
      _ => {}
    }
  }
}

impl Wayland {
  fn request_screencopy_frame(&mut self) {
    let Some(manager) = self.screencopy.as_ref().map(|global| global.proxy()) else {
      tracing::error!("screencopy manager is not available to request screencopy frame");
      return;
    };

    let Some(output) = self.output.as_ref().map(|global| global.proxy()) else {
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

    let Some(buffer) = self.buffer.as_ref() else {
      // this happens on the first frame request, as the buffer is created asynchronously
      // tracing::error!("no dmabuf buffer available for screencopy frame");
      return;
    };

    tracing::debug!("requesting screencopy frame to dmabuf buffer");
    frame.copy_with_damage(buffer);
  }

  fn request_create_dmabuf_buffer(&mut self, format: u32, width: u32, height: u32, qh: &QueueHandle<Wayland>) {
    let dmabuf = match self.dmabuf.as_ref().map(|global| global.proxy()) {
      Some(dmabuf) => dmabuf,
      None => {
        tracing::error!("received screencopy buffer event but dmabuf global is not available");
        return;
      }
    };

    tracing::trace!("creating gbm buffer object");
    let gbm_device = match self.gbm_device.as_ref() {
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
