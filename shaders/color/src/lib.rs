#![no_std]
// HACK(eddyb) can't easily see warnings otherwise from `spirv-builder` builds.
#![deny(warnings)]
#![allow(unexpected_cfgs)]

use spirv_std::{
  arch::control_barrier,
  memory::{Scope, Semantics},
  spirv,
};

#[repr(C)]
pub struct DispatchParams {
  pub width: u32,
  pub height: u32,
  pub stride: u32,
  pub bytes_per_pixel: u32,
}

#[repr(C)]
pub struct AverageBuffer {
  pub sum_r: i64,
  pub sum_g: i64,
  pub sum_b: i64,
  pub pixel_count: i64,
  pub _padding: i64,
}

const WORKGROUP_SIZE: usize = 1024;

#[repr(C)]
pub struct WorkgroupSums {
  sum_r: [i64; WORKGROUP_SIZE],
  sum_g: [i64; WORKGROUP_SIZE],
  sum_b: [i64; WORKGROUP_SIZE],
  count: [i64; WORKGROUP_SIZE],
}

#[spirv(compute(threads(1024, 1, 1)))]
pub fn main(
  #[spirv(storage_buffer, descriptor_set = 0, binding = 0)] input: &[u8],
  #[spirv(storage_buffer, descriptor_set = 0, binding = 1)] output: &mut AverageBuffer,
  #[spirv(push_constant)] params: &DispatchParams,
  #[spirv(local_invocation_index)] local_index: u32,
  #[spirv(workgroup)] shared: &mut WorkgroupSums,
) {
  let width = params.width as usize;
  let height = params.height as usize;
  let stride = params.stride as usize;
  let bpp = params.bytes_per_pixel as usize;

  if width == 0 || height == 0 || bpp < 3 {
    output.sum_r = 0;
    output.sum_g = 0;
    output.sum_b = 0;
    output.pixel_count = 0;
    output._padding = 0;
    return;
  }

  let mut sum_r = 0i64;
  let mut sum_g = 0i64;
  let mut sum_b = 0i64;
  let mut count = 0i64;

  let total_pixels = width * height;
  let mut index = local_index as usize;
  while index < total_pixels {
    let y = index / width;
    let x = index % width;
    let offset = y * stride + x * bpp;
    unsafe {
      let b_val = *input.get_unchecked(offset) as i64;
      let g_val = *input.get_unchecked(offset + 1) as i64;
      let r_val = *input.get_unchecked(offset + 2) as i64;
      sum_r += r_val;
      sum_g += g_val;
      sum_b += b_val;
      count += 1;
    }
    index += WORKGROUP_SIZE;
  }

  let local_idx = local_index as usize;
  shared.sum_r[local_idx] = sum_r;
  shared.sum_g[local_idx] = sum_g;
  shared.sum_b[local_idx] = sum_b;
  shared.count[local_idx] = count;

  control_barrier::<
    { Scope::Workgroup as u32 },
    { Scope::Workgroup as u32 },
    { Semantics::ACQUIRE_RELEASE.bits() | Semantics::WORKGROUP_MEMORY.bits() },
  >();

  if local_idx == 0 {
    let mut accum_r = 0i64;
    let mut accum_g = 0i64;
    let mut accum_b = 0i64;
    let mut accum_count = 0i64;
    let mut i = 0usize;
    while i < WORKGROUP_SIZE {
      accum_r += shared.sum_r[i];
      accum_g += shared.sum_g[i];
      accum_b += shared.sum_b[i];
      accum_count += shared.count[i];
      i += 1;
    }
    output.sum_r = accum_r;
    output.sum_g = accum_g;
    output.sum_b = accum_b;
    output.pixel_count = accum_count;
    output._padding = 0;
  }
}
