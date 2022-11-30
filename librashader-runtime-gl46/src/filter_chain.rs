use crate::binding::{BufferStorage, UniformLocation, VariableLocation};
use crate::filter_pass::FilterPass;
use crate::framebuffer::{Framebuffer, GlImage, Viewport};
use crate::quad_render::DrawQuad;
use crate::render_target::RenderTarget;
use crate::util;
use crate::util::{gl_get_version, gl_u16_to_version, InlineRingBuffer};
use crate::error::{FilterChainError, Result};

use gl::types::{GLint, GLsizei, GLsizeiptr, GLuint};
use librashader_common::image::Image;
use librashader_common::{FilterMode, Size, WrapMode};
use librashader_preprocess::ShaderSource;
use librashader_presets::{ShaderPassConfig, ShaderPreset, TextureConfig};
use librashader_reflect::back::cross::{GlslangGlslContext, GlVersion};
use librashader_reflect::back::targets::GLSL;
use librashader_reflect::reflect::semantics::{MemberOffset, ReflectSemantics, SemanticMap, TextureSemantics, UniformBinding, UniformMeta, UniformSemantic, VariableSemantics};
use librashader_reflect::reflect::ReflectShader;
use rustc_hash::FxHashMap;
use spirv_cross::spirv::Decoration;
use std::collections::VecDeque;
use std::path::Path;
use librashader_reflect::back::{CompilerBackend, CompileShader, FromCompilation};
use librashader_reflect::front::shaderc::GlslangCompilation;
use crate::options::{FilterChainOptions, FrameOptions};
use crate::samplers::SamplerSet;
use crate::texture::Texture;

pub struct FilterChain {
    passes: Box<[FilterPass]>,
    common: FilterCommon,
    pub(crate) draw_quad: DrawQuad,
    output_framebuffers: Box<[Framebuffer]>,
    feedback_framebuffers: Box<[Framebuffer]>,
    history_framebuffers: VecDeque<Framebuffer>,
}

pub struct FilterCommon {
    // semantics: ReflectSemantics,
    pub(crate) config: FilterMutable,
    pub(crate) luts: FxHashMap<usize, Texture>,
    pub(crate) samplers: SamplerSet,
    pub output_textures: Box<[Texture]>,
    pub feedback_textures: Box<[Texture]>,
    pub history_textures: Box<[Texture]>,
}

pub struct FilterMutable {
    pub(crate) passes_enabled: usize,
    pub(crate) parameters: FxHashMap<String, f32>
}

impl FilterChain {
    fn reflect_uniform_location(pipeline: GLuint, meta: &impl UniformMeta) -> VariableLocation {
        // todo: support both ubo and pushco
        // todo: fix this.
        match meta.offset() {
            MemberOffset::Ubo(_) => {
                let vert_name = format!("LIBRA_UBO_VERTEX_INSTANCE.{}\0", meta.id());
                let frag_name = format!("LIBRA_UBO_FRAGMENT_INSTANCE.{}\0", meta.id());
                unsafe {
                    let vertex = gl::GetUniformLocation(pipeline, vert_name.as_ptr().cast());
                    let fragment = gl::GetUniformLocation(pipeline, frag_name.as_ptr().cast());

                    VariableLocation::Ubo(UniformLocation { vertex, fragment })
                }
            }
            MemberOffset::PushConstant(_) => {
                let vert_name = format!("LIBRA_PUSH_VERTEX_INSTANCE.{}\0", meta.id());
                let frag_name = format!("LIBRA_PUSH_FRAGMENT_INSTANCE.{}\0", meta.id());
                unsafe {
                    let vertex = gl::GetUniformLocation(pipeline, vert_name.as_ptr().cast());
                    let fragment = gl::GetUniformLocation(pipeline, frag_name.as_ptr().cast());

                    VariableLocation::Push(UniformLocation { vertex, fragment })
                }
            }
        }
    }
}

type ShaderPassMeta = (
    ShaderPassConfig,
    ShaderSource,
    CompilerBackend<
        impl CompileShader<GLSL, Options = GlVersion, Context = GlslangGlslContext> + ReflectShader,
    >,
);

impl FilterChain {
    /// Load a filter chain from a pre-parsed `ShaderPreset`.
    pub fn load_from_preset(preset: ShaderPreset, options: Option<&FilterChainOptions>) -> Result<FilterChain> {
        let (passes, semantics) = FilterChain::load_preset(preset.shaders, &preset.textures)?;

        let version = options.map(|o| gl_u16_to_version(o.gl_version))
            .unwrap_or_else(|| gl_get_version());

        // initialize passes
        let filters = FilterChain::init_passes(version, passes, &semantics)?;

        let default_filter = filters.first().map(|f| f.config.filter).unwrap_or_default();
        let default_wrap = filters
            .first()
            .map(|f| f.config.wrap_mode)
            .unwrap_or_default();

        let samplers = SamplerSet::new();

        // initialize output framebuffers
        let mut output_framebuffers = Vec::new();
        output_framebuffers.resize_with(filters.len(), || Framebuffer::new(1));
        let mut output_textures = Vec::new();
        output_textures.resize_with(filters.len(), Texture::default);

        // initialize feedback framebuffers
        let mut feedback_framebuffers = Vec::new();
        feedback_framebuffers.resize_with(filters.len(), || Framebuffer::new(1));
        let mut feedback_textures = Vec::new();
        feedback_textures.resize_with(filters.len(), Texture::default);

        // load luts
        let luts = FilterChain::load_luts(&preset.textures)?;

        let (history_framebuffers, history_textures) =
            FilterChain::init_history(&filters, default_filter, default_wrap);

        // create VBO objects
        let draw_quad = DrawQuad::new();

        Ok(FilterChain {
            passes: filters,
            output_framebuffers: output_framebuffers.into_boxed_slice(),
            feedback_framebuffers: feedback_framebuffers.into_boxed_slice(),
            history_framebuffers,
            draw_quad,
            common: FilterCommon {
                config: FilterMutable {
                    passes_enabled: preset.shader_count as usize,
                    parameters: preset.parameters.into_iter()
                        .map(|param| (param.name, param.value)).collect(),
                },
                luts,
                samplers,
                output_textures: output_textures.into_boxed_slice(),
                feedback_textures: feedback_textures.into_boxed_slice(),
                history_textures,
            },
        })
    }

    /// Load the shader preset at the given path into a filter chain.
    pub fn load_from_path(path: impl AsRef<Path>, options: Option<&FilterChainOptions>) -> Result<FilterChain> {
        // load passes from preset
        let preset = ShaderPreset::try_parse(path)?;
        Self::load_from_preset(preset, options)
    }

    fn load_preset(
        passes: Vec<ShaderPassConfig>,
        textures: &[TextureConfig]
    ) -> Result<(Vec<ShaderPassMeta>, ReflectSemantics)> {
        let mut uniform_semantics: FxHashMap<String, UniformSemantic> = Default::default();
        let mut texture_semantics: FxHashMap<String, SemanticMap<TextureSemantics>> =
            Default::default();

        let passes = passes
            .into_iter()
            .map(|shader| {
                eprintln!("[gl] loading {}", &shader.name.display());
                let source: ShaderSource = ShaderSource::load(&shader.name)?;

                let spirv = GlslangCompilation::compile(&source)?;
                let reflect = GLSL::from_compilation(spirv)?;

                for parameter in source.parameters.iter() {
                    uniform_semantics.insert(
                        parameter.id.clone(),
                        UniformSemantic::Variable(SemanticMap {
                            semantics: VariableSemantics::FloatParameter,
                            index: (),
                        }),
                    );
                }
                Ok::<_, FilterChainError>((shader, source, reflect))
            })
            .into_iter()
            .collect::<Result<Vec<(ShaderPassConfig, ShaderSource, CompilerBackend<_>)>>>()?;

        for details in &passes {
            librashader_runtime::semantics::insert_pass_semantics(
                &mut uniform_semantics,
                &mut texture_semantics,
                &details.0,
            )
        }

        // add lut params
        librashader_runtime::semantics::insert_lut_semantics(textures,
                                                             &mut uniform_semantics,
                                                             &mut texture_semantics);
        let semantics = ReflectSemantics {
            uniform_semantics,
            texture_semantics,
        };

        Ok((passes, semantics))
    }

    fn load_luts(textures: &[TextureConfig]) -> Result<FxHashMap<usize, Texture>> {
        let mut luts = FxHashMap::default();
        let pixel_unpack = unsafe {
            let mut binding = 0;
            gl::GetIntegerv(gl::PIXEL_UNPACK_BUFFER_BINDING, &mut binding);
            binding
        };

        unsafe {
            gl::BindBuffer(gl::PIXEL_UNPACK_BUFFER, 0);
        }

        for (index, texture) in textures.iter().enumerate() {
            let image = Image::load(&texture.path)?;
            let levels = if texture.mipmap {
                util::calc_miplevel(image.size)
            } else {
                1u32
            };

            let mut handle = 0;
            unsafe {
                gl::CreateTextures(gl::TEXTURE_2D,1, &mut handle);

                gl::TextureStorage2D(
                    handle,
                    levels as GLsizei,
                    gl::RGBA8,
                    image.size.width as GLsizei,
                    image.size.height as GLsizei,
                );

                gl::PixelStorei(gl::UNPACK_ROW_LENGTH, 0);
                gl::PixelStorei(gl::UNPACK_ALIGNMENT, 4);

                gl::TextureSubImage2D(
                    handle,
                    0, 0, 0,
                    image.size.width as GLsizei,
                    image.size.height as GLsizei,
                    gl::RGBA,
                    gl::UNSIGNED_BYTE,
                    image.bytes.as_ptr().cast(),
                );

                let mipmap = levels > 1;
                if mipmap {
                    gl::GenerateTextureMipmap(handle);
                }
            }

            luts.insert(
                index,
                Texture {
                    image: GlImage {
                        handle,
                        format: gl::RGBA8,
                        size: image.size,
                        padded_size: Size::default(),
                    },
                    filter: texture.filter_mode,
                    mip_filter: texture.filter_mode,
                    wrap_mode: texture.wrap_mode,
                },
            );
        }

        unsafe {
            gl::BindBuffer(gl::PIXEL_UNPACK_BUFFER, pixel_unpack as GLuint);
        };
        Ok(luts)
    }

    fn init_passes(
        version: GlVersion,
        passes: Vec<ShaderPassMeta>,
        semantics: &ReflectSemantics,
    ) -> Result<Box<[FilterPass]>> {
        let mut filters = Vec::new();

        // initialize passes
        for (index, (config, source, mut reflect)) in passes.into_iter().enumerate() {
            let reflection = reflect.reflect(index, semantics)?;
            let glsl = reflect.compile(version)?;

            let vertex_resources = glsl.context.compiler.vertex.get_shader_resources()?;

            // todo: split this out.
            let (program, ubo_location) = unsafe {
                let vertex = util::gl_compile_shader(gl::VERTEX_SHADER, glsl.vertex.as_str());
                let fragment = util::gl_compile_shader(gl::FRAGMENT_SHADER, glsl.fragment.as_str());

                let program = gl::CreateProgram();
                gl::AttachShader(program, vertex);
                gl::AttachShader(program, fragment);

                for res in vertex_resources.stage_inputs {
                    let loc = glsl
                        .context
                        .compiler
                        .vertex
                        .get_decoration(res.id, Decoration::Location)?;
                    let mut name = res.name;
                    name.push('\0');

                    gl::BindAttribLocation(program, loc, name.as_str().as_ptr().cast())
                }
                gl::LinkProgram(program);
                gl::DeleteShader(vertex);
                gl::DeleteShader(fragment);

                let mut status = 0;
                gl::GetProgramiv(program, gl::LINK_STATUS, &mut status);
                if status != 1 {
                    panic!("failed to link program")
                }

                gl::UseProgram(program);

                for (name, binding) in &glsl.context.sampler_bindings {
                    let location =
                        gl::GetUniformLocation(program, name.as_str().as_ptr().cast());
                    if location >= 0 {
                        // eprintln!("setting sampler {location} to sample from {binding}");
                        gl::Uniform1i(location, *binding as GLint);
                    }
                }

                gl::UseProgram(0);
                (
                    program,
                    UniformLocation {
                        vertex: gl::GetUniformBlockIndex(
                            program,
                            b"LIBRA_UBO_VERTEX\0".as_ptr().cast(),
                        ),
                        fragment: gl::GetUniformBlockIndex(
                            program,
                            b"LIBRA_UBO_FRAGMENT\0".as_ptr().cast(),
                        ),
                    },
                )
            };

            let ubo_ring = if let Some(ubo) = &reflection.ubo {
                let size = ubo.size;
                let mut ring: InlineRingBuffer<GLuint, 16> = InlineRingBuffer::new();
                unsafe {
                    gl::CreateBuffers(16, ring.items_mut().as_mut_ptr());
                    for buffer in ring.items() {
                        gl::NamedBufferData(
                            *buffer,
                            size as GLsizeiptr,
                            std::ptr::null(),
                            gl::STREAM_DRAW,
                        );
                    }
                }
                Some(ring)
            } else {
                None
            };

            let uniform_storage = BufferStorage::new(reflection
                                                         .ubo
                                                         .as_ref()
                                                         .map(|ubo| ubo.size as usize)
                                                         .unwrap_or(0),
                                                     reflection
                                                         .push_constant
                                                         .as_ref()
                                                         .map(|push| push.size as usize)
                                                         .unwrap_or(0)
            );

            let mut uniform_bindings = FxHashMap::default();
            for param in reflection.meta.parameter_meta.values() {
                uniform_bindings.insert(
                    UniformBinding::Parameter(param.id.clone()),
                    (
                        FilterChain::reflect_uniform_location(program, param),
                        param.offset,
                    ),
                );
            }

            for (semantics, param) in &reflection.meta.variable_meta {
                uniform_bindings.insert(
                    UniformBinding::SemanticVariable(*semantics),
                    (
                        FilterChain::reflect_uniform_location(program, param),
                        param.offset,
                    ),
                );
            }

            for (semantics, param) in &reflection.meta.texture_size_meta {
                uniform_bindings.insert(
                    UniformBinding::TextureSize(*semantics),
                    (
                        FilterChain::reflect_uniform_location(program, param),
                        param.offset,
                    ),
                );
            }

            // eprintln!("{:#?}", reflection.meta.texture_meta);
            // eprintln!("{:#?}", reflection.meta);
            // eprintln!("{:#?}", locations);
            // eprintln!("{:#?}", reflection.push_constant);
            // eprintln!("====fragment====");
            // eprintln!("{:#}", glsl.fragment);
            // eprintln!("====vertex====");
            // eprintln!("{:#}", glsl.vertex);

            filters.push(FilterPass {
                reflection,
                compiled: glsl,
                program,
                ubo_location,
                ubo_ring,
                uniform_storage,
                uniform_bindings,
                source,
                config
            });
        }

        Ok(filters.into_boxed_slice())
    }

    fn init_history(
        filters: &[FilterPass],
        filter: FilterMode,
        wrap_mode: WrapMode,
    ) -> (VecDeque<Framebuffer>, Box<[Texture]>) {
        let mut required_images = 0;

        for pass in filters {
            // If a shader uses history size, but not history, we still need to keep the texture.
            let texture_count = pass
                .reflection
                .meta
                .texture_meta
                .iter()
                .filter(|(semantics, _)| semantics.semantics == TextureSemantics::OriginalHistory)
                .count();
            let texture_size_count = pass
                .reflection
                .meta
                .texture_size_meta
                .iter()
                .filter(|(semantics, _)| semantics.semantics == TextureSemantics::OriginalHistory)
                .count();

            required_images = std::cmp::max(required_images, texture_count);
            required_images = std::cmp::max(required_images, texture_size_count);
        }

        // not using frame history;
        if required_images <= 1 {
            println!("[history] not using frame history");
            return (VecDeque::new(), Box::new([]));
        }

        // history0 is aliased with the original

        eprintln!("[history] using frame history with {required_images} images");
        let mut framebuffers = VecDeque::with_capacity(required_images);
        framebuffers.resize_with(required_images, || Framebuffer::new(1));

        let mut history_textures = Vec::new();
        history_textures.resize_with(required_images, || Texture {
            image: Default::default(),
            filter,
            mip_filter: filter,
            wrap_mode,
        });

        (framebuffers, history_textures.into_boxed_slice())
    }

    fn push_history(&mut self, input: &GlImage) -> Result<()> {
        if let Some(mut back) = self.history_framebuffers.pop_back() {
            if back.size != input.size || (input.format != 0 && input.format != back.format) {
                eprintln!("[history] resizing");
                back.init(input.size, input.format)?;
            }

            back.copy_from(input)?;

            self.history_framebuffers.push_front(back)
        }

        Ok(())
    }

    /// Process a frame with the input image.
    ///
    /// When this frame returns, GL_FRAMEBUFFER is bound to 0.
    pub fn frame(&mut self, count: usize, viewport: &Viewport, input: &GlImage, options: Option<&FrameOptions>) -> Result<()> {
        // limit number of passes to those enabled.
        let passes = &mut self.passes[0..self.common.config.passes_enabled];
        if let Some(options) = options {
            if options.clear_history {
                for framebuffer in &self.history_framebuffers {
                    framebuffer.clear()
                }
            }
        }

        if passes.is_empty() {
            return Ok(());
        }

        // do not need to rebind FBO 0 here since first `draw` will
        // bind automatically.
        self.draw_quad.bind_vao();

        let filter = passes[0].config.filter;
        let wrap_mode = passes[0].config.wrap_mode;

        // update history
        for (texture, fbo) in self
            .common
            .history_textures
            .iter_mut()
            .zip(self.history_framebuffers.iter())
        {
            texture.image = fbo.as_texture(filter, wrap_mode).image;
        }

        for ((texture, fbo), pass) in self
            .common
            .feedback_textures
            .iter_mut()
            .zip(self.feedback_framebuffers.iter())
            .zip(passes.iter())
        {
            texture.image = fbo
                .as_texture(pass.config.filter, pass.config.wrap_mode)
                .image;
        }

        // shader_gl3: 2067
        let original = Texture {
            image: *input,
            filter,
            mip_filter: filter,
            wrap_mode,
        };

        let mut source = original;

        // rescale render buffers to ensure all bindings are valid.
        for (index, pass) in passes.iter_mut().enumerate() {
            self.output_framebuffers[index].scale(
                pass.config.scaling.clone(),
                pass.get_format(),
                viewport,
                &original,
                &source,
            )?;

            self.feedback_framebuffers[index].scale(
                pass.config.scaling.clone(),
                pass.get_format(),
                viewport,
                &original,
                &source,
            )?;
        }

        let passes_len = passes.len();
        let (pass, last) = passes.split_at_mut(passes_len - 1);

        for (index, pass) in pass.iter_mut().enumerate() {
            let target = &self.output_framebuffers[index];
            pass.draw(
                index,
                &self.common,
                if pass.config.frame_count_mod > 0 {
                    count % pass.config.frame_count_mod as usize
                } else {
                    count
                } as u32,
                1,
                viewport,
                &original,
                &source,
                RenderTarget::new(target, None, 0, 0),
            );

            let target = target.as_texture(pass.config.filter, pass.config.wrap_mode);
            self.common.output_textures[index] = target;
            source = target;
        }

        assert_eq!(last.len(), 1);
        for pass in last {
            source.filter = pass.config.filter;
            source.mip_filter = pass.config.filter;

            pass.draw(
                passes_len - 1,
                &self.common,
                if pass.config.frame_count_mod > 0 {
                    count % pass.config.frame_count_mod as usize
                } else {
                    count
                } as u32,
                1,
                viewport,
                &original,
                &source,
                RenderTarget::new(viewport.output, viewport.mvp, viewport.x, viewport.y),
            );
        }

        // swap feedback framebuffers with output
        for (output, feedback) in self
            .output_framebuffers
            .iter_mut()
            .zip(self.feedback_framebuffers.iter_mut())
        {
            std::mem::swap(output, feedback);
        }

        self.push_history(input)?;

        // do not need to rebind FBO 0 here since first `draw` will
        // bind automatically.
        self.draw_quad.unbind_vao();

        Ok(())
    }
}