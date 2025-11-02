#![no_std]
// HACK(eddyb) can't easily see warnings otherwise from `spirv-builder` builds.
#![deny(warnings)]
#![allow(unexpected_cfgs)]

use glam::{U8Vec3, UVec3};
use spirv_std::{image::Image2d, spirv};

#[spirv(compute(threads(128, 1, 1)))]
pub fn main(
  #[spirv(global_invocation_id)] id: UVec3,
  #[spirv(descriptor_set = 0, binding = 1)] _image: &Image2d,
  #[spirv(storage_buffer, descriptor_set = 0, binding = 2)] output_rgb: &mut U8Vec3,
) {
  let r = (id.x & 0xff) as u8;
  let g = (id.y & 0xff) as u8;
  let b = (id.z & 0xff) as u8;

  *output_rgb = U8Vec3::new(r, g, b);
}
