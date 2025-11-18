#![no_std]

pub const SIDE_LEDS: usize = 20; // LEDS on left and right of screen
pub const TOP_LEDS: usize = 50; // LEDS on top of screen

#[repr(C)]
pub struct AverageBuffer {
  pub sum_r: i64,
  pub sum_g: i64,
  pub sum_b: i64,
  pub pixel_count: i64,
  pub _padding: i64,
}
