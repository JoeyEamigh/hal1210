use std::{cell::OnceCell, os::fd::OwnedFd};

use ash::{vk, Entry, Instance};

pub mod drm;

const SHADER: &[u8] = include_bytes!(env!("color.spv"));

pub struct Compute {
  instance: Instance,
  dmabuf: OnceCell<OwnedFd>,
}

impl Compute {
  pub fn init() -> Result<Self, ComputeError> {
    let entry = unsafe { Entry::load()? };
    let app_info = vk::ApplicationInfo {
      api_version: vk::make_api_version(0, 1, 0, 0),
      ..Default::default()
    };
    let create_info = vk::InstanceCreateInfo {
      p_application_info: &app_info,
      ..Default::default()
    };
    let instance = unsafe { entry.create_instance(&create_info, None)? };

    Ok(Self {
      instance,
      dmabuf: Default::default(),
    })
  }

  pub fn set_dmabuf(&self, dmabuf: OwnedFd) -> Result<(), ComputeError> {
    self.dmabuf.set(dmabuf).map_err(|_| ComputeError::DmabufCell)
  }
}

#[derive(thiserror::Error, Debug)]
pub enum ComputeError {
  #[error("failed to load Vulkan library: {0}")]
  VkLibrary(#[from] ash::LoadingError),
  #[error("Vulkan error: {0}")]
  VkResult(#[from] vk::Result),
  #[error("DMA-BUF already set")]
  DmabufCell,
}
