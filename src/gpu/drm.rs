use std::{
  fs::{File, OpenOptions},
  os::unix::io::{AsFd, BorrowedFd},
};

use drm::node::DrmNode;

#[derive(Debug)]
pub struct Device(File);

impl AsFd for Device {
  fn as_fd(&self) -> BorrowedFd<'_> {
    self.0.as_fd()
  }
}

impl drm::Device for Device {}

impl Device {
  pub fn open(device: Vec<u8>) -> Result<Self, DrmError> {
    let mut options = OpenOptions::new();
    options.read(true);
    options.write(true);

    let dev_id_slice = device.as_slice().try_into().map_err(|_| DrmError::InvalidDeviceId)?;
    let drm_node = DrmNode::from_dev_id(u64::from_ne_bytes(dev_id_slice))?;
    let path = drm_node.dev_path().ok_or(DrmError::MissingDevicePath)?;

    Ok(Self(options.open(path)?))
  }
}

#[derive(thiserror::Error, Debug)]
pub enum DrmError {
  #[error("failed to open DRM device: {0}")]
  Open(#[from] std::io::Error),
  #[error("drm error: {0}")]
  Node(#[from] drm::node::CreateDrmNodeError),
  #[error("invalid device id")]
  InvalidDeviceId,
  #[error("could not find device path")]
  MissingDevicePath,
}
