use std::{ffi::CString, io::Cursor, ptr};

use crate::gpu::ComputeError;
use ash::{Device, util::read_spv, vk};

use super::{DispatchConfig, ImageInfo, ShaderResult};

pub const SPV: &[u8] = include_bytes!(env!("border_colors.spv"));

pub struct Shaders {
  descriptor_set_layout: vk::DescriptorSetLayout,
  pipeline_layout: vk::PipelineLayout,
  pipeline: vk::Pipeline,
  descriptor_pool: vk::DescriptorPool,
  descriptor_set: vk::DescriptorSet,
  output: ShaderOutput<ReturnBuffer>,
}

impl Shaders {
  pub fn new(device: &Device, memory_properties: &vk::PhysicalDeviceMemoryProperties) -> Result<Self, ComputeError> {
    let bindings = [
      vk::DescriptorSetLayoutBinding {
        binding: 0,
        descriptor_type: vk::DescriptorType::STORAGE_BUFFER,
        descriptor_count: 1,
        stage_flags: vk::ShaderStageFlags::COMPUTE,
        ..Default::default()
      },
      vk::DescriptorSetLayoutBinding {
        binding: 1,
        descriptor_type: vk::DescriptorType::STORAGE_BUFFER,
        descriptor_count: 1,
        stage_flags: vk::ShaderStageFlags::COMPUTE,
        ..Default::default()
      },
    ];
    let descriptor_set_layout_info = vk::DescriptorSetLayoutCreateInfo {
      binding_count: bindings.len() as u32,
      p_bindings: bindings.as_ptr(),
      ..Default::default()
    };
    let descriptor_set_layout = unsafe { device.create_descriptor_set_layout(&descriptor_set_layout_info, None)? };

    let push_constant_range = vk::PushConstantRange {
      stage_flags: vk::ShaderStageFlags::COMPUTE,
      offset: 0,
      size: std::mem::size_of::<DispatchParams>() as u32,
    };
    let pipeline_layout_info = vk::PipelineLayoutCreateInfo {
      set_layout_count: 1,
      p_set_layouts: &descriptor_set_layout,
      push_constant_range_count: 1,
      p_push_constant_ranges: &push_constant_range,
      ..Default::default()
    };
    let pipeline_layout = unsafe { device.create_pipeline_layout(&pipeline_layout_info, None)? };

    let shader_code = read_spv(&mut Cursor::new(SPV)).map_err(ComputeError::ShaderRead)?;
    let shader_module_info = vk::ShaderModuleCreateInfo {
      code_size: shader_code.len() * std::mem::size_of::<u32>(),
      p_code: shader_code.as_ptr(),
      ..Default::default()
    };
    let shader_module = unsafe { device.create_shader_module(&shader_module_info, None)? };

    let entry_point = CString::new("main")?;
    let shader_stage = vk::PipelineShaderStageCreateInfo {
      stage: vk::ShaderStageFlags::COMPUTE,
      module: shader_module,
      p_name: entry_point.as_ptr(),
      ..Default::default()
    };
    let pipeline_info = vk::ComputePipelineCreateInfo {
      stage: shader_stage,
      layout: pipeline_layout,
      ..Default::default()
    };
    let pipeline = match unsafe {
      device.create_compute_pipelines(vk::PipelineCache::null(), std::slice::from_ref(&pipeline_info), None)
    } {
      Ok(mut pipelines) => pipelines.pop().ok_or(ComputeError::PipelineCreation)?,
      Err((_, err)) => {
        unsafe {
          device.destroy_shader_module(shader_module, None);
          device.destroy_pipeline_layout(pipeline_layout, None);
          device.destroy_descriptor_set_layout(descriptor_set_layout, None);
        }
        return Err(err.into());
      }
    };
    unsafe {
      device.destroy_shader_module(shader_module, None);
    }

    let descriptor_pool_sizes = [vk::DescriptorPoolSize {
      ty: vk::DescriptorType::STORAGE_BUFFER,
      descriptor_count: 2,
    }];
    let descriptor_pool_info = vk::DescriptorPoolCreateInfo {
      pool_size_count: descriptor_pool_sizes.len() as u32,
      p_pool_sizes: descriptor_pool_sizes.as_ptr(),
      max_sets: 1,
      ..Default::default()
    };
    let descriptor_pool = unsafe { device.create_descriptor_pool(&descriptor_pool_info, None)? };

    let descriptor_set_alloc_info = vk::DescriptorSetAllocateInfo {
      descriptor_pool,
      descriptor_set_count: 1,
      p_set_layouts: &descriptor_set_layout,
      ..Default::default()
    };
    let descriptor_set = match unsafe { device.allocate_descriptor_sets(&descriptor_set_alloc_info) } {
      Ok(mut sets) => sets.pop().ok_or(ComputeError::DescriptorAllocation)?,
      Err(err) => {
        unsafe {
          device.destroy_descriptor_pool(descriptor_pool, None);
          device.destroy_pipeline(pipeline, None);
          device.destroy_pipeline_layout(pipeline_layout, None);
          device.destroy_descriptor_set_layout(descriptor_set_layout, None);
        }
        return Err(err.into());
      }
    };

    let output = match ShaderOutput::new(device, memory_properties) {
      Ok(output) => output,
      Err(err) => {
        unsafe {
          device.destroy_descriptor_pool(descriptor_pool, None);
          device.destroy_pipeline(pipeline, None);
          device.destroy_pipeline_layout(pipeline_layout, None);
          device.destroy_descriptor_set_layout(descriptor_set_layout, None);
        }
        return Err(err);
      }
    };

    let output_info = [vk::DescriptorBufferInfo {
      buffer: output.buffer(),
      offset: 0,
      range: std::mem::size_of::<ReturnBuffer>() as u64,
    }];
    let write = [vk::WriteDescriptorSet {
      s_type: vk::StructureType::WRITE_DESCRIPTOR_SET,
      dst_set: descriptor_set,
      dst_binding: 1,
      descriptor_count: 1,
      descriptor_type: vk::DescriptorType::STORAGE_BUFFER,
      p_buffer_info: output_info.as_ptr(),
      ..Default::default()
    }];
    unsafe {
      device.update_descriptor_sets(&write, &[]);
    }

    output.reset();

    Ok(Self {
      descriptor_set_layout,
      pipeline_layout,
      pipeline,
      descriptor_pool,
      descriptor_set,
      output,
    })
  }

  pub fn bind_input_buffer(&self, device: &Device, buffer: vk::Buffer, range: vk::DeviceSize) {
    let input_info = [vk::DescriptorBufferInfo {
      buffer,
      offset: 0,
      range,
    }];
    let write = [vk::WriteDescriptorSet {
      s_type: vk::StructureType::WRITE_DESCRIPTOR_SET,
      dst_set: self.descriptor_set,
      dst_binding: 0,
      descriptor_count: 1,
      descriptor_type: vk::DescriptorType::STORAGE_BUFFER,
      p_buffer_info: input_info.as_ptr(),
      ..Default::default()
    }];
    unsafe {
      device.update_descriptor_sets(&write, &[]);
    }
  }

  pub fn reset_output(&self) {
    self.output.reset();
  }

  pub fn prepare<'a>(&'a self, scratch: &'a mut DispatchParams, image: ImageInfo) -> DispatchConfig<'a> {
    *scratch = DispatchParams {
      width: image.width,
      height: image.height,
      stride: image.stride,
      bytes_per_pixel: image.bytes_per_pixel,
    };

    let push_constants = unsafe {
      std::slice::from_raw_parts(
        (scratch as *const DispatchParams) as *const u8,
        std::mem::size_of::<DispatchParams>(),
      )
    };

    DispatchConfig {
      pipeline: self.pipeline,
      pipeline_layout: self.pipeline_layout,
      descriptor_set: self.descriptor_set,
      push_constants,
      output_ptr: self.output.mapped() as *mut std::ffi::c_void,
      readback: border_colors_readback,
    }
  }

  pub unsafe fn destroy(&mut self, device: &Device) {
    unsafe {
      self.output.destroy(device);
      device.destroy_descriptor_pool(self.descriptor_pool, None);
      device.destroy_pipeline(self.pipeline, None);
      device.destroy_pipeline_layout(self.pipeline_layout, None);
      device.destroy_descriptor_set_layout(self.descriptor_set_layout, None);
    }
  }
}

struct ShaderOutput<T> {
  buffer: vk::Buffer,
  memory: vk::DeviceMemory,
  mapped: *mut T,
}

impl<T: Default> ShaderOutput<T> {
  fn new(device: &Device, memory_properties: &vk::PhysicalDeviceMemoryProperties) -> Result<Self, ComputeError> {
    let buffer_info = vk::BufferCreateInfo {
      size: std::mem::size_of::<T>() as u64,
      usage: vk::BufferUsageFlags::STORAGE_BUFFER,
      sharing_mode: vk::SharingMode::EXCLUSIVE,
      ..Default::default()
    };
    let buffer = unsafe { device.create_buffer(&buffer_info, None)? };
    let requirements = unsafe { device.get_buffer_memory_requirements(buffer) };

    let memory_type_index = find_memory_type(
      memory_properties,
      requirements.memory_type_bits,
      vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
    )
    .ok_or(ComputeError::NoHostVisibleMemory)?;

    let allocate_info = vk::MemoryAllocateInfo {
      allocation_size: requirements.size,
      memory_type_index,
      ..Default::default()
    };

    let memory = match unsafe { device.allocate_memory(&allocate_info, None) } {
      Ok(memory) => memory,
      Err(err) => {
        unsafe {
          device.destroy_buffer(buffer, None);
        }
        return Err(err.into());
      }
    };

    if let Err(err) = unsafe { device.bind_buffer_memory(buffer, memory, 0) } {
      unsafe {
        device.free_memory(memory, None);
        device.destroy_buffer(buffer, None);
      }
      return Err(err.into());
    }

    let mapped_raw =
      match unsafe { device.map_memory(memory, 0, allocate_info.allocation_size, vk::MemoryMapFlags::empty()) } {
        Ok(ptr) => ptr,
        Err(err) => {
          unsafe {
            device.free_memory(memory, None);
            device.destroy_buffer(buffer, None);
          }
          return Err(err.into());
        }
      };

    let mapped = mapped_raw.cast::<T>();
    unsafe {
      ptr::write(mapped, T::default());
    }

    Ok(Self { buffer, memory, mapped })
  }

  fn buffer(&self) -> vk::Buffer {
    self.buffer
  }

  fn mapped(&self) -> *mut T {
    self.mapped
  }

  fn reset(&self) {
    unsafe {
      ptr::write(self.mapped, T::default());
    }
  }

  unsafe fn destroy(&mut self, device: &Device) {
    unsafe {
      if !self.mapped.is_null() {
        device.unmap_memory(self.memory);
        self.mapped = ptr::null_mut();
      }
      device.destroy_buffer(self.buffer, None);
      device.free_memory(self.memory, None);
    }
  }
}

unsafe fn border_colors_readback(ptr: *mut std::ffi::c_void) -> ShaderResult {
  let raw = ptr.cast::<ReturnBuffer>();
  let value = unsafe { ptr::read(raw) };
  ShaderResult::from(Output::from(value))
}

fn find_memory_type(
  properties: &vk::PhysicalDeviceMemoryProperties,
  type_bits: u32,
  flags: vk::MemoryPropertyFlags,
) -> Option<u32> {
  for index in 0..properties.memory_type_count {
    let supported = type_bits & (1 << index) != 0;
    if supported && properties.memory_types[index as usize].property_flags.contains(flags) {
      return Some(index);
    }
  }
  None
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DispatchParams {
  pub width: u32,
  pub height: u32,
  pub stride: u32,
  pub bytes_per_pixel: u32,
}

#[repr(C, align(16))]
#[derive(Clone, Copy, Debug, Default)]
pub struct ReturnBuffer {
  sum_r: i64,
  sum_g: i64,
  sum_b: i64,
  pixel_count: i64,
  _padding: i64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Output {
  pub sum_r: i64,
  pub sum_g: i64,
  pub sum_b: i64,
  pub pixel_count: i64,
}

impl Output {
  pub fn average_rgb(&self) -> Option<[f32; 3]> {
    if self.pixel_count <= 0 {
      return None;
    }
    let count = self.pixel_count as f32;
    Some([
      (self.sum_r as f32) / count,
      (self.sum_g as f32) / count,
      (self.sum_b as f32) / count,
    ])
  }

  pub fn average_rgb_u8(&self) -> Option<[u8; 3]> {
    self.average_rgb().map(|[r, g, b]| {
      [
        r.clamp(0.0, 255.0).round() as u8,
        g.clamp(0.0, 255.0).round() as u8,
        b.clamp(0.0, 255.0).round() as u8,
      ]
    })
  }
}

impl From<ReturnBuffer> for Output {
  fn from(value: ReturnBuffer) -> Self {
    Self {
      sum_r: value.sum_r,
      sum_g: value.sum_g,
      sum_b: value.sum_b,
      pixel_count: value.pixel_count,
    }
  }
}
