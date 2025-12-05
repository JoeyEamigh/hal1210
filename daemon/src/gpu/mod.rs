use std::{
  ffi::CString,
  io,
  os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd},
  ptr,
  sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
  },
  thread,
  time::Instant,
};

use self::shader::{DispatchScratch, ImageInfo, ShaderKind, ShaderResult, Shaders};
use ash::{vk, Device, Entry, Instance};
use tokio::{sync::oneshot, task::spawn_blocking};

use crate::wayland;

pub mod drm;
mod shader;

#[cfg(test)]
mod test;

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
  queue_family_index: u32,
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
  image: vk::Image,
  image_view: vk::ImageView,
  subresource_range: vk::ImageSubresourceRange,
  info: ImageInfo,
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
    let supported_features = unsafe { instance.get_physical_device_features(physical_device) };
    if supported_features.shader_storage_image_read_without_format == vk::FALSE {
      return Err(ComputeError::MissingFeature("shaderStorageImageReadWithoutFormat"));
    }
    let enabled_features = vk::PhysicalDeviceFeatures {
      shader_storage_image_read_without_format: vk::TRUE,
      ..Default::default()
    };

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
      CString::new("VK_KHR_bind_memory2")?,
      CString::new("VK_EXT_image_drm_format_modifier")?,
      CString::new("VK_EXT_external_memory_dma_buf")?,
      CString::new("VK_KHR_dedicated_allocation")?,
    ];
    let device_extension_ptrs: Vec<*const i8> = device_extension_names.iter().map(|ext| ext.as_ptr()).collect();
    let device_info = vk::DeviceCreateInfo {
      queue_create_info_count: 1,
      p_queue_create_infos: &queue_info,
      enabled_extension_count: device_extension_ptrs.len() as u32,
      pp_enabled_extension_names: device_extension_ptrs.as_ptr(),
      p_enabled_features: &enabled_features,
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
      queue_family_index,
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

  pub fn set_screen_dmabuf(&mut self, dmabuf: wayland::screencopy::Dmabuf) -> Result<(), ComputeError> {
    tracing::debug!(
      width = dmabuf.width,
      height = dmabuf.height,
      stride = dmabuf.stride,
      format = %fourcc_string(dmabuf.format),
      modifier = dmabuf.modifier,
      "importing dma-buf"
    );
    self.wait_for_idle();
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
    let config = self.shaders.prepare(shader, &mut scratch, dmabuf.info);

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

      let acquire_barrier = vk::ImageMemoryBarrier {
        src_access_mask: vk::AccessFlags::MEMORY_WRITE,
        dst_access_mask: vk::AccessFlags::SHADER_READ,
        old_layout: vk::ImageLayout::GENERAL,
        new_layout: vk::ImageLayout::GENERAL,
        src_queue_family_index: vk::QUEUE_FAMILY_EXTERNAL,
        dst_queue_family_index: self.queue_family_index,
        image: dmabuf.image,
        subresource_range: dmabuf.subresource_range,
        ..Default::default()
      };
      self.device.cmd_pipeline_barrier(
        self.command_buffer,
        vk::PipelineStageFlags::ALL_COMMANDS,
        vk::PipelineStageFlags::COMPUTE_SHADER,
        vk::DependencyFlags::empty(),
        &[],
        &[],
        &[acquire_barrier],
      );

      self.device.cmd_push_constants(
        self.command_buffer,
        config.pipeline_layout,
        vk::ShaderStageFlags::COMPUTE,
        0,
        config.push_constants,
      );

      self.device.cmd_dispatch(self.command_buffer, 1, 1, 1);

      let release_barrier = vk::ImageMemoryBarrier {
        src_access_mask: vk::AccessFlags::SHADER_READ,
        dst_access_mask: vk::AccessFlags::MEMORY_WRITE,
        old_layout: vk::ImageLayout::GENERAL,
        new_layout: vk::ImageLayout::GENERAL,
        src_queue_family_index: self.queue_family_index,
        dst_queue_family_index: vk::QUEUE_FAMILY_EXTERNAL,
        image: dmabuf.image,
        subresource_range: dmabuf.subresource_range,
        ..Default::default()
      };
      self.device.cmd_pipeline_barrier(
        self.command_buffer,
        vk::PipelineStageFlags::COMPUTE_SHADER,
        vk::PipelineStageFlags::ALL_COMMANDS,
        vk::DependencyFlags::empty(),
        &[],
        &[],
        &[release_barrier],
      );
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
  fn import_screen_dmabuf(&self, dmabuf: wayland::screencopy::Dmabuf) -> Result<MappedDmabuf, ComputeError> {
    let (bytes_per_pixel, known_format) = format_bytes_per_pixel(dmabuf.format);
    if !known_format {
      tracing::warn!(
        format = %fourcc_string(dmabuf.format),
        "unsupported dmabuf format, assuming 4 bytes per pixel"
      );
    }

    let vk_format =
      vk_format_from_drm(dmabuf.format).ok_or_else(|| ComputeError::UnsupportedFormat(fourcc_string(dmabuf.format)))?;

    let plane_count = usize::try_from(dmabuf.num_planes).map_err(|_| ComputeError::MissingPlaneMetadata)?;
    let plane_count = plane_count.min(dmabuf.planes.len());
    if plane_count == 0 {
      return Err(ComputeError::MissingPlaneMetadata);
    }

    let available_bytes = dmabuf_byte_len(dmabuf.fd.as_fd())?;
    if available_bytes == 0 {
      return Err(ComputeError::EmptyDmabuf);
    }

    let mut plane_layouts = Vec::with_capacity(plane_count);
    let mut max_plane_end: vk::DeviceSize = 0;
    for plane in dmabuf.planes.iter().take(plane_count) {
      let row_pitch = vk::DeviceSize::from(plane.stride);
      if row_pitch == 0 {
        continue;
      }
      let height = vk::DeviceSize::from(dmabuf.height);
      let plane_size = row_pitch * height;
      let offset = vk::DeviceSize::from(plane.offset);
      let layout = vk::SubresourceLayout {
        offset,
        size: plane_size,
        row_pitch,
        array_pitch: 0,
        depth_pitch: 0,
      };
      let plane_end = offset.saturating_add(plane_size);
      max_plane_end = max_plane_end.max(plane_end);
      plane_layouts.push(layout);
    }

    if plane_layouts.is_empty() {
      return Err(ComputeError::EmptyDmabuf);
    }

    if max_plane_end > available_bytes {
      tracing::warn!(
        plane_extent = max_plane_end,
        available = available_bytes,
        "plane layouts exceed reported dma-buf size; continuing with fd size"
      );
    }

    let external_info = vk::ExternalMemoryImageCreateInfo {
      handle_types: vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
      ..Default::default()
    };

    let plane_layouts = plane_layouts;
    let modifier_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT {
      drm_format_modifier: dmabuf.modifier,
      drm_format_modifier_plane_count: plane_layouts.len() as u32,
      p_plane_layouts: plane_layouts.as_ptr(),
      p_next: &external_info as *const _ as *const std::ffi::c_void,
      ..Default::default()
    };

    let image_info = vk::ImageCreateInfo {
      s_type: vk::StructureType::IMAGE_CREATE_INFO,
      p_next: &modifier_info as *const _ as *const std::ffi::c_void,
      image_type: vk::ImageType::TYPE_2D,
      format: vk_format,
      extent: vk::Extent3D {
        width: dmabuf.width,
        height: dmabuf.height,
        depth: 1,
      },
      mip_levels: 1,
      array_layers: 1,
      samples: vk::SampleCountFlags::TYPE_1,
      tiling: vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT,
      usage: vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_SRC,
      sharing_mode: vk::SharingMode::EXCLUSIVE,
      initial_layout: vk::ImageLayout::UNDEFINED,
      ..Default::default()
    };
    let image = unsafe { self.device.create_image(&image_info, None)? };
    let requirements = unsafe { self.device.get_image_memory_requirements(image) };
    if requirements.size > available_bytes {
      unsafe {
        self.device.destroy_image(image, None);
      }
      return Err(ComputeError::DmabufTooSmall {
        required: requirements.size,
        available: available_bytes,
      });
    }

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
    let dedicated_info = vk::MemoryDedicatedAllocateInfo {
      s_type: vk::StructureType::MEMORY_DEDICATED_ALLOCATE_INFO,
      p_next: &dmabuf_info as *const _ as *const std::ffi::c_void,
      image,
      buffer: vk::Buffer::null(),
      _marker: std::marker::PhantomData,
    };
    let alloc_info = vk::MemoryAllocateInfo {
      allocation_size: requirements.size,
      memory_type_index: memory_type,
      p_next: &dedicated_info as *const _ as *const std::ffi::c_void,
      ..Default::default()
    };
    let memory = match unsafe { self.device.allocate_memory(&alloc_info, None) } {
      Ok(memory) => memory,
      Err(err) => {
        unsafe {
          let _ = OwnedFd::from_raw_fd(fd);
          self.device.destroy_image(image, None);
        }
        return Err(err.into());
      }
    };

    let bind_info = [vk::BindImageMemoryInfo {
      s_type: vk::StructureType::BIND_IMAGE_MEMORY_INFO,
      p_next: ptr::null(),
      image,
      memory,
      memory_offset: 0,
      ..Default::default()
    }];
    if let Err(err) = unsafe { self.device.bind_image_memory2(&bind_info) } {
      unsafe {
        self.device.destroy_image(image, None);
        self.device.free_memory(memory, None);
      }
      return Err(err.into());
    }

    let subresource_range = vk::ImageSubresourceRange {
      aspect_mask: vk::ImageAspectFlags::COLOR,
      base_mip_level: 0,
      level_count: 1,
      base_array_layer: 0,
      layer_count: 1,
    };

    if let Err(err) = self.prepare_imported_image(image, subresource_range) {
      unsafe {
        self.device.destroy_image(image, None);
        self.device.free_memory(memory, None);
      }
      return Err(err.into());
    }

    let view_info = vk::ImageViewCreateInfo {
      image,
      view_type: vk::ImageViewType::TYPE_2D,
      format: vk_format,
      components: vk::ComponentMapping::default(),
      subresource_range,
      ..Default::default()
    };
    let image_view = match unsafe { self.device.create_image_view(&view_info, None) } {
      Ok(view) => view,
      Err(err) => {
        unsafe {
          self.device.destroy_image(image, None);
          self.device.free_memory(memory, None);
        }
        return Err(err.into());
      }
    };

    self.shaders.bind_input_image(&self.device, image_view);

    let info = ImageInfo {
      width: dmabuf.width,
      height: dmabuf.height,
      stride: dmabuf.stride,
      bytes_per_pixel,
    };

    Ok(MappedDmabuf {
      memory,
      image,
      image_view,
      subresource_range,
      info,
    })
  }

  #[cfg(test)]
  pub(crate) fn copy_screen_to_host(&self) -> Result<Vec<u8>, ComputeError> {
    self.wait_for_idle();
    let Some(dmabuf) = &self.screen_dmabuf else {
      return Err(ComputeError::NoDmabuf);
    };

    let copy_size = vk::DeviceSize::from(dmabuf.info.stride) * vk::DeviceSize::from(dmabuf.info.height);
    if copy_size == 0 {
      return Err(ComputeError::EmptyDmabuf);
    }

    let buffer_info = vk::BufferCreateInfo {
      size: copy_size,
      usage: vk::BufferUsageFlags::TRANSFER_DST,
      sharing_mode: vk::SharingMode::EXCLUSIVE,
      ..Default::default()
    };
    let staging_buffer = unsafe { self.device.create_buffer(&buffer_info, None)? };
    let requirements = unsafe { self.device.get_buffer_memory_requirements(staging_buffer) };
    let memory_type = self
      .find_memory_type(
        requirements.memory_type_bits,
        vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
      )
      .ok_or(ComputeError::NoHostVisibleMemory)?;
    let alloc_info = vk::MemoryAllocateInfo {
      allocation_size: requirements.size,
      memory_type_index: memory_type,
      ..Default::default()
    };
    let staging_memory = unsafe { self.device.allocate_memory(&alloc_info, None)? };
    unsafe { self.device.bind_buffer_memory(staging_buffer, staging_memory, 0)? };

    let copy_result: Result<(), vk::Result> = unsafe {
      self.device.reset_fences(&[self.fence])?;
      self
        .device
        .reset_command_pool(self.command_pool, vk::CommandPoolResetFlags::empty())?;

      let begin_info = vk::CommandBufferBeginInfo::default();
      self.device.begin_command_buffer(self.command_buffer, &begin_info)?;

      let acquire_barrier = vk::ImageMemoryBarrier {
        src_access_mask: vk::AccessFlags::MEMORY_WRITE,
        dst_access_mask: vk::AccessFlags::TRANSFER_READ,
        old_layout: vk::ImageLayout::GENERAL,
        new_layout: vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
        src_queue_family_index: vk::QUEUE_FAMILY_EXTERNAL,
        dst_queue_family_index: self.queue_family_index,
        image: dmabuf.image,
        subresource_range: dmabuf.subresource_range,
        ..Default::default()
      };
      self.device.cmd_pipeline_barrier(
        self.command_buffer,
        vk::PipelineStageFlags::ALL_COMMANDS,
        vk::PipelineStageFlags::TRANSFER,
        vk::DependencyFlags::empty(),
        &[],
        &[],
        &[acquire_barrier],
      );

      let copy_region = vk::BufferImageCopy {
        buffer_offset: 0,
        buffer_row_length: 0,
        buffer_image_height: 0,
        image_subresource: vk::ImageSubresourceLayers {
          aspect_mask: vk::ImageAspectFlags::COLOR,
          mip_level: 0,
          base_array_layer: 0,
          layer_count: 1,
        },
        image_offset: vk::Offset3D { x: 0, y: 0, z: 0 },
        image_extent: vk::Extent3D {
          width: dmabuf.info.width,
          height: dmabuf.info.height,
          depth: 1,
        },
      };
      self.device.cmd_copy_image_to_buffer(
        self.command_buffer,
        dmabuf.image,
        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
        staging_buffer,
        &[copy_region],
      );

      let release_barrier = vk::ImageMemoryBarrier {
        src_access_mask: vk::AccessFlags::TRANSFER_READ,
        dst_access_mask: vk::AccessFlags::MEMORY_WRITE,
        old_layout: vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
        new_layout: vk::ImageLayout::GENERAL,
        src_queue_family_index: self.queue_family_index,
        dst_queue_family_index: vk::QUEUE_FAMILY_EXTERNAL,
        image: dmabuf.image,
        subresource_range: dmabuf.subresource_range,
        ..Default::default()
      };
      self.device.cmd_pipeline_barrier(
        self.command_buffer,
        vk::PipelineStageFlags::TRANSFER,
        vk::PipelineStageFlags::ALL_COMMANDS,
        vk::DependencyFlags::empty(),
        &[],
        &[],
        &[release_barrier],
      );

      self.device.end_command_buffer(self.command_buffer)?;

      let submit_info = vk::SubmitInfo {
        command_buffer_count: 1,
        p_command_buffers: &self.command_buffer,
        ..Default::default()
      };
      self.device.queue_submit(self.queue, &[submit_info], self.fence)?;
      self.device.wait_for_fences(&[self.fence], true, u64::MAX)?;
      self
        .device
        .reset_command_pool(self.command_pool, vk::CommandPoolResetFlags::empty())?;
      Ok(())
    };

    let copy_status = copy_result;

    let mut output = Vec::new();
    if copy_status.is_ok() {
      unsafe {
        let mapped = self
          .device
          .map_memory(staging_memory, 0, copy_size, vk::MemoryMapFlags::empty())?;
        let slice = std::slice::from_raw_parts(mapped as *const u8, copy_size as usize);
        output.extend_from_slice(slice);
        self.device.unmap_memory(staging_memory);
      }
    }

    unsafe {
      self.device.destroy_buffer(staging_buffer, None);
      self.device.free_memory(staging_memory, None);
    }

    copy_status?;
    Ok(output)
  }

  #[tracing::instrument(level = "trace", skip(self))]
  fn release_screen_dmabuf(&mut self) {
    if let Some(imported) = self.screen_dmabuf.take() {
      tracing::trace!(
        width = imported.info.width,
        height = imported.info.height,
        "releasing imported dma-buf"
      );
      unsafe {
        self.device.destroy_image_view(imported.image_view, None);
        self.device.destroy_image(imported.image, None);
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

  fn prepare_imported_image(
    &self,
    image: vk::Image,
    subresource_range: vk::ImageSubresourceRange,
  ) -> Result<(), vk::Result> {
    unsafe {
      self
        .device
        .reset_command_pool(self.command_pool, vk::CommandPoolResetFlags::empty())?;
      let begin_info = vk::CommandBufferBeginInfo::default();
      self.device.begin_command_buffer(self.command_buffer, &begin_info)?;

      let layout_barrier = vk::ImageMemoryBarrier {
        src_access_mask: vk::AccessFlags::empty(),
        dst_access_mask: vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE,
        old_layout: vk::ImageLayout::UNDEFINED,
        new_layout: vk::ImageLayout::GENERAL,
        src_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
        dst_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
        image,
        subresource_range,
        ..Default::default()
      };
      self.device.cmd_pipeline_barrier(
        self.command_buffer,
        vk::PipelineStageFlags::TOP_OF_PIPE,
        vk::PipelineStageFlags::ALL_COMMANDS,
        vk::DependencyFlags::empty(),
        &[],
        &[],
        &[layout_barrier],
      );

      let release_barrier = vk::ImageMemoryBarrier {
        src_access_mask: vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE,
        dst_access_mask: vk::AccessFlags::MEMORY_WRITE,
        old_layout: vk::ImageLayout::GENERAL,
        new_layout: vk::ImageLayout::GENERAL,
        src_queue_family_index: self.queue_family_index,
        dst_queue_family_index: vk::QUEUE_FAMILY_EXTERNAL,
        image,
        subresource_range,
        ..Default::default()
      };
      self.device.cmd_pipeline_barrier(
        self.command_buffer,
        vk::PipelineStageFlags::ALL_COMMANDS,
        vk::PipelineStageFlags::ALL_COMMANDS,
        vk::DependencyFlags::empty(),
        &[],
        &[],
        &[release_barrier],
      );

      self.device.end_command_buffer(self.command_buffer)?;
      let submit_info = vk::SubmitInfo {
        command_buffer_count: 1,
        p_command_buffers: &self.command_buffer,
        ..Default::default()
      };
      self
        .device
        .queue_submit(self.queue, &[submit_info], vk::Fence::null())?;
      self.device.queue_wait_idle(self.queue)?;
      self
        .device
        .reset_command_pool(self.command_pool, vk::CommandPoolResetFlags::empty())?;
    }
    Ok(())
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

fn vk_format_from_drm(format: u32) -> Option<vk::Format> {
  match format {
    DRM_FORMAT_ARGB8888 | DRM_FORMAT_XRGB8888 => Some(vk::Format::B8G8R8A8_UNORM),
    DRM_FORMAT_ABGR8888 | DRM_FORMAT_XBGR8888 => Some(vk::Format::R8G8B8A8_UNORM),
    _ => None,
  }
}

fn dmabuf_byte_len(fd: BorrowedFd<'_>) -> Result<vk::DeviceSize, ComputeError> {
  let len = unsafe { libc::lseek(fd.as_raw_fd(), 0, libc::SEEK_END) };
  if len < 0 {
    return Err(ComputeError::DmabufSize(io::Error::last_os_error()));
  }
  Ok(len as vk::DeviceSize)
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
  #[error("failed to query DMA-BUF size: {0}")]
  DmabufSize(#[source] io::Error),
  #[error("missing DMA-BUF plane metadata")]
  MissingPlaneMetadata,
  #[error("DMA-BUF smaller than image requirements (need {required} bytes, have {available} bytes)")]
  DmabufTooSmall {
    required: vk::DeviceSize,
    available: vk::DeviceSize,
  },
  #[error("command buffer already in flight")]
  DispatchInFlight,
  #[error("required device feature not supported: {0}")]
  MissingFeature(&'static str),
  #[error("unsupported DRM format: {0}")]
  UnsupportedFormat(String),
}
