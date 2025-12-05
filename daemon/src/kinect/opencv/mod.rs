use opencv::{core, prelude::MatTraitManual, Error as OpenCvError};

use crate::kinect::freenect::VideoFrame;

pub struct RgbSummary {
  pub timestamp: u32,
  pub mean_rgb: [f64; 3],
  pub frame_bytes: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum RgbFrameError {
  #[error("unexpected RGB buffer size: expected {expected} bytes, got {actual}")]
  InvalidSize { expected: usize, actual: usize },
  #[error(transparent)]
  OpenCv(#[from] OpenCvError),
}

pub fn summarize_rgb_frame(frame: VideoFrame, width: i32, height: i32) -> Result<RgbSummary, RgbFrameError> {
  let expected_len = (width as usize) * (height as usize) * 3;
  if frame.data.len() != expected_len {
    return Err(RgbFrameError::InvalidSize {
      expected: expected_len,
      actual: frame.data.len(),
    });
  }

  let mut mat = unsafe { core::Mat::new_rows_cols(height, width, core::CV_8UC3)? };
  mat.data_bytes_mut()?.copy_from_slice(&frame.data);

  let mask = core::no_array();
  let mean = core::mean(&mat, &mask)?;

  Ok(RgbSummary {
    timestamp: frame.timestamp,
    mean_rgb: [mean[0], mean[1], mean[2]],
    frame_bytes: expected_len,
  })
}
