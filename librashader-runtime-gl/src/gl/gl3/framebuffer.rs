use gl::types::{GLenum, GLint, GLsizei, GLuint};
use librashader_common::{FilterMode, ImageFormat, Size, WrapMode};
use librashader_presets::Scale2D;
use crate::framebuffer::{GLImage, Viewport};
use crate::error::{FilterChainError, Result};
use crate::gl::Framebuffer;
use crate::texture::Texture;

#[derive(Debug)]
pub struct Gl3Framebuffer {
    image: GLuint,
    handle: GLuint,
    size: Size<u32>,
    format: GLenum,
    max_levels: u32,
    mip_levels: u32,
    is_raw: bool,
}

impl Framebuffer for Gl3Framebuffer {
    fn handle(&self) -> GLuint {
        self.handle
    }

    fn size(&self) -> Size<u32> {
        self.size
    }

    fn image(&self) -> GLuint {
        self.image
    }

    fn format(&self) -> GLenum {
        self.format
    }

    fn new(max_levels: u32) -> Gl3Framebuffer {
        let mut framebuffer = 0;
        unsafe {
            gl::GenFramebuffers(1, &mut framebuffer);
            gl::BindFramebuffer(gl::FRAMEBUFFER, framebuffer);
            gl::BindFramebuffer(gl::FRAMEBUFFER, 0);
        }

        Gl3Framebuffer {
            image: 0,
            size: Size {
                width: 1,
                height: 1,
            },
            format: 0,
            max_levels,
            mip_levels: 0,
            handle: framebuffer,
            is_raw: false,
        }
    }
    fn new_from_raw(
        texture: GLuint,
        handle: GLuint,
        format: GLenum,
        size: Size<u32>,
        miplevels: u32,
    ) -> Gl3Framebuffer {
        Gl3Framebuffer {
            image: texture,
            size,
            format,
            max_levels: miplevels,
            mip_levels: miplevels,
            handle,
            is_raw: true,
        }
    }
    fn as_texture(&self, filter: FilterMode, wrap_mode: WrapMode) -> Texture {
        Texture {
            image: GLImage {
                handle: self.image,
                format: self.format,
                size: self.size,
                padded_size: Default::default(),
            },
            filter,
            mip_filter: filter,
            wrap_mode,
        }
    }
    fn scale(
        &mut self,
        scaling: Scale2D,
        format: ImageFormat,
        viewport: &Viewport<Self>,
        _original: &Texture,
        source: &Texture,
    ) -> Result<Size<u32>> {
        if self.is_raw {
            return Ok(self.size);
        }

        let size = librashader_runtime::scaling::scale(scaling, source.image.size, viewport.output.size);

        if self.size != size {
            self.size = size;

            self.init(
                size,
                if format == ImageFormat::Unknown {
                    ImageFormat::R8G8B8A8Unorm
                } else {
                    format
                },
            )?;
        }
        Ok(size)
    }
    fn clear<const REBIND: bool>(&self) {
        unsafe {
            if REBIND {
                gl::BindFramebuffer(gl::FRAMEBUFFER, self.handle);
            }
            gl::ColorMask(gl::TRUE, gl::TRUE, gl::TRUE, gl::TRUE);
            gl::ClearColor(0.0, 0.0, 0.0, 0.0);
            gl::Clear(gl::COLOR_BUFFER_BIT);
            if REBIND {
                gl::BindFramebuffer(gl::FRAMEBUFFER, 0);
            }
        }
    }
    fn copy_from(&mut self, image: &GLImage) -> Result<()> {
        // todo: may want to use a shader and draw a quad to be faster.
        if image.size != self.size || image.format != self.format {
            self.init(image.size, image.format)?;
        }

        unsafe {
            gl::BindFramebuffer(gl::FRAMEBUFFER, self.handle);

            gl::FramebufferTexture2D(
                gl::READ_FRAMEBUFFER,
                gl::COLOR_ATTACHMENT0,
                gl::TEXTURE_2D,
                image.handle,
                0,
            );

            gl::FramebufferTexture2D(
                gl::DRAW_FRAMEBUFFER,
                gl::COLOR_ATTACHMENT1,
                gl::TEXTURE_2D,
                self.image,
                0,
            );
            gl::ReadBuffer(gl::COLOR_ATTACHMENT0);
            gl::DrawBuffer(gl::COLOR_ATTACHMENT1);
            gl::BlitFramebuffer(
                0,
                0,
                self.size.width as GLint,
                self.size.height as GLint,
                0,
                0,
                self.size.width as GLint,
                self.size.height as GLint,
                gl::COLOR_BUFFER_BIT,
                gl::NEAREST,
            );

            // cleanup after ourselves.
            gl::FramebufferTexture2D(
                gl::READ_FRAMEBUFFER,
                gl::COLOR_ATTACHMENT0,
                gl::TEXTURE_2D,
                0,
                0,
            );

            gl::FramebufferTexture2D(
                gl::DRAW_FRAMEBUFFER,
                gl::COLOR_ATTACHMENT1,
                gl::TEXTURE_2D,
                0,
                0,
            );

            // set this back to color_attachment 0
            gl::FramebufferTexture2D(
                gl::FRAMEBUFFER,
                gl::COLOR_ATTACHMENT0,
                gl::TEXTURE_2D,
                self.image,
                0,
            );

            gl::BindFramebuffer(gl::FRAMEBUFFER, 0);
        }

        Ok(())
    }
    fn init(&mut self, mut size: Size<u32>, format: impl Into<GLenum>) -> Result<()> {
        if self.is_raw {
            return Ok(());
        }
        self.format = format.into();
        self.size = size;

        unsafe {
            gl::BindFramebuffer(gl::FRAMEBUFFER, self.handle);

            // reset the framebuffer image
            if self.image != 0 {
                gl::FramebufferTexture2D(
                    gl::FRAMEBUFFER,
                    gl::COLOR_ATTACHMENT0,
                    gl::TEXTURE_2D,
                    0,
                    0,
                );
                gl::DeleteTextures(1, &self.image);
            }

            gl::GenTextures(1, &mut self.image);
            gl::BindTexture(gl::TEXTURE_2D, self.image);

            if size.width == 0 {
                size.width = 1;
            }
            if size.height == 0 {
                size.height = 1;
            }

            self.mip_levels = librashader_runtime::scaling::calc_miplevel(size);
            if self.mip_levels > self.max_levels {
                self.mip_levels = self.max_levels;
            }
            if self.mip_levels == 0 {
                self.mip_levels = 1;
            }

            gl::TexStorage2D(
                gl::TEXTURE_2D,
                self.mip_levels as GLsizei,
                self.format,
                size.width as GLsizei,
                size.height as GLsizei,
            );

            gl::FramebufferTexture2D(
                gl::FRAMEBUFFER,
                gl::COLOR_ATTACHMENT0,
                gl::TEXTURE_2D,
                self.image,
                0,
            );

            let status = gl::CheckFramebufferStatus(gl::FRAMEBUFFER);
            if status != gl::FRAMEBUFFER_COMPLETE {
                match status {
                    gl::FRAMEBUFFER_UNSUPPORTED => {
                        eprintln!("unsupported fbo");

                        gl::FramebufferTexture2D(
                            gl::FRAMEBUFFER,
                            gl::COLOR_ATTACHMENT0,
                            gl::TEXTURE_2D,
                            0,
                            0,
                        );
                        gl::DeleteTextures(1, &self.image);
                        gl::GenTextures(1, &mut self.image);
                        gl::BindTexture(gl::TEXTURE_2D, self.image);

                        self.mip_levels = librashader_runtime::scaling::calc_miplevel(size);
                        if self.mip_levels > self.max_levels {
                            self.mip_levels = self.max_levels;
                        }
                        if self.mip_levels == 0 {
                            self.mip_levels = 1;
                        }

                        gl::TexStorage2D(
                            gl::TEXTURE_2D,
                            self.mip_levels as GLsizei,
                            ImageFormat::R8G8B8A8Unorm.into(),
                            size.width as GLsizei,
                            size.height as GLsizei,
                        );
                        gl::FramebufferTexture2D(
                            gl::FRAMEBUFFER,
                            gl::COLOR_ATTACHMENT0,
                            gl::TEXTURE_2D,
                            self.image,
                            0,
                        );
                        // self.init =
                        //     gl::CheckFramebufferStatus(gl::FRAMEBUFFER) == gl::FRAMEBUFFER_COMPLETE;
                    }
                    _ => return Err(FilterChainError::FramebufferInit(status))
                }
            }

            gl::BindFramebuffer(gl::FRAMEBUFFER, 0);
            gl::BindTexture(gl::TEXTURE_2D, 0);
        }

        Ok(())
    }
}

impl Drop for Gl3Framebuffer {
    fn drop(&mut self) {
        unsafe {
            if self.handle != 0 {
                gl::DeleteFramebuffers(1, &self.handle);
            }
            if self.image != 0 {
                gl::DeleteTextures(1, &self.image);
            }
        }
    }
}