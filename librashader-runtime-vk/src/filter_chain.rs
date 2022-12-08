use std::error::Error;
use std::path::Path;
use ash::vk;
use ash::vk::{PFN_vkGetInstanceProcAddr, PrimitiveTopology, PushConstantRange, StaticFn};
use rustc_hash::FxHashMap;
use librashader_preprocess::ShaderSource;
use librashader_presets::{ShaderPassConfig, ShaderPreset, TextureConfig};
use librashader_reflect::back::{CompilerBackend, CompileShader, FromCompilation};
use librashader_reflect::back::targets::SpirV;
use librashader_reflect::front::shaderc::GlslangCompilation;
use librashader_reflect::reflect::ReflectShader;
use librashader_reflect::reflect::semantics::{Semantic, ShaderSemantics, TextureSemantics, UniformBinding, UniformSemantic, UniqueSemantics};
use librashader_runtime::uniforms::UniformStorage;
use crate::{error, util};
use crate::filter_pass::{FilterPass, PipelineDescriptors, PipelineObjects};
use crate::framebuffer::Framebuffer;

pub struct Vulkan {
    physical_device: vk::PhysicalDevice,
    device: ash::Device,
    instance: ash::Instance,
    queue: vk::Queue,
    command_pool: vk::CommandPool,
    pipelines: vk::PipelineCache,
}

type ShaderPassMeta = (
    ShaderPassConfig,
    ShaderSource,
    CompilerBackend<
        impl CompileShader<SpirV, Options = Option<()>, Context =()> + ReflectShader,
    >,
);


#[derive(Clone)]
pub struct VulkanInfo<'a> {
    physical_device: &'a vk::PhysicalDevice,
    device: &'a vk::Device,
    instance: &'a vk::Instance,
    queue: &'a vk::Queue,
    memory_properties: &'a vk::PhysicalDeviceMemoryProperties,
    get_instance_proc_addr: PFN_vkGetInstanceProcAddr
}

pub struct FilterChainVulkan {
    pub(crate) common: FilterCommon,
    pub(crate) passes: Vec<FilterPass>,
    // pub(crate) output_framebuffers: Box<[OwnedFramebuffer]>,
    // pub(crate) feedback_framebuffers: Box<[OwnedFramebuffer]>,
    // pub(crate) history_framebuffers: VecDeque<OwnedFramebuffer>,
    // pub(crate) draw_quad: DrawQuad,
}

pub(crate) struct FilterCommon {
    // pub(crate) luts: FxHashMap<usize, LutTexture>,
    // pub samplers: SamplerSet,
    // pub output_textures: Box<[Option<Texture>]>,
    // pub feedback_textures: Box<[Option<Texture>]>,
    // pub history_textures: Box<[Option<Texture>]>,
    // pub config: FilterMutable,
}

pub type FilterChainOptionsVulkan = ();

impl FilterChainVulkan {
    /// Load the shader preset at the given path into a filter chain.
    pub fn load_from_path(
        vulkan: VulkanInfo,
        path: impl AsRef<Path>,
        options: Option<&FilterChainOptionsVulkan>,
    ) -> error::Result<FilterChainVulkan> {
        // load passes from preset
        let preset = ShaderPreset::try_parse(path)?;
        Self::load_from_preset(vulkan, preset, options)
    }

    pub fn load_from_preset(
        vulkan: VulkanInfo,
        preset: ShaderPreset,
        options: Option<&FilterChainOptionsVulkan>,
    ) -> error::Result<FilterChainVulkan> {
        let (passes, semantics) = FilterChainVulkan::load_preset(preset.shaders, &preset.textures)?;

        unsafe {
            let instance = ash::Instance::load(&StaticFn {
                get_instance_proc_addr: vulkan.get_instance_proc_addr,
            }, vulkan.instance.clone());

            let device = ash::Device::load(instance.fp_v1_0(), vulkan.device.clone());
        }


        todo!();
    }

    fn load_preset(
        passes: Vec<ShaderPassConfig>,
        textures: &[TextureConfig],
    ) -> error::Result<(Vec<ShaderPassMeta>, ShaderSemantics)> {
        let mut uniform_semantics: FxHashMap<String, UniformSemantic> = Default::default();
        let mut texture_semantics: FxHashMap<String, Semantic<TextureSemantics>> =
            Default::default();

        let passes = passes
            .into_iter()
            .map(|shader| {
                eprintln!("[vk] loading {}", &shader.name.display());
                let source: ShaderSource = ShaderSource::load(&shader.name)?;

                let spirv = GlslangCompilation::compile(&source)?;
                let reflect = SpirV::from_compilation(spirv)?;

                for parameter in source.parameters.iter() {
                    uniform_semantics.insert(
                        parameter.id.clone(),
                        UniformSemantic::Unique(Semantic {
                            semantics: UniqueSemantics::FloatParameter,
                            index: (),
                        }),
                    );
                }
                Ok::<_, Box<dyn Error>>((shader, source, reflect))
            })
            .into_iter()
            .collect::<error::Result<Vec<(ShaderPassConfig, ShaderSource, CompilerBackend<_>)>>>()?;

        for details in &passes {
            librashader_runtime::semantics::insert_pass_semantics(
                &mut uniform_semantics,
                &mut texture_semantics,
                &details.0,
            )
        }
        librashader_runtime::semantics::insert_lut_semantics(
            textures,
            &mut uniform_semantics,
            &mut texture_semantics,
        );

        let semantics = ShaderSemantics {
            uniform_semantics,
            texture_semantics,
        };

        Ok((passes, semantics))
    }

    fn init_passes(
        device: &ash::Device,
        passes: Vec<ShaderPassMeta>,
        semantics: &ShaderSemantics,
        images: u32,
    ) -> error::Result<Box<[FilterPass]>> {
        let mut filters = Vec::new();

        // initialize passes
        for (index, (config, source, mut reflect)) in passes.into_iter().enumerate() {
            let reflection = reflect.reflect(index, semantics)?;
            let spirv_words = reflect.compile(None)?;

            // todo: make framebuffers:
            // shader_vulkan: 2280

            let uniform_storage = UniformStorage::new(
                reflection
                    .ubo
                    .as_ref()
                    .map(|ubo| ubo.size as usize)
                    .unwrap_or(0),
                reflection
                    .push_constant
                    .as_ref()
                    .map(|push| push.size as usize)
                    .unwrap_or(0),
            );

            let mut uniform_bindings = FxHashMap::default();

            for param in reflection.meta.parameter_meta.values() {
                uniform_bindings.insert(UniformBinding::Parameter(param.id.clone()), param.offset);
            }

            for (semantics, param) in &reflection.meta.unique_meta {
                uniform_bindings.insert(UniformBinding::SemanticVariable(*semantics), param.offset);
            }

            for (semantics, param) in &reflection.meta.texture_size_meta {
                uniform_bindings.insert(UniformBinding::TextureSize(*semantics), param.offset);
            }

            // shader_vulkan 1927 (pipeline_layout)
            let pipeline_objects = PipelineObjects::new(&reflection, images, device)?;

            let ia = vk::PipelineInputAssemblyStateCreateInfo::builder()
                .topology(PrimitiveTopology::TRIANGLE_STRIP);
            let vao_attrs = [vk::VertexInputAttributeDescription {
                location: 0,
                binding: 0,
                format: vk::Format::R32G32_SFLOAT,
                offset: 0,
            }, vk::VertexInputAttributeDescription {
                location: 1,
                binding: 0,
                format: vk::Format::R32G32_SFLOAT,
                offset: (2 * std::mem::size_of::<f32>()) as u32,
            }];

            // shader_vulkan: 2026

            filters.push(FilterPass {
                compiled: spirv_words,
                uniform_storage,
                uniform_bindings,
                source,
                config,
            });
        }

        todo!();
    }
}