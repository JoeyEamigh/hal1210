#![no_std]
// HACK(eddyb) can't easily see warnings otherwise from `spirv-builder` builds.
// #![deny(warnings)]
#![allow(unexpected_cfgs)]

use spirv_std::{
  Image,
  arch::control_barrier,
  glam::{IVec2, UVec4},
  memory::{Scope, Semantics},
  spirv,
};

use shadercomm::{AverageBuffer, DEPTH, DispatchParams, PADDING, SIDE_LEDS, TOP_LEDS, WorkgroupSums};

type InputImage = Image!(2D, type=f32, sampled=false);

#[spirv(compute(threads(1024, 1, 1)))]
pub fn main(
  #[spirv(descriptor_set = 0, binding = 0)] image: &InputImage,
  #[spirv(descriptor_set = 0, binding = 1, storage_buffer)] output: &mut AverageBuffer,
  #[spirv(push_constant)] params: &DispatchParams,
  #[spirv(local_invocation_index)] local_index: u32,
  #[spirv(workgroup)] shared: &mut WorkgroupSums,
) {
  let width = params.width as usize;
  let height = params.height as usize;

  if width == 0 || height == 0 || params.bytes_per_pixel < 3 {
    for i in 0..(SIDE_LEDS * 2 + TOP_LEDS * 2) {
      output.sum_r[i] = 0;
      output.sum_g[i] = 0;
      output.sum_b[i] = 0;
      output.pixel_count[i] = 0;
    }
    // output.sum_r = [0; SIDE_LEDS * 2 + TOP_LEDS];
    // output.sum_g = [0; SIDE_LEDS * 2 + TOP_LEDS];
    // output.sum_b = [0; SIDE_LEDS * 2 + TOP_LEDS];
    // output.pixel_count = [0; SIDE_LEDS * 2 + TOP_LEDS];
    // output._padding = 0;
    return;
  }

  let threads_per_led = 1024 / (SIDE_LEDS * 2 + TOP_LEDS * 2);
  let local_idx = local_index as usize;
  // Which LED we're getting the average for (0 - SIDE_LEDS * 2 + TOP_LEDS)
  let led_index = local_idx / threads_per_led;
  // For the given LED, which thread are we (0 - threads_per_led)
  let thread_index = local_idx - led_index * threads_per_led;

  // Corners of the box that we are getting the average of
  let mut x_min = 0;
  let mut y_min = 0;
  let mut x_max = 0;
  let mut y_max = 0;

  let block_height = height as f32 / SIDE_LEDS as f32;
  let block_width = width as f32 / TOP_LEDS as f32;

  let mut used_thread = true;

  if led_index < SIDE_LEDS {
    // Left side
    let height_first = (block_height * (led_index) as f32) as usize;
    let height_second = (block_height * (led_index + 1) as f32) as usize;
    y_min = height - height_second;
    y_max = height - height_first + PADDING;
    if y_min < PADDING {
      y_min = 0
    } else {
      y_min -= PADDING;
    }

    if y_max > height {
      y_max = height;
    }

    x_min = 0;
    x_max = DEPTH;
  } else if led_index < SIDE_LEDS + TOP_LEDS {
    // Top side

    let width_first = (block_width * (led_index - SIDE_LEDS) as f32) as usize;
    let width_second = (block_width * (led_index - SIDE_LEDS + 1) as f32) as usize;
    x_min = width_first;
    x_max = width_second + PADDING;

    if x_min < PADDING {
      x_min = 0;
    } else {
      x_min -= PADDING;
    }

    if x_max > width {
      x_max = width;
    }

    y_min = 0;
    y_max = DEPTH;
  } else if led_index < SIDE_LEDS * 2 + TOP_LEDS {
    // Right side

    let height_first = (block_height * (led_index - SIDE_LEDS - TOP_LEDS) as f32) as usize;
    let height_second = (block_height * (led_index - SIDE_LEDS - TOP_LEDS + 1) as f32) as usize;
    y_min = height_first;
    y_max = height_second + PADDING;

    if y_min < PADDING {
      y_min = 0;
    } else {
      y_min -= PADDING;
    }

    if y_max > height {
      y_max = height;
    }

    x_min = width - DEPTH;
    x_max = width;
  } else if led_index < SIDE_LEDS * 2 + TOP_LEDS * 2 {
    // Bottom side

    let width_first = (block_width * (led_index - SIDE_LEDS * 2 - TOP_LEDS) as f32) as usize;
    let width_second = (block_width * (led_index - SIDE_LEDS * 2 - TOP_LEDS + 1) as f32) as usize;
    x_min = width - width_second;
    x_max = width - width_first + PADDING;

    if x_min < PADDING {
      x_min = 0;
    } else {
      x_min -= PADDING;
    }

    if x_max > width {
      x_max = width;
    }

    y_min = height - DEPTH;
    y_max = height;
  } else {
    // Unused
    used_thread = false;
  }

  if used_thread {
    let block_width = x_max - x_min;
    let block_height = y_max - y_min;
    let pixels_in_block = block_width * block_height;

    let mut sum_r = 0u32;
    let mut sum_g = 0u32;
    let mut sum_b = 0u32;
    let mut count = 0u32;

    let mut index = thread_index;
    while index < pixels_in_block {
      let x = (index % block_width) + x_min;
      let y = (index / block_width) + y_min;
      let coords = IVec2::new(x as i32, y as i32);
      let texel = image.read(coords);
      let r_val = texel.x;
      let g_val = texel.y;
      let b_val = texel.z;
      sum_r += (r_val * 255.0) as u32;
      sum_g += (g_val * 255.0) as u32;
      sum_b += (b_val * 255.0) as u32;
      count += 1;

      index += threads_per_led * 2;
    }
    shared.data[local_idx].r = sum_r;
    shared.data[local_idx].g = sum_g;
    shared.data[local_idx].b = sum_b;
    shared.data[local_idx].count = count;
  }
  control_barrier::<
    { Scope::Workgroup as u32 },
    { Scope::Workgroup as u32 },
    { Semantics::ACQUIRE_RELEASE.bits() | Semantics::WORKGROUP_MEMORY.bits() },
  >();

  if used_thread && thread_index == 0 {
    let mut accum_r = 0u32;
    let mut accum_g = 0u32;
    let mut accum_b = 0u32;
    let mut accum_count = 0u32;
    let mut i = 0usize;
    let base = led_index * threads_per_led;
    while i < threads_per_led {
      let pixel = shared.data[base + i];

      accum_r += pixel.r;
      accum_g += pixel.g;
      accum_b += pixel.b;
      accum_count += pixel.count;

      i += 1;
    }
    output.sum_r[led_index] = accum_r;
    output.sum_g[led_index] = accum_g;
    output.sum_b[led_index] = accum_b;
    output.pixel_count[led_index] = accum_count;
  }
}
