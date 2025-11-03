use std::{
  ffi::CString,
  os::fd::{FromRawFd, IntoRawFd, OwnedFd},
  ptr,
  sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
  },
  thread,
  time::Instant,
};

use self::shader::{DispatchScratch, ImageInfo, ShaderKind, ShaderResult, Shaders};
use ash::{Device, Entry, Instance, vk};
use tokio::{sync::oneshot, task::spawn_blocking};

use crate::wayland;

pub mod drm;
mod shader;

pub type ComputeOutput = ShaderResult;

const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
  (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
}

const DRM_FORMAT_ARGB8888: u32 = fourcc(b'A', b'R', b'2', b'4');
const DRM_FORMAT_XRGB8888: u32 = fourcc(b'X', b'R', b'2', b'4');
const DRM_FORMAT_ABGR8888: u32 = fourcc(b'A', b'B', b'2', b'4');
const DRM_FORMAT_XBGR8888: u32 = fourcc(b'X', b'B', b'2', b'4');

pub struct Compute {
  // vulkan stuffs
  _entry: Entry,
  instance: Instance,
  device: Device,
  queue: vk::Queue,
  memory_properties: vk::PhysicalDeviceMemoryProperties,
  command_pool: vk::CommandPool,
  command_buffer: vk::CommandBuffer,
  fence: vk::Fence,

  // shader-specific stuffs
  screen_dmabuf: Option<MappedDmabuf>, // screen buffer dmabuf mapped to vulkan
  shaders: Shaders,                    // shader output buffers
  in_flight: Arc<AtomicBool>,
}

struct MappedDmabuf {
  memory: vk::DeviceMemory,
  buffer: vk::Buffer,
  size: vk::DeviceSize,
  image: ImageInfo,
}

unsafe impl Send for Compute {}
unsafe impl Sync for Compute {}

impl Compute {
  pub fn init() -> Result<Self, ComputeError> {
    let entry = unsafe { Entry::load()? };
    let app_info = vk::ApplicationInfo {
      api_version: vk::make_api_version(0, 1, 0, 0),
      ..Default::default()
    };
    let create_info = vk::InstanceCreateInfo {
      p_application_info: &app_info,
      ..Default::default()
    };
    let instance = unsafe { entry.create_instance(&create_info, None)? };

    let (physical_device, queue_family_index, memory_properties) = Self::select_physical_device(&instance)?;

    let queue_priorities = [1.0];
    let queue_info = vk::DeviceQueueCreateInfo {
      queue_family_index,
      queue_count: 1,
      p_queue_priorities: queue_priorities.as_ptr(),
      ..Default::default()
    };
    let device_extension_names = [
      CString::new("VK_KHR_external_memory")?,
      CString::new("VK_KHR_external_memory_fd")?,
    ];
    let device_extension_ptrs: Vec<*const i8> = device_extension_names.iter().map(|ext| ext.as_ptr()).collect();
    let device_info = vk::DeviceCreateInfo {
      queue_create_info_count: 1,
      p_queue_create_infos: &queue_info,
      enabled_extension_count: device_extension_ptrs.len() as u32,
      pp_enabled_extension_names: device_extension_ptrs.as_ptr(),
      ..Default::default()
    };
    let device = unsafe { instance.create_device(physical_device, &device_info, None)? };
    let queue = unsafe { device.get_device_queue(queue_family_index, 0) };

    let shaders = Shaders::new(&device, &memory_properties)?;

    let command_pool_info = vk::CommandPoolCreateInfo {
      flags: vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER,
      queue_family_index,
      ..Default::default()
    };
    let command_pool = unsafe { device.create_command_pool(&command_pool_info, None)? };
    let command_buffer_alloc_info = vk::CommandBufferAllocateInfo {
      command_pool,
      level: vk::CommandBufferLevel::PRIMARY,
      command_buffer_count: 1,
      ..Default::default()
    };
    let command_buffer = unsafe { device.allocate_command_buffers(&command_buffer_alloc_info) }?[0];

    let fence_info = vk::FenceCreateInfo {
      flags: vk::FenceCreateFlags::SIGNALED,
      ..Default::default()
    };
    let fence = unsafe { device.create_fence(&fence_info, None)? };

    Ok(Self {
      _entry: entry,
      instance,
      device,
      queue,
      memory_properties,
      command_pool,
      command_buffer,
      fence,
      screen_dmabuf: None,
      shaders,
      in_flight: Arc::new(AtomicBool::new(false)),
    })
  }

  #[tracing::instrument(level = "debug", skip(self))]
  pub fn wait_for_idle(&self) {
    if !self.in_flight.load(Ordering::Acquire) {
      return;
    }
    tracing::debug!("waiting for in-flight compute to finish");
    let _ = unsafe { self.device.wait_for_fences(&[self.fence], true, u64::MAX) };
    while self.in_flight.load(Ordering::Acquire) {
      thread::yield_now();
    }
  }

  pub fn set_screen_dmabuf(&mut self, dmabuf: wayland::Dmabuf) -> Result<(), ComputeError> {
    tracing::debug!(
      width = dmabuf.width,
      height = dmabuf.height,
      stride = dmabuf.stride,
      format = %fourcc_string(dmabuf.format),
      modifier = dmabuf.modifier,
      "importing dma-buf"
    );
    self.release_screen_dmabuf();
    let imported = self.import_screen_dmabuf(dmabuf)?;
    self.screen_dmabuf = Some(imported);
    Ok(())
  }

  #[tracing::instrument(level = "trace", skip(self))]
  pub fn dispatch(&self) -> Result<oneshot::Receiver<ComputeOutput>, ComputeError> {
    self.dispatch_with(ShaderKind::BorderColors)
  }

  #[tracing::instrument(level = "trace", skip(self))]
  pub fn dispatch_with(&self, shader: ShaderKind) -> Result<oneshot::Receiver<ComputeOutput>, ComputeError> {
    if self.in_flight.swap(true, Ordering::Acquire) {
      return Err(ComputeError::DispatchInFlight);
    }

    let Some(dmabuf) = &self.screen_dmabuf else {
      self.in_flight.store(false, Ordering::Release);
      return Err(ComputeError::NoDmabuf);
    };

    self.shaders.reset_output(shader);

    let mut scratch = DispatchScratch::default();
    let image = dmabuf.image;
    let config = self.shaders.prepare(shader, &mut scratch, image);

    let record_result: Result<(), vk::Result> = unsafe {
      tracing::trace!("resetting fence");
      self.device.reset_fences(&[self.fence])?;
      self
        .device
        .reset_command_pool(self.command_pool, vk::CommandPoolResetFlags::empty())?;

      let begin_info = vk::CommandBufferBeginInfo::default();
      tracing::trace!("beginning command buffer recording");
      self.device.begin_command_buffer(self.command_buffer, &begin_info)?;
      self
        .device
        .cmd_bind_pipeline(self.command_buffer, vk::PipelineBindPoint::COMPUTE, config.pipeline);

      let descriptor_sets = [config.descriptor_set];
      tracing::trace!("binding descriptor sets");
      self.device.cmd_bind_descriptor_sets(
        self.command_buffer,
        vk::PipelineBindPoint::COMPUTE,
        config.pipeline_layout,
        0,
        &descriptor_sets,
        &[],
      );

      self.device.cmd_push_constants(
        self.command_buffer,
        config.pipeline_layout,
        vk::ShaderStageFlags::COMPUTE,
        0,
        config.push_constants,
      );

      self.device.cmd_dispatch(self.command_buffer, 1, 1, 1);
      tracing::trace!("ending command buffer recording");
      self.device.end_command_buffer(self.command_buffer)?;
      Ok(())
    };
    if let Err(err) = record_result {
      self.in_flight.store(false, Ordering::Release);
      return Err(err.into());
    }

    let readback = config.readback;
    let output_ptr = config.output_ptr as usize;

    let submit_info = vk::SubmitInfo {
      command_buffer_count: 1,
      p_command_buffers: &self.command_buffer,
      ..Default::default()
    };
    tracing::trace!("submitting command buffer to queue");
    if let Err(err) = unsafe { self.device.queue_submit(self.queue, &[submit_info], self.fence) } {
      self.in_flight.store(false, Ordering::Release);
      return Err(err.into());
    }

    let device = self.device.clone();
    let fence = self.fence;
    let in_flight = self.in_flight.clone();
    let readback_fn = readback;
    let output_ptr_value = output_ptr;
    let (tx, rx) = oneshot::channel();

    let wait_span = tracing::trace_span!("wait_for_fence");
    let wait_handle = spawn_blocking(move || {
      let _enter = wait_span.enter();
      tracing::trace!("waiting for fence completion");
      let start = Instant::now();
      let wait_result = unsafe { device.wait_for_fences(&[fence], true, u64::MAX) };
      let elapsed = start.elapsed();
      let output = match wait_result {
        Ok(_) => {
          tracing::debug!(elapsed_ms = elapsed.as_secs_f64() * 1_000.0, "fence signaled");
          let output_ptr = output_ptr_value as *mut std::ffi::c_void;
          unsafe { readback_fn(output_ptr) }
        }
        Err(err) => {
          tracing::error!(?err, "failed to wait for fence");
          ComputeOutput::default()
        }
      };
      let _ = tx.send(output);
      in_flight.store(false, Ordering::Release);
    });
    drop(wait_handle);

    Ok(rx)
  }

  #[tracing::instrument(level = "trace", skip_all)]
  fn import_screen_dmabuf(&self, dmabuf: wayland::Dmabuf) -> Result<MappedDmabuf, ComputeError> {
    let (bytes_per_pixel, known_format) = format_bytes_per_pixel(dmabuf.format);
    if !known_format {
      tracing::warn!(
        format = %fourcc_string(dmabuf.format),
        "unsupported dmabuf format, assuming 4 bytes per pixel"
      );
    }

    let size = vk::DeviceSize::from(dmabuf.stride) * vk::DeviceSize::from(dmabuf.height);
    if size == 0 {
      return Err(ComputeError::EmptyDmabuf);
    }

    let external_info = vk::ExternalMemoryBufferCreateInfo {
      handle_types: vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
      ..Default::default()
    };
    let buffer_info = vk::BufferCreateInfo {
      p_next: &external_info as *const _ as *const std::ffi::c_void,
      flags: vk::BufferCreateFlags::empty(),
      size,
      usage: vk::BufferUsageFlags::STORAGE_BUFFER,
      sharing_mode: vk::SharingMode::EXCLUSIVE,
      ..Default::default()
    };
    let buffer = unsafe { self.device.create_buffer(&buffer_info, None)? };
    let requirements = unsafe { self.device.get_buffer_memory_requirements(buffer) };

    let memory_type = self
      .find_memory_type(requirements.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)
      .ok_or(ComputeError::NoSuitableMemory)?;

    let fd = dmabuf.fd.into_raw_fd();
    let dmabuf_info = vk::ImportMemoryFdInfoKHR {
      s_type: vk::StructureType::IMPORT_MEMORY_FD_INFO_KHR,
      p_next: ptr::null(),
      handle_type: vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
      fd,
      _marker: std::marker::PhantomData,
    };
    let alloc_info = vk::MemoryAllocateInfo {
      allocation_size: requirements.size,
      memory_type_index: memory_type,
      p_next: &dmabuf_info as *const _ as *const std::ffi::c_void,
      ..Default::default()
    };
    let memory = match unsafe { self.device.allocate_memory(&alloc_info, None) } {
      Ok(memory) => memory,
      Err(err) => {
        unsafe {
          let _ = OwnedFd::from_raw_fd(fd);
          self.device.destroy_buffer(buffer, None);
        }
        return Err(err.into());
      }
    };
    unsafe {
      self.device.bind_buffer_memory(buffer, memory, 0)?;
    }

    let image = ImageInfo {
      width: dmabuf.width,
      height: dmabuf.height,
      stride: dmabuf.stride,
      bytes_per_pixel,
    };

    self.shaders.bind_input(&self.device, buffer, size);

    Ok(MappedDmabuf {
      memory,
      buffer,
      size,
      image,
    })
  }

  #[tracing::instrument(level = "trace", skip(self))]
  fn release_screen_dmabuf(&mut self) {
    if let Some(imported) = self.screen_dmabuf.take() {
      tracing::trace!(size = imported.size, "releasing imported dma-buf");
      unsafe {
        self.device.destroy_buffer(imported.buffer, None);
        self.device.free_memory(imported.memory, None);
      }
    }
  }

  #[tracing::instrument(level = "trace", skip_all)]
  fn select_physical_device(
    instance: &Instance,
  ) -> Result<(vk::PhysicalDevice, u32, vk::PhysicalDeviceMemoryProperties), ComputeError> {
    let devices = unsafe { instance.enumerate_physical_devices()? };
    let Some(&device) = devices.first() else {
      return Err(ComputeError::NoPhysicalDevice);
    };
    let queue_families = unsafe { instance.get_physical_device_queue_family_properties(device) };
    let (index, _) = queue_families
      .iter()
      .enumerate()
      .find(|&(_, info)| info.queue_flags.contains(vk::QueueFlags::COMPUTE))
      .ok_or(ComputeError::NoComputeQueue)?;
    let memory_properties = unsafe { instance.get_physical_device_memory_properties(device) };
    Ok((device, index as u32, memory_properties))
  }

  fn find_memory_type(&self, type_bits: u32, flags: vk::MemoryPropertyFlags) -> Option<u32> {
    Self::find_memory_type_static(&self.memory_properties, type_bits, flags)
  }

  fn find_memory_type_static(
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
}

impl Drop for Compute {
  fn drop(&mut self) {
    self.wait_for_idle();
    self.release_screen_dmabuf();
    unsafe {
      self.shaders.destroy(&self.device);
      self.device.destroy_fence(self.fence, None);
      self
        .device
        .free_command_buffers(self.command_pool, &[self.command_buffer]);
      self.device.destroy_command_pool(self.command_pool, None);
      self.device.destroy_device(None);
      self.instance.destroy_instance(None);
    }
  }
}

fn format_bytes_per_pixel(format: u32) -> (u32, bool) {
  let bpp = match format {
    DRM_FORMAT_ARGB8888 | DRM_FORMAT_XRGB8888 | DRM_FORMAT_ABGR8888 | DRM_FORMAT_XBGR8888 => 4,
    _ => 4,
  };
  let known = matches!(
    format,
    DRM_FORMAT_ARGB8888 | DRM_FORMAT_XRGB8888 | DRM_FORMAT_ABGR8888 | DRM_FORMAT_XBGR8888
  );
  (bpp, known)
}

fn fourcc_string(code: u32) -> String {
  let bytes = [
    (code & 0xff) as u8,
    ((code >> 8) & 0xff) as u8,
    ((code >> 16) & 0xff) as u8,
    ((code >> 24) & 0xff) as u8,
  ];
  String::from_utf8_lossy(&bytes).into_owned()
}

#[derive(thiserror::Error, Debug)]
pub enum ComputeError {
  #[error("failed to load Vulkan library: {0}")]
  VkLibrary(#[from] ash::LoadingError),
  #[error("Vulkan error: {0}")]
  VkResult(#[from] vk::Result),
  #[error("failed to read shader module: {0}")]
  ShaderRead(std::io::Error),
  #[error("C interop null error: {0}")]
  NulError(#[from] std::ffi::NulError),
  #[error("failed to create compute pipeline")]
  PipelineCreation,
  #[error("failed to allocate descriptor set")]
  DescriptorAllocation,
  #[error("no Vulkan physical device available")]
  NoPhysicalDevice,
  #[error("no compute-capable queue family found")]
  NoComputeQueue,
  #[error("no host-visible memory type available")]
  NoHostVisibleMemory,
  #[error("no device-local memory type available")]
  NoSuitableMemory,
  #[error("no DMA-BUF available")]
  NoDmabuf,
  #[error("DMA-BUF has zero size")]
  EmptyDmabuf,
  #[error("command buffer already in flight")]
  DispatchInFlight,
}
