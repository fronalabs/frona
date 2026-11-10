//! Shared file operations. Authorization remains with the caller: the runtime
//! applies Cedar-derived sandbox policy; the SDK grants one explicit root.

pub mod glob;
pub mod image;
pub mod path;
pub mod read;

use std::path::{Path, PathBuf};

use image::ImageData;

pub fn resolve_path(root: &Path, input: &str) -> Result<PathBuf, FileError> {
    validate_input(input)?;
    path::resolve_filesystem_path(&root.join(input))
}

pub(super) fn validate_input(input: &str) -> Result<(), FileError> {
    if input.is_empty() {
        return Err(FileError::Validation("path must not be empty".into()));
    }
    if input.starts_with("agent://") || input.starts_with("user://") {
        return Err(FileError::Validation(format!(
            "virtual-path URIs are not supported here: {input}",
        )));
    }
    Ok(())
}

/// File content or an ordinary read refusal. Binary metadata lets callers
/// provide guidance appropriate to their own available tools.
pub enum FileOutput {
    Content {
        text: String,
        images: Vec<ImageData>,
        success: bool,
    },
    Binary {
        media_type: String,
        size: usize,
        path: String,
    },
}

impl FileOutput {
    fn text(text: impl Into<String>) -> Self {
        Self::mixed(text, Vec::new())
    }

    fn error(text: impl Into<String>) -> Self {
        Self::Content {
            text: text.into(),
            images: Vec::new(),
            success: false,
        }
    }

    fn mixed(text: impl Into<String>, images: Vec<ImageData>) -> Self {
        Self::Content {
            text: text.into(),
            images,
            success: true,
        }
    }

    pub(super) fn into_tool_output(self) -> crate::ToolResult {
        use crate::{ToolContent, ToolError, ToolOutput};
        use base64::Engine;
        use rig_core::completion::message::{DocumentSourceKind, Image, ImageMediaType, MimeType};
        let (text, images) = match self {
            Self::Content {
                text,
                images,
                success: true,
            } => (text, images),
            Self::Content {
                text,
                success: false,
                ..
            } => return Err(ToolError::failed(text)),
            Self::Binary {
                media_type,
                size,
                path,
            } => {
                return Err(ToolError::failed(format!(
                    "Read got a binary file ({media_type}, {size} bytes) at {path}. Choose a text file or a supported image (PNG/JPEG/GIF/WebP)."
                )));
            }
        };
        let mut content = vec![ToolContent::Text(text.into())];
        content.extend(images.into_iter().map(|image| {
            ToolContent::Image(Image {
                data: DocumentSourceKind::Base64(
                    base64::engine::general_purpose::STANDARD.encode(image.bytes),
                ),
                media_type: ImageMediaType::from_mime_type(&image.media_type),
                detail: None,
                native: None,
            })
        }));
        Ok(ToolOutput::content(content))
    }
}

/// Portable filesystem failures; application authorization errors stay with callers.
#[derive(Debug, thiserror::Error)]
pub enum FileError {
    #[error("Not found: {0}")]
    NotFound(String),
    #[error("Validation error: {0}")]
    Validation(String),
    #[error("Internal error: {0}")]
    Io(String),
}
