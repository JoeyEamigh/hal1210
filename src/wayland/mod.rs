use std::{
  cell::OnceCell,
  os::fd::{AsFd, OwnedFd},
};

use calloop::{LoopHandle, channel};
use calloop_wayland_source::WaylandSource;
use wayland_client::{
  Connection, Dispatch, Proxy, QueueHandle, delegate_noop, event_created_child,
  protocol::{
    wl_buffer::WlBuffer,
    wl_output::WlOutput,
    wl_registry::{self, WlRegistry},
    wl_seat::WlSeat,
  },
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
  zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
  zwp_linux_dmabuf_feedback_v1::{self, ZwpLinuxDmabufFeedbackV1},
  zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
};
use wayland_protocols_wlr::screencopy::v1::client::{
  zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
  zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

use crate::gpu;

pub type CommandTx = calloop::channel::Sender<Command>;
pub type CommandRx = calloop::channel::Channel<Command>;
pub type EventTx = tokio::sync::mpsc::UnboundedSender<Event>;
pub type EventRx = tokio::sync::mpsc::UnboundedReceiver<Event>;

#[derive(Debug)]
pub enum Command {
  ComputeDone,
}

#[derive(Debug)]
pub enum Event {
  DmabufCreated {
    fd: OwnedFd,
    width: u32,
    height: u32,
    stride: u32,
    format: u32,
    modifier: u64,
  },
  FrameReady,
}

pub struct Wayland {
  tx: EventTx,
  qh: QueueHandle<Wayland>,

  gbm_device: OnceCell<gbm::Device<gpu::drm::Device>>,
  seat: OnceCell<WlSeat>,
  output: OnceCell<WlOutput>,
  screencopy: OnceCell<ZwlrScreencopyManagerV1>,
  dmabuf: OnceCell<ZwpLinuxDmabufV1>,
  buffer: OnceCell<WlBuffer>,

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

      gbm_device: Default::default(),
      seat: Default::default(),
      output: Default::default(),
      screencopy: Default::default(),
      dmabuf: Default::default(),
      buffer: Default::default(),

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
}

delegate_noop!(Wayland: ignore WlSeat);
delegate_noop!(Wayland: ignore WlOutput);
delegate_noop!(Wayland: ignore WlBuffer);
delegate_noop!(Wayland: ignore ZwpLinuxDmabufV1);
delegate_noop!(Wayland: ZwlrScreencopyManagerV1);

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
    // When receiving events from the wl_registry, we are only interested in the
    // `global` event, which signals a new available global.
    // When receiving this event, we just print its characteristics in this example.
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
        _ => {}
      }
    }
  }
}

impl Dispatch<ZwpLinuxDmabufFeedbackV1, ()> for Wayland {
  #[tracing::instrument(level = "trace", target = "dmabuf", skip_all)]
  fn event(
    data: &mut Self,
    _: &ZwpLinuxDmabufFeedbackV1,
    event: zwp_linux_dmabuf_feedback_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Wayland>,
  ) {
    match event {
      zwp_linux_dmabuf_feedback_v1::Event::Done => {
        tracing::trace!("dmabuf feedback done");
      }
      zwp_linux_dmabuf_feedback_v1::Event::FormatTable { fd, size } => {
        tracing::trace!("dmabuf feedback format table: fd={fd:?}, size={size}");
      }
      zwp_linux_dmabuf_feedback_v1::Event::MainDevice { device } => {
        tracing::trace!("dmabuf feedback main device: {device:?}");

        tracing::debug!("opening drm device from dmabuf feedback main device");
        let drm_device = match gpu::drm::Device::open(device) {
          Ok(dev) => dev,
          Err(err) => {
            tracing::error!("failed to open drm device from dmabuf feedback main device: {}", err);
            return;
          }
        };
        tracing::debug!(
          "successfully opened drm device from dmabuf feedback main device: {:?}",
          drm_device
        );

        let gbm_device = match gbm::Device::new(drm_device) {
          Ok(dev) => dev,
          Err(err) => {
            tracing::error!("failed to create gbm device from drm device: {}", err);
            return;
          }
        };
        tracing::debug!("successfully created gbm device from drm device: {:?}", gbm_device);

        if let Err(err) = data.gbm_device.set(gbm_device) {
          tracing::error!("failed to store gbm device: {:?}", err);
        }
      }
      zwp_linux_dmabuf_feedback_v1::Event::TrancheDone => {
        tracing::trace!("dmabuf feedback tranche done");
      }
      zwp_linux_dmabuf_feedback_v1::Event::TrancheFlags { flags } => {
        tracing::trace!("dmabuf feedback tranche flags: {flags:?}");
      }
      zwp_linux_dmabuf_feedback_v1::Event::TrancheFormats { indices } => {
        tracing::trace!("dmabuf feedback tranche formats with len: {}", indices.len());
      }
      zwp_linux_dmabuf_feedback_v1::Event::TrancheTargetDevice { device } => {
        tracing::trace!("dmabuf feedback tranche target device: {device:?}");
      }
      unknown => {
        tracing::debug!("unknown dmabuf feedback event: {unknown:?}");
      }
    }
  }
}

impl Dispatch<ZwpLinuxBufferParamsV1, ()> for Wayland {
  #[tracing::instrument(level = "trace", target = "dmabuf", skip_all)]
  fn event(
    data: &mut Self,
    _: &ZwpLinuxBufferParamsV1,
    event: zwp_linux_buffer_params_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Wayland>,
  ) {
    match event {
      zwp_linux_buffer_params_v1::Event::Created { buffer } => {
        tracing::trace!("dmabuf buffer created: {:?}", buffer);

        if let Err(err) = data.buffer.set(buffer) {
          tracing::error!("failed to store dmabuf buffer: {:?}", err);
          return;
        }

        data.screencopy_frame();
      }
      zwp_linux_buffer_params_v1::Event::Failed => {
        tracing::error!("dmabuf buffer creation failed");
      }
      unknown => {
        tracing::warn!("unknown dmabuf buffer params event: {:?}", unknown);
      }
    }
  }

  event_created_child!(Wayland, ZwpLinuxBufferParamsV1, [zwp_linux_buffer_params_v1::EVT_CREATED_OPCODE => (WlBuffer, ())]);
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for Wayland {
  #[tracing::instrument(level = "trace", target = "screencopy", skip_all)]
  fn event(
    data: &mut Self,
    _: &ZwlrScreencopyFrameV1,
    event: zwlr_screencopy_frame_v1::Event,
    _: &(),
    _: &Connection,
    qh: &QueueHandle<Wayland>,
  ) {
    match event {
      zwlr_screencopy_frame_v1::Event::Buffer {
        format,
        width,
        height,
        stride,
      } => {
        tracing::trace!("screencopy frame buffer: format={format:?}, width={width}, height={height}, stride={stride}");
      }
      zwlr_screencopy_frame_v1::Event::Flags { flags } => {
        tracing::trace!("screencopy frame flags: {flags:?}");
      }
      zwlr_screencopy_frame_v1::Event::Ready {
        tv_sec_hi,
        tv_sec_lo,
        tv_nsec,
      } => {
        tracing::trace!(
          "screencopy frame ready: time={}s {}ns",
          ((tv_sec_hi as u64) << 32) | (tv_sec_lo as u64),
          tv_nsec
        );

        if let Err(err) = data.tx.send(Event::FrameReady) {
          tracing::error!("failed to send frame ready event: {}", err);
        }
      }
      zwlr_screencopy_frame_v1::Event::Failed => {
        tracing::error!("screencopy frame failed");
      }
      zwlr_screencopy_frame_v1::Event::BufferDone => {
        tracing::debug!("screencopy frame buffer done");
        data.screencopy_frame();
      }
      zwlr_screencopy_frame_v1::Event::Damage { x, y, width, height } => {
        tracing::trace!(
          "screencopy frame damage: x={}, y={}, width={}, height={}",
          x,
          y,
          width,
          height
        );
      }
      zwlr_screencopy_frame_v1::Event::LinuxDmabuf { format, width, height } => {
        tracing::trace!("screencopy frame linux dmabuf: format={format:?}, width={width}, height={height}");

        if data.buffer.get().is_none() {
          data.request_create_dmabuf_buffer(format, width, height, qh);
        }
      }
      _ => {}
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
    if let Err(err) = self.tx.send(Event::DmabufCreated {
      fd: bo_fd.try_clone().expect("failed to clone gbm buffer object fd"),
      width,
      height,
      stride: bo.stride(),
      format,
      modifier: bo_modifier,
    }) {
      tracing::error!("failed to send dmabuf created event: {}", err);
    }
    tracing::trace!("requesting dmabuf buffer creation");
    let params = dmabuf.create_params(qh, ());
    params.add(
      bo_fd.as_fd(),
      0,
      0,
      bo.stride(),
      (bo_modifier >> 32) as u32,
      (bo_modifier & 0xffffffff) as u32,
    );
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
