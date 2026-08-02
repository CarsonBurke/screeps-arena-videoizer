//! Exportable Vulkan backing for a packed NV12 render target.
//!
//! NV12 bytes are represented as one `R8Unorm` image whose upper `height`
//! rows contain Y and whose lower `height / 2` rows contain interleaved UV.
//! The dedicated Vulkan allocation is exportable as `OPAQUE_FD` and the image
//! is wrapped into the active Vulkan `wgpu` device as a render attachment.

use std::os::fd::{AsFd, BorrowedFd, FromRawFd, OwnedFd};
use std::time::Duration;

use ash::vk;

const TEXTURE_LABEL: &str = "screeps-arena-exportable-nv12";

/// Failure to create an exportable Vulkan NV12 render target.
#[derive(Debug, thiserror::Error)]
pub enum VulkanExternalNv12Error {
    /// The requested dimensions cannot represent a packed 4:2:0 frame.
    #[error("NV12 dimensions must be nonzero and even, got {width}x{height}")]
    InvalidDimensions { width: u32, height: u32 },
    /// The packed image height overflowed `u32`.
    #[error("packed NV12 image height overflows u32 for frame height {height}")]
    PackedHeightOverflow { height: u32 },
    /// The supplied `wgpu` device does not use the Vulkan backend.
    #[error("wgpu device is not using the Vulkan backend")]
    UnsupportedBackend,
    /// wgpu did not enable the external-memory extension on this device.
    #[error("VK_KHR_external_memory_fd is not enabled on the Vulkan device")]
    ExternalMemoryExtensionUnavailable,
    /// No compatible device-local Vulkan memory type exists.
    #[error("no DEVICE_LOCAL memory type is compatible with the exportable NV12 image")]
    DeviceLocalMemoryUnavailable,
    /// A raw Vulkan operation failed.
    #[error("{operation} failed: {result:?}")]
    Vulkan {
        operation: &'static str,
        result: vk::Result,
    },
}

/// A packed NV12 render target backed by dedicated exportable Vulkan memory.
///
/// The texture owns the Vulkan image and memory through wgpu's HAL drop
/// callback. The exported descriptor is independently owned and may be cloned
/// for a consuming API. CUDA's opaque-FD import consumes its descriptor, so
/// callers should pass [`Self::try_clone_opaque_fd`] rather than the borrowed
/// descriptor retained here.
#[derive(Debug)]
pub struct VulkanExternalNv12 {
    // Views must be released before the texture invokes the raw-image callback.
    view: wgpu::TextureView,
    texture: wgpu::Texture,
    opaque_fd: OwnedFd,
    allocation_size: u64,
    device_uuid: [u8; vk::UUID_SIZE],
    width: u32,
    height: u32,
    packed_height: u32,
}

impl VulkanExternalNv12 {
    /// Allocate and export a packed NV12 render target on `device`.
    pub fn create(
        device: &wgpu::Device,
        width: u32,
        height: u32,
    ) -> Result<Self, VulkanExternalNv12Error> {
        validate_dimensions(width, height)?;
        let packed_height = height
            .checked_add(height / 2)
            .ok_or(VulkanExternalNv12Error::PackedHeightOverflow { height })?;

        let device_hal = unsafe {
            device
                .as_hal::<wgpu::hal::api::Vulkan>()
                .ok_or(VulkanExternalNv12Error::UnsupportedBackend)?
        };
        if !device_hal
            .enabled_device_extensions()
            .contains(&ash::khr::external_memory_fd::NAME)
        {
            return Err(VulkanExternalNv12Error::ExternalMemoryExtensionUnavailable);
        }
        let raw_device = device_hal.raw_device();
        let raw_instance = device_hal.shared_instance().raw_instance();
        let mut id_properties = vk::PhysicalDeviceIDProperties::default();
        let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut id_properties);
        unsafe {
            raw_instance
                .get_physical_device_properties2(device_hal.raw_physical_device(), &mut properties);
        }
        let handle_type = vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD;

        let mut external_image =
            vk::ExternalMemoryImageCreateInfo::default().handle_types(handle_type);
        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::R8_UNORM)
            .extent(vk::Extent3D {
                width,
                height: packed_height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(
                vk::ImageUsageFlags::COLOR_ATTACHMENT
                    | vk::ImageUsageFlags::TRANSFER_SRC
                    | vk::ImageUsageFlags::TRANSFER_DST
                    | vk::ImageUsageFlags::SAMPLED,
            )
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut external_image);

        let image = unsafe { raw_device.create_image(&image_info, None) }
            .map_err(|result| vk_error("vkCreateImage", result))?;
        let requirements = unsafe { raw_device.get_image_memory_requirements(image) };
        let memory_type_index = match find_device_local_memory_type(
            raw_instance,
            device_hal.raw_physical_device(),
            requirements.memory_type_bits,
        ) {
            Some(index) => index,
            None => {
                unsafe { raw_device.destroy_image(image, None) };
                return Err(VulkanExternalNv12Error::DeviceLocalMemoryUnavailable);
            }
        };

        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let mut export = vk::ExportMemoryAllocateInfo::default().handle_types(handle_type);
        let allocation_info = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(memory_type_index)
            .push_next(&mut dedicated)
            .push_next(&mut export);
        let memory = match unsafe { raw_device.allocate_memory(&allocation_info, None) } {
            Ok(memory) => memory,
            Err(result) => {
                unsafe { raw_device.destroy_image(image, None) };
                return Err(vk_error("vkAllocateMemory", result));
            }
        };
        if let Err(result) = unsafe { raw_device.bind_image_memory(image, memory, 0) } {
            unsafe {
                raw_device.free_memory(memory, None);
                raw_device.destroy_image(image, None);
            }
            return Err(vk_error("vkBindImageMemory", result));
        }

        let external_memory = ash::khr::external_memory_fd::Device::new(raw_instance, raw_device);
        let fd_info = vk::MemoryGetFdInfoKHR::default()
            .memory(memory)
            .handle_type(handle_type);
        let raw_fd = match unsafe { external_memory.get_memory_fd(&fd_info) } {
            Ok(fd) => fd,
            Err(result) => {
                unsafe {
                    raw_device.free_memory(memory, None);
                    raw_device.destroy_image(image, None);
                }
                return Err(vk_error("vkGetMemoryFdKHR", result));
            }
        };
        let opaque_fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };

        let hal_descriptor = wgpu::hal::TextureDescriptor {
            label: Some(TEXTURE_LABEL),
            size: wgpu::Extent3d {
                width,
                height: packed_height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUses::COLOR_TARGET
                | wgpu::TextureUses::COPY_SRC
                | wgpu::TextureUses::COPY_DST
                | wgpu::TextureUses::RESOURCE,
            memory_flags: wgpu::hal::MemoryFlags::empty(),
            view_formats: Vec::new(),
        };
        let drop_device = raw_device.clone();
        let drop_callback: wgpu::hal::DropCallback = Box::new(move || unsafe {
            drop_device.destroy_image(image, None);
            drop_device.free_memory(memory, None);
        });
        let hal_texture =
            unsafe { device_hal.texture_from_raw(image, &hal_descriptor, Some(drop_callback)) };

        let texture_descriptor = wgpu::TextureDescriptor {
            label: Some(TEXTURE_LABEL),
            size: hal_descriptor.size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        };
        let texture = unsafe {
            device
                .create_texture_from_hal::<wgpu::hal::api::Vulkan>(hal_texture, &texture_descriptor)
        };
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        Ok(Self {
            view,
            texture,
            opaque_fd,
            allocation_size: requirements.size,
            device_uuid: id_properties.device_uuid,
            width,
            height,
            packed_height,
        })
    }

    /// The logical NV12 frame width in pixels/bytes.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// The logical NV12 frame height before the chroma rows are appended.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Physical texture height: `height + height / 2` rows.
    pub fn packed_height(&self) -> u32 {
        self.packed_height
    }

    /// Size of the dedicated Vulkan allocation exported by the FD.
    pub fn allocation_size(&self) -> u64 {
        self.allocation_size
    }

    /// Vulkan physical-device UUID used to reject cross-device CUDA imports.
    pub fn device_uuid(&self) -> [u8; vk::UUID_SIZE] {
        self.device_uuid
    }

    /// Borrow the exportable texture.
    pub fn texture(&self) -> &wgpu::Texture {
        &self.texture
    }

    /// Borrow the full packed-image render attachment view.
    pub fn view(&self) -> &wgpu::TextureView {
        &self.view
    }

    /// Borrow the retained Vulkan opaque-memory descriptor.
    pub fn opaque_fd(&self) -> BorrowedFd<'_> {
        self.opaque_fd.as_fd()
    }

    /// Clone the descriptor for an importing API that consumes FD ownership.
    pub fn try_clone_opaque_fd(&self) -> std::io::Result<OwnedFd> {
        self.opaque_fd.try_clone()
    }

    /// Wait until all work submitted to this wgpu device has completed.
    ///
    /// This is the current host-serialized handoff before CUDA access. Portable
    /// cross-API visibility additionally requires an exported semaphore, which
    /// the wgpu-created Vulkan device does not currently enable.
    pub fn wait_for_gpu(device: &wgpu::Device) -> Result<(), wgpu::PollError> {
        device.poll(wgpu::PollType::wait_indefinitely()).map(|_| ())
    }

    /// Wait for one Vulkan submission while allowing later submissions that
    /// use disjoint in-flight targets to continue on the GPU.
    pub fn wait_for_submission(
        device: &wgpu::Device,
        submission_index: wgpu::SubmissionIndex,
    ) -> Result<(), wgpu::PollError> {
        device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(submission_index),
                timeout: Some(Duration::from_secs(120)),
            })
            .map(|_| ())
    }
}

fn validate_dimensions(width: u32, height: u32) -> Result<(), VulkanExternalNv12Error> {
    if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err(VulkanExternalNv12Error::InvalidDimensions { width, height });
    }
    Ok(())
}

fn find_device_local_memory_type(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
    compatible_types: u32,
) -> Option<u32> {
    let properties = unsafe { instance.get_physical_device_memory_properties(physical_device) };
    (0..properties.memory_type_count).find(|index| {
        compatible_types & (1 << index) != 0
            && properties.memory_types[*index as usize]
                .property_flags
                .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
    })
}

fn vk_error(operation: &'static str, result: vk::Result) -> VulkanExternalNv12Error {
    VulkanExternalNv12Error::Vulkan { operation, result }
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        pin::pin,
        sync::Arc,
        task::{Context, Poll, Wake, Waker},
        thread,
    };

    use super::{VulkanExternalNv12, VulkanExternalNv12Error, validate_dimensions};

    #[test]
    fn accepts_even_nv12_dimensions() {
        validate_dimensions(2_048, 2_048).unwrap();
    }

    #[test]
    fn rejects_zero_or_odd_nv12_dimensions() {
        for (width, height) in [(0, 2), (2, 0), (1, 2), (2, 1)] {
            assert!(matches!(
                validate_dimensions(width, height),
                Err(VulkanExternalNv12Error::InvalidDimensions { .. })
            ));
        }
    }

    #[test]
    #[ignore = "requires a Vulkan device with VK_KHR_external_memory_fd"]
    fn creates_real_exportable_render_attachment() {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..Default::default()
        });
        let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .unwrap();
        let (device, _queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("external NV12 smoke-test device"),
            required_features: wgpu::Features::empty(),
            required_limits: adapter.limits(),
            ..Default::default()
        }))
        .unwrap();

        let target = VulkanExternalNv12::create(&device, 2_048, 2_048).unwrap();
        assert_eq!(target.texture().format(), wgpu::TextureFormat::R8Unorm);
        assert_eq!(target.texture().size().width, 2_048);
        assert_eq!(target.texture().size().height, 3_072);
        assert!(target.allocation_size() >= 2_048 * 3_072);
        target.try_clone_opaque_fd().unwrap();
        VulkanExternalNv12::wait_for_gpu(&device).unwrap();
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        struct ThreadWaker(thread::Thread);

        impl Wake for ThreadWaker {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }

            fn wake_by_ref(self: &Arc<Self>) {
                self.0.unpark();
            }
        }

        let waker = Waker::from(Arc::new(ThreadWaker(thread::current())));
        let mut context = Context::from_waker(&waker);
        let mut future = pin!(future);
        loop {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(output) => return output,
                Poll::Pending => thread::park(),
            }
        }
    }
}
