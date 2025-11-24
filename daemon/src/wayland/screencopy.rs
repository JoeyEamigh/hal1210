use std::os::fd::OwnedFd;

use wayland_client::{
  Connection, Dispatch, QueueHandle, delegate_noop, event_created_child, protocol::wl_buffer::WlBuffer,
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

use super::{Event, MAX_DMABUF_PLANES, Wayland};
use crate::gpu;

delegate_noop!(Wayland: ignore ZwpLinuxDmabufV1);
delegate_noop!(Wayland: ZwlrScreencopyManagerV1);

#[derive(Debug)]
pub struct Dmabuf {
  pub fd: OwnedFd,
  pub width: u32,
  pub height: u32,
  pub stride: u32,
  pub format: u32,
  pub modifier: u64,
  pub num_planes: u32,
  pub planes: [DmabufPlane; MAX_DMABUF_PLANES],
}

#[derive(Debug, Clone, Copy, Default)]
pub struct DmabufPlane {
  pub stride: u32,
  pub offset: u32,
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

        data.state.buffer_created = true;
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

        data.state.frame = None;
      }
      zwlr_screencopy_frame_v1::Event::Failed => {
        tracing::error!("screencopy frame failed");
      }
      zwlr_screencopy_frame_v1::Event::BufferDone => {
        tracing::debug!("screencopy frame buffer done");
        data.state.frame_ready_for_copy = true;
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
