#![no_std]

pub const SIDE_LEDS: usize = 47; // LEDS on left and right of screen
pub const TOP_LEDS: usize = 85; // LEDS on top of screen

pub const PADDING: usize = 10; // Pixel padding

pub const DEPTH: usize = 500; // Pixels to go in, must be less than height

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AverageBuffer {
  pub sum_r: [u32; SIDE_LEDS * 2 + TOP_LEDS * 2],
  pub sum_g: [u32; SIDE_LEDS * 2 + TOP_LEDS * 2],
  pub sum_b: [u32; SIDE_LEDS * 2 + TOP_LEDS * 2],
  pub pixel_count: [u32; SIDE_LEDS * 2 + TOP_LEDS * 2],
}
impl Default for AverageBuffer {
  fn default() -> Self {
    Self {
      sum_r: [0; SIDE_LEDS * 2 + TOP_LEDS * 2],
      sum_g: [0; SIDE_LEDS * 2 + TOP_LEDS * 2],
      sum_b: [0; SIDE_LEDS * 2 + TOP_LEDS * 2],
      pixel_count: [0; SIDE_LEDS * 2 + TOP_LEDS * 2],
    }
  }
}

impl AverageBuffer {
  pub fn average_rgb(&self) -> Option<[f32; 3 * (SIDE_LEDS * 2 + TOP_LEDS * 2)]> {
    let mut averages: [f32; 3 * (SIDE_LEDS * 2 + TOP_LEDS * 2)] = [0f32; 3 * (SIDE_LEDS * 2 + TOP_LEDS * 2)];
    for i in 0..(SIDE_LEDS * 2 + TOP_LEDS * 2) {
      let pixel_count = self.pixel_count[i];
      let sum_r = self.sum_r[i];
      let sum_g = self.sum_g[i];
      let sum_b = self.sum_b[i];
      let count = pixel_count as f32;
      if pixel_count == 0 {
        averages[i * 3] = 0f32;
        averages[i * 3 + 1] = 0f32;
        averages[i * 3 + 2] = 0f32;
      } else {
        averages[i * 3] = (sum_r as f32) / count;
        averages[i * 3 + 1] = (sum_g as f32) / count;
        averages[i * 3 + 2] = (sum_b as f32) / count;
      }
    }
    Some(averages)
  }

  pub fn average_rgb_u8(&self) -> Option<[u8; 3 * (SIDE_LEDS * 2 + TOP_LEDS * 2)]> {
    self.average_rgb().map(|arr| arr.map(|x| x.clamp(0.0, 255.0) as u8))
  }
}

#[repr(C)]
pub struct WorkgroupSums {
  pub data: [PixelSum; 1024],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct PixelSum {
  pub r: u32,
  pub g: u32,
  pub b: u32,
  pub count: u32,
  // pub _pad: u32, // Padding to make size 20 bytes (5 words) -> Conflict Free!
}

#[repr(C)]
pub struct DispatchParams {
  pub width: u32,
  pub height: u32,
  pub stride: u32,
  pub bytes_per_pixel: u32,
}
