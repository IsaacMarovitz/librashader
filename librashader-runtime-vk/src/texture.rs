use std::sync::Arc;
use crate::filter_chain::VulkanObjects;
use crate::util::find_vulkan_memory_type;
use crate::vulkan_primitives::VulkanImageMemory;
use crate::{error, util};
use ash::vk;

use librashader_common::{FilterMode, ImageFormat, Size, WrapMode};
use librashader_presets::Scale2D;
use librashader_runtime::scaling::{MipmapSize, ViewportSize};

pub struct OwnedImage {
    pub device: Arc<ash::Device>,
    pub mem_props: vk::PhysicalDeviceMemoryProperties,
    pub image_view: vk::ImageView,
    pub image: VulkanImage,
    pub memory: VulkanImageMemory,
    pub max_miplevels: u32,
    pub levels: u32,
}

pub struct OwnedImageLayout {
    pub(crate) dst_layout: vk::ImageLayout,
    pub(crate) dst_access: vk::AccessFlags,
    pub(crate) src_stage: vk::PipelineStageFlags,
    pub(crate) dst_stage: vk::PipelineStageFlags,
    pub(crate) cmd: vk::CommandBuffer,
}

impl OwnedImage {
    fn new_internal(
        device: Arc<ash::Device>,
        mem_props: vk::PhysicalDeviceMemoryProperties,
        size: Size<u32>,
        mut format: ImageFormat,
        max_miplevels: u32,
    ) -> error::Result<OwnedImage> {
        // default to something sane
        if format == ImageFormat::Unknown {
            format = ImageFormat::R8G8B8A8Unorm
        }
        let image_create_info = vk::ImageCreateInfo::builder()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format.into())
            .extent(size.into())
            .mip_levels(std::cmp::min(max_miplevels, size.calculate_miplevels()))
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .flags(vk::ImageCreateFlags::MUTABLE_FORMAT)
            .usage(
                vk::ImageUsageFlags::SAMPLED
                    | vk::ImageUsageFlags::COLOR_ATTACHMENT
                    | vk::ImageUsageFlags::TRANSFER_DST
                    | vk::ImageUsageFlags::TRANSFER_SRC,
            )
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .build();

        let image = unsafe { device.create_image(&image_create_info, None)? };
        let mem_reqs = unsafe { device.get_image_memory_requirements(image.clone()) };

        let alloc_info = vk::MemoryAllocateInfo::builder()
            .allocation_size(mem_reqs.size)
            .memory_type_index(find_vulkan_memory_type(
                &mem_props,
                mem_reqs.memory_type_bits,
                vk::MemoryPropertyFlags::DEVICE_LOCAL,
            )?)
            .build();

        // todo: optimize by reusing existing memory.
        let memory = VulkanImageMemory::new(&device, &alloc_info)?;
        memory.bind(&image)?;

        let image_subresource = vk::ImageSubresourceRange::builder()
            .base_mip_level(0)
            .base_array_layer(0)
            .level_count(image_create_info.mip_levels)
            .layer_count(1)
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .build();

        let swizzle_components = vk::ComponentMapping::builder()
            .r(vk::ComponentSwizzle::R)
            .g(vk::ComponentSwizzle::G)
            .b(vk::ComponentSwizzle::B)
            .a(vk::ComponentSwizzle::A)
            .build();

        let view_info = vk::ImageViewCreateInfo::builder()
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format.into())
            .image(image.clone())
            .subresource_range(image_subresource)
            .components(swizzle_components)
            .build();

        let image_view = unsafe { device.create_image_view(&view_info, None)? };

        Ok(OwnedImage {
            device,
            mem_props,
            image_view,
            image: VulkanImage {
                image,
                size,
                format: format.into(),
            },
            memory,
            max_miplevels,
            levels: std::cmp::min(max_miplevels, size.calculate_miplevels()),
        })
    }

    pub fn new(
        vulkan: &VulkanObjects,
        size: Size<u32>,
        format: ImageFormat,
        max_miplevels: u32,
    ) -> error::Result<OwnedImage> {
        Self::new_internal(
            vulkan.device.clone(),
            vulkan.memory_properties,
            size,
            format,
            max_miplevels,
        )
    }

    pub(crate) fn scale(
        &mut self,
        scaling: Scale2D,
        format: ImageFormat,
        viewport_size: &Size<u32>,
        _original: &InputImage,
        source: &InputImage,
        mipmap: bool,
        layout: Option<OwnedImageLayout>,
    ) -> error::Result<Size<u32>> {
        let size = source.image.size.scale_viewport(scaling, *viewport_size);
        if self.image.size != size
            || (mipmap && self.max_miplevels == 1)
            || (!mipmap && self.max_miplevels != 1)
        {
            let max_levels = if mipmap { u32::MAX } else { 1 };

            let new = OwnedImage::new_internal(
                self.device.clone(),
                self.mem_props,
                size,
                if format == ImageFormat::Unknown {
                    ImageFormat::R8G8B8A8Unorm
                } else {
                    format
                },
                max_levels,
            )?;

            let old = std::mem::replace(self, new);
            drop(old);

            if let Some(layout) = layout {
                unsafe {
                    util::vulkan_image_layout_transition_levels(
                        &self.device,
                        layout.cmd,
                        self.image.image,
                        self.levels,
                        vk::ImageLayout::UNDEFINED,
                        layout.dst_layout,
                        vk::AccessFlags::empty(),
                        layout.dst_access,
                        layout.src_stage,
                        layout.dst_stage,
                        vk::QUEUE_FAMILY_IGNORED,
                        vk::QUEUE_FAMILY_IGNORED,
                    )
                }
            }
        }
        Ok(size)
    }

    pub fn as_input(&self, filter: FilterMode, wrap_mode: WrapMode) -> InputImage {
        InputImage {
            image: self.image.clone(),
            image_view: self.image_view.clone(),
            wrap_mode,
            filter_mode: filter,
            mip_filter: filter,
        }
    }

    pub fn generate_mipmaps_and_end_pass(&self, cmd: vk::CommandBuffer) {
        let input_barrier = vk::ImageMemoryBarrier::builder()
            .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
            .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
            .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(self.image.image)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: vk::REMAINING_ARRAY_LAYERS,
            })
            .build();

        let mipchain_barrier = vk::ImageMemoryBarrier::builder()
            .src_access_mask(vk::AccessFlags::empty())
            .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(self.image.image)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 1,
                base_array_layer: 0,
                level_count: vk::REMAINING_MIP_LEVELS,
                layer_count: vk::REMAINING_ARRAY_LAYERS,
            })
            .build();

        unsafe {
            self.device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::ALL_GRAPHICS,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[input_barrier, mipchain_barrier],
            );

            for level in 1..self.levels {
                // need to transition from DST to SRC, one level at a time.
                if level > 1 {
                    let next_barrier = vk::ImageMemoryBarrier::builder()
                        .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                        .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
                        .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                        .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .image(self.image.image)
                        .subresource_range(vk::ImageSubresourceRange {
                            aspect_mask: vk::ImageAspectFlags::COLOR,
                            base_mip_level: level - 1,
                            base_array_layer: 0,
                            level_count: 1,
                            layer_count: vk::REMAINING_ARRAY_LAYERS,
                        })
                        .build();

                    self.device.cmd_pipeline_barrier(
                        cmd,
                        vk::PipelineStageFlags::TRANSFER,
                        vk::PipelineStageFlags::TRANSFER,
                        vk::DependencyFlags::empty(),
                        &[],
                        &[],
                        &[next_barrier],
                    );
                }

                let source_size = self.image.size.scale_mipmap(level - 1);
                let target_size = self.image.size.scale_mipmap(level);

                let src_offsets = [
                    vk::Offset3D { x: 0, y: 0, z: 0 },
                    vk::Offset3D {
                        x: source_size.width as i32,
                        y: source_size.height as i32,
                        z: 1,
                    },
                ];

                let dst_offsets = [
                    vk::Offset3D { x: 0, y: 0, z: 0 },
                    vk::Offset3D {
                        x: target_size.width as i32,
                        y: target_size.height as i32,
                        z: 1,
                    },
                ];

                let src_subresource = vk::ImageSubresourceLayers::builder()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .mip_level(level - 1)
                    .base_array_layer(0)
                    .layer_count(1)
                    .build();

                let dst_subresource = vk::ImageSubresourceLayers::builder()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .mip_level(level)
                    .base_array_layer(0)
                    .layer_count(1)
                    .build();

                let image_blit = [vk::ImageBlit::builder()
                    .src_subresource(src_subresource)
                    .src_offsets(src_offsets)
                    .dst_subresource(dst_subresource)
                    .dst_offsets(dst_offsets)
                    .build()];

                self.device.cmd_blit_image(
                    cmd,
                    self.image.image,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    self.image.image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &image_blit,
                    vk::Filter::LINEAR,
                );
            }

            // move everything to SHADER_READ_ONLY_OPTIMAL

            let input_barrier = vk::ImageMemoryBarrier::builder()
                .src_access_mask(vk::AccessFlags::TRANSFER_READ)
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(self.image.image)
                .subresource_range(vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: self.levels - 1,
                    base_array_layer: 0,
                    layer_count: vk::REMAINING_ARRAY_LAYERS,
                })
                .build();

            let mipchain_barrier = vk::ImageMemoryBarrier::builder()
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(self.image.image)
                .subresource_range(vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: self.levels - 1,
                    base_array_layer: 0,
                    level_count: 1,
                    layer_count: vk::REMAINING_ARRAY_LAYERS,
                })
                .build();

            // next past waits for ALL_GRAPHICS, use dependency chain and FRAGMENT_SHADER dst stage
            // to ensure that next pass doesn't start until mipchain is complete.
            self.device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[input_barrier, mipchain_barrier],
            );
        }
    }

    /// SAFETY: self must fit the source image
    pub unsafe fn copy_from(
        &self,
        cmd: vk::CommandBuffer,
        source: &VulkanImage,
        source_layout: vk::ImageLayout,
    ) {
        let region = vk::ImageCopy::builder()
            .src_subresource(
                vk::ImageSubresourceLayers::builder()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .mip_level(0)
                    .base_array_layer(0)
                    .layer_count(1)
                    .build(),
            )
            .dst_subresource(
                vk::ImageSubresourceLayers::builder()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .mip_level(0)
                    .base_array_layer(0)
                    .layer_count(1)
                    .build(),
            )
            .src_offset(Default::default())
            .dst_offset(Default::default())
            .extent(source.size.into())
            .build();

        unsafe {
            util::vulkan_image_layout_transition_levels(
                &self.device,
                cmd,
                self.image.image,
                vk::REMAINING_MIP_LEVELS,
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::AccessFlags::empty(),
                vk::AccessFlags::TRANSFER_WRITE,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::PipelineStageFlags::TRANSFER,
                vk::QUEUE_FAMILY_IGNORED,
                vk::QUEUE_FAMILY_IGNORED,
            );

            self.device.cmd_copy_image(
                cmd,
                source.image,
                source_layout,
                self.image.image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[region],
            );
            util::vulkan_image_layout_transition_levels(
                &self.device,
                cmd,
                self.image.image,
                vk::REMAINING_MIP_LEVELS,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                vk::AccessFlags::TRANSFER_WRITE,
                vk::AccessFlags::SHADER_READ,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::QUEUE_FAMILY_IGNORED,
                vk::QUEUE_FAMILY_IGNORED,
            );
        }
    }

    pub fn clear(&self, cmd: vk::CommandBuffer) {
        unsafe {
            util::vulkan_image_layout_transition_levels(
                &self.device,
                cmd,
                self.image.image,
                vk::REMAINING_MIP_LEVELS,
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::AccessFlags::empty(),
                vk::AccessFlags::TRANSFER_WRITE,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::QUEUE_FAMILY_IGNORED,
                vk::QUEUE_FAMILY_IGNORED,
            );
            self.device.cmd_clear_color_image(
                cmd,
                self.image.image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &vk::ClearColorValue {
                    float32: [0.0, 0.0, 0.0, 0.0],
                },
                &[vk::ImageSubresourceRange::builder()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .base_mip_level(0)
                    .level_count(1)
                    .base_array_layer(0)
                    .layer_count(1)
                    .build()],
            );

            util::vulkan_image_layout_transition_levels(
                &self.device,
                cmd,
                self.image.image,
                vk::REMAINING_MIP_LEVELS,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                vk::AccessFlags::TRANSFER_WRITE,
                vk::AccessFlags::SHADER_READ,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::QUEUE_FAMILY_IGNORED,
                vk::QUEUE_FAMILY_IGNORED,
            );
        }
    }
}

impl Drop for OwnedImage {
    fn drop(&mut self) {
        unsafe {
            if self.image_view != vk::ImageView::null() {
                self.device.destroy_image_view(self.image_view, None);
            }
            if self.image.image != vk::Image::null() {
                self.device.destroy_image(self.image.image, None);
            }
        }
    }
}

/// A handle to a `VkImage` with size and format information.
#[derive(Clone)]
pub struct VulkanImage {
    pub size: Size<u32>,
    pub image: vk::Image,
    pub format: vk::Format,
}

#[derive(Clone)]
pub struct InputImage {
    pub image: VulkanImage,
    pub image_view: vk::ImageView,
    pub wrap_mode: WrapMode,
    pub filter_mode: FilterMode,
    pub mip_filter: FilterMode,
}
