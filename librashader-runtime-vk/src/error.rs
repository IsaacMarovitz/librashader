//! Vulkan shader runtime errors.
use librashader_preprocess::PreprocessError;
use librashader_presets::ParsePresetError;
use librashader_reflect::error::{ShaderCompileError, ShaderReflectError};
use librashader_runtime::image::ImageError;
use thiserror::Error;

/// Cumulative error type for Vulkan filter chains.
#[derive(Error, Debug)]
pub enum FilterChainError {
    #[error("SPIRV reflection error")]
    SpirvCrossReflectError(#[from] spirv_cross::ErrorCode),
    #[error("shader preset parse error")]
    ShaderPresetError(#[from] ParsePresetError),
    #[error("shader preprocess error")]
    ShaderPreprocessError(#[from] PreprocessError),
    #[error("shader compile error")]
    ShaderCompileError(#[from] ShaderCompileError),
    #[error("shader reflect error")]
    ShaderReflectError(#[from] ShaderReflectError),
    #[error("lut loading error")]
    LutLoadError(#[from] ImageError),
    #[error("vulkan error")]
    VulkanResult(#[from] ash::vk::Result),
}

/// Result type for Vulkan filter chains.
pub type Result<T> = std::result::Result<T, FilterChainError>;
