mod libfreenect {
  #![allow(unsafe_op_in_unsafe_fn)]
  #![allow(unused_imports)]
  use autocxx::prelude::*;

  include_cpp! {
    #include "libfreenect.hpp"
    safety!(unsafe_ffi)

    generate!("Freenect::FreenectTiltState")
    generate!("Freenect::FreenectDevice")
    generate!("Freenect::Freenect")
  }

  pub use ffi::*;
}

use autocxx::{c_void, prelude::*};
use libfreenect::Freenect;
use std::pin::Pin;
use tokio::sync::mpsc;

pub struct VideoFrame {
  pub data: Vec<u8>,
  pub timestamp: u32,
}

pub struct DepthFrame {
  pub data: Vec<u8>,
  pub timestamp: u32,
}

struct CallbackData<T> {
  sender: mpsc::UnboundedSender<T>,
  device: *mut Freenect::FreenectDevice,
}

pub struct Kinect {
  // Freenect context must be kept alive
  _freenect: UniquePtr<Freenect::Freenect>,
  // Device pointer (owned by Freenect)
  device: *mut Freenect::FreenectDevice,

  // Channels
  video_rx: Option<mpsc::UnboundedReceiver<VideoFrame>>,
  depth_rx: Option<mpsc::UnboundedReceiver<DepthFrame>>,

  // Pointers to callback data to drop them later
  video_cb_data: *mut c_void,
  depth_cb_data: *mut c_void,
}

unsafe impl Send for Kinect {}
unsafe impl Sync for Kinect {}

impl Kinect {
  pub fn new(index: i32) -> Result<Self, Box<dyn std::error::Error>> {
    let mut freenect = Freenect::Freenect::new().within_unique_ptr();

    // createSimpleDevice returns Pin<&mut FreenectDevice>
    let device_ref = freenect.pin_mut().createSimpleDevice(c_int(index));

    // Get raw pointer from Pin<&mut T>
    // We use unsafe to get the mutable reference and then cast to pointer
    let device_ptr = unsafe {
      let mut_ref = device_ref.get_unchecked_mut();
      mut_ref as *mut Freenect::FreenectDevice
    };

    let (video_tx, video_rx) = mpsc::unbounded_channel();
    let (depth_tx, depth_rx) = mpsc::unbounded_channel();

    let video_data = Box::new(CallbackData {
      sender: video_tx,
      device: device_ptr,
    });
    let video_data_ptr = Box::into_raw(video_data) as *mut c_void;

    let depth_data = Box::new(CallbackData {
      sender: depth_tx,
      device: device_ptr,
    });
    let depth_data_ptr = Box::into_raw(depth_data) as *mut c_void;

    unsafe {
      // We need to call setRustVideoCallback on the device.
      // Since we have a raw pointer, we need to be careful.
      // We can reconstruct a Pin<&mut> temporarily.
      Pin::new_unchecked(&mut *device_ptr).setRustVideoCallback(video_callback as *mut c_void, video_data_ptr);
      Pin::new_unchecked(&mut *device_ptr).setRustDepthCallback(depth_callback as *mut c_void, depth_data_ptr);
    }

    Ok(Kinect {
      _freenect: freenect,
      device: device_ptr,
      video_rx: Some(video_rx),
      depth_rx: Some(depth_rx),
      video_cb_data: video_data_ptr,
      depth_cb_data: depth_data_ptr,
    })
  }

  pub fn start_video(&self) {
    unsafe {
      Pin::new_unchecked(&mut *self.device).startVideo();
    }
  }

  pub fn stop_video(&self) {
    unsafe {
      Pin::new_unchecked(&mut *self.device).stopVideo();
    }
  }

  pub fn start_depth(&self) {
    unsafe {
      Pin::new_unchecked(&mut *self.device).startDepth();
    }
  }

  pub fn stop_depth(&self) {
    unsafe {
      Pin::new_unchecked(&mut *self.device).stopDepth();
    }
  }

  pub fn set_led(&self, option: libfreenect::freenect_led_options) {
    unsafe {
      Pin::new_unchecked(&mut *self.device).setLed(option);
    }
  }

  pub fn set_tilt(&self, degrees: f64) {
    unsafe {
      Pin::new_unchecked(&mut *self.device).setTiltDegrees(degrees);
    }
  }

  pub fn set_video_format(&self, fmt: libfreenect::freenect_video_format, res: libfreenect::freenect_resolution) {
    unsafe {
      Pin::new_unchecked(&mut *self.device).setVideoFormat(fmt, res);
    }
  }

  pub fn set_depth_format(&self, fmt: libfreenect::freenect_depth_format, res: libfreenect::freenect_resolution) {
    unsafe {
      Pin::new_unchecked(&mut *self.device).setDepthFormat(fmt, res);
    }
  }

  pub fn get_video_stream(&mut self) -> Option<mpsc::UnboundedReceiver<VideoFrame>> {
    self.video_rx.take()
  }

  pub fn get_depth_stream(&mut self) -> Option<mpsc::UnboundedReceiver<DepthFrame>> {
    self.depth_rx.take()
  }
}

impl Drop for Kinect {
  fn drop(&mut self) {
    unsafe {
      let mut device = Pin::new_unchecked(&mut *self.device);
      device.as_mut().stopVideo();
      device.as_mut().stopDepth();
      // Clean up callbacks
      device
        .as_mut()
        .setRustVideoCallback(std::ptr::null_mut(), std::ptr::null_mut());
      device
        .as_mut()
        .setRustDepthCallback(std::ptr::null_mut(), std::ptr::null_mut());

      // Drop callback data
      let _ = Box::from_raw(self.video_cb_data as *mut CallbackData<VideoFrame>);
      let _ = Box::from_raw(self.depth_cb_data as *mut CallbackData<DepthFrame>);
    }
  }
}

extern "C" fn video_callback(user_data: *mut c_void, data: *mut c_void, timestamp: u32) {
  unsafe {
    if user_data.is_null() || data.is_null() {
      return;
    }
    let cb_data = &*(user_data as *const CallbackData<VideoFrame>);
    let size = Pin::new_unchecked(&mut *cb_data.device).getVideoBufferSize().0;
    if size > 0 {
      let slice = std::slice::from_raw_parts(data as *const u8, size as usize);
      let vec = slice.to_vec();
      let _ = cb_data.sender.send(VideoFrame { data: vec, timestamp });
    }
  }
}

extern "C" fn depth_callback(user_data: *mut c_void, data: *mut c_void, timestamp: u32) {
  unsafe {
    if user_data.is_null() || data.is_null() {
      return;
    }
    let cb_data = &*(user_data as *const CallbackData<DepthFrame>);
    let size = Pin::new_unchecked(&mut *cb_data.device).getDepthBufferSize().0;
    if size > 0 {
      let slice = std::slice::from_raw_parts(data as *const u8, size as usize);
      let vec = slice.to_vec();
      let _ = cb_data.sender.send(DepthFrame { data: vec, timestamp });
    }
  }
}
