use super::ComputeError;
use ash::{Device, vk};

pub mod border_colors;

#[derive(Debug, Clone, Copy)]
pub enum ShaderKind {
  BorderColors,
}

#[derive(Debug, Clone, Copy)]
pub struct ImageInfo {
  pub width: u32,
  pub height: u32,
  pub stride: u32,
  pub bytes_per_pixel: u32,
}

#[derive(Default)]
pub struct DispatchScratch {
  border_colors: border_colors::DispatchParams,
}

impl DispatchScratch {
  pub(crate) fn border_colors_mut(&mut self) -> &mut border_colors::DispatchParams {
    &mut self.border_colors
  }
}

pub type ReadbackFn = unsafe fn(*mut std::ffi::c_void) -> ShaderResult;

pub struct DispatchConfig<'a> {
  pub pipeline: vk::Pipeline,
  pub pipeline_layout: vk::PipelineLayout,
  pub descriptor_set: vk::DescriptorSet,
  pub push_constants: &'a [u8],
  pub output_ptr: *mut std::ffi::c_void,
  pub readback: ReadbackFn,
}

#[derive(Debug, Clone, Copy)]
pub enum ShaderResult {
  BorderColors(border_colors::Output),
}

impl ShaderResult {
  pub fn average_rgb_u8(&self) -> Option<[u8; 3]> {
    match self {
      ShaderResult::BorderColors(output) => output.average_rgb_u8(),
    }
  }
}

impl Default for ShaderResult {
  fn default() -> Self {
    ShaderResult::BorderColors(border_colors::Output::default())
  }
}

impl From<border_colors::Output> for ShaderResult {
  fn from(value: border_colors::Output) -> Self {
    ShaderResult::BorderColors(value)
  }
}

pub struct Shaders {
  border_colors: border_colors::Shaders,
}

impl Shaders {
  pub fn new(device: &Device, memory_properties: &vk::PhysicalDeviceMemoryProperties) -> Result<Self, ComputeError> {
    let border_colors = border_colors::Shaders::new(device, memory_properties)?;
    Ok(Self { border_colors })
  }

  pub fn bind_input(&self, device: &Device, buffer: vk::Buffer, range: vk::DeviceSize) {
    self.border_colors.bind_input_buffer(device, buffer, range);
  }

  pub fn reset_output(&self, shader: ShaderKind) {
    match shader {
      ShaderKind::BorderColors => self.border_colors.reset_output(),
    }
  }

  pub fn prepare<'a>(
    &'a self,
    shader: ShaderKind,
    scratch: &'a mut DispatchScratch,
    image: ImageInfo,
  ) -> DispatchConfig<'a> {
    match shader {
      ShaderKind::BorderColors => self.border_colors.prepare(scratch.border_colors_mut(), image),
    }
  }

  pub unsafe fn destroy(&mut self, device: &Device) {
    unsafe {
      self.border_colors.destroy(device);
    }
  }
}

unsafe impl Send for Shaders {}
unsafe impl Sync for Shaders {}

