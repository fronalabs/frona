use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use serde_json::Value;

use crate::core::error::AppError;
use crate::tool::image::{MAX_IMAGE_BYTES, is_supported_image, prepare_image};
use crate::tool::{AgentTool, InferenceContext, ToolDefinition, ToolOutput};

use super::manager::McpManager;

// A result may contain several screenshots. Bound both work and retained bytes;
// bad/excess images become text placeholders without changing the MCP status.
const MAX_RESULT_IMAGES: usize = 8;
const MAX_RESULT_IMAGE_BYTES: usize = 20 * 1024 * 1024;

fn tool_output(result: rmcp::model::CallToolResult) -> ToolOutput {
    let mut text = Vec::new();
    let mut images = Vec::new();
    let mut image_count = 0;
    let mut image_bytes = 0;
    for content in result.content {
        match content {
            rmcp::model::ContentBlock::Text(value) => text.push(value.text),
            rmcp::model::ContentBlock::Image(value) => {
                image_count += 1;
                let prepared = (|| {
                    if image_count > MAX_RESULT_IMAGES {
                        return Err("MCP result exceeds the 8 image limit");
                    }
                    if !is_supported_image(&value.mime_type) {
                        return Err("unsupported image type; expected PNG, JPEG, GIF or WebP");
                    }
                    // Reject before base64 allocation, then check actual bytes
                    // in the shared validator (padding can change the size).
                    if value.data.len() > MAX_IMAGE_BYTES.div_ceil(3) * 4 {
                        return Err("image exceeds the 10 MiB size limit");
                    }
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(value.data)
                        .map_err(|_| "invalid base64 image data")?;
                    let image = prepare_image(&bytes, &value.mime_type)?;
                    if image_bytes + image.bytes.len() > MAX_RESULT_IMAGE_BYTES {
                        return Err("MCP result exceeds the 20 MiB image budget");
                    }
                    Ok(image)
                })();
                match prepared {
                    Ok(image) => {
                        image_bytes += image.bytes.len();
                        images.push(image);
                    }
                    Err(reason) => text.push(format!("[MCP image omitted: {reason}]")),
                }
            }
            _ => {}
        }
    }
    let text = text.join("\n");
    let output = if result.is_error.unwrap_or(false) {
        ToolOutput::error(text)
    } else {
        ToolOutput::text(text)
    };
    output.with_images(images)
}

pub struct McpTool {
    manager: Arc<McpManager>,
    owner_name: String,
    cached_definitions: Vec<ToolDefinition>,
}

impl McpTool {
    pub fn new(manager: Arc<McpManager>, slug: &str, definitions: Vec<ToolDefinition>) -> Self {
        Self {
            manager,
            owner_name: format!("mcp__{slug}"),
            cached_definitions: definitions,
        }
    }
}

#[async_trait]
impl AgentTool for McpTool {
    fn name(&self) -> &str {
        &self.owner_name
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        self.cached_definitions.clone()
    }

    async fn execute(
        &self,
        tool_name: &str,
        arguments: Value,
        ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        let server_id = self
            .manager
            .server_for_tool(&ctx.user.id, tool_name)
            .await
            .ok_or_else(|| {
                AppError::Tool(format!("no running MCP server exposes tool {tool_name}"))
            })?;

        let prefix = format!("{}__", self.owner_name);
        let bare_name = tool_name.strip_prefix(&prefix).unwrap_or(tool_name);

        let result = self.manager.call(&server_id, bare_name, arguments).await?;

        Ok(tool_output(result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{CallToolResult, ContentBlock};

    fn png() -> Vec<u8> {
        let mut buffer = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(1, 1)
            .write_to(&mut buffer, image::ImageFormat::Png)
            .unwrap();
        buffer.into_inner()
    }

    fn image(bytes: &[u8]) -> ContentBlock {
        ContentBlock::image(
            base64::engine::general_purpose::STANDARD.encode(bytes),
            "image/png",
        )
    }

    #[test]
    fn text_and_image_results_keep_text_order_and_error_status() {
        let png = png();
        for is_error in [false, true] {
            let mut result = CallToolResult::success(vec![
                ContentBlock::text("before"),
                image(&png),
                ContentBlock::text("after"),
            ]);
            result.is_error = Some(is_error);
            let output = tool_output(result);
            assert_eq!(output.is_success(), !is_error);
            assert_eq!(output.text_content(), "before\nafter");
            assert_eq!(output.images().len(), 1);
            assert_eq!(output.images()[0].bytes, png);
            assert_eq!(output.images()[0].media_type, "image/png");
        }
    }

    #[test]
    fn image_only_text_only_and_empty_results_remain_explicit() {
        let output = tool_output(CallToolResult::success(vec![image(&png())]));
        assert!(output.is_success());
        assert_eq!(output.images().len(), 1);
        assert!(output.text_content().is_empty());
        let output = tool_output(CallToolResult::success(vec![
            ContentBlock::text("one"),
            ContentBlock::text("two"),
        ]));
        assert_eq!(output.text_content(), "one\ntwo");
        assert!(output.images().is_empty());
        let output = tool_output(CallToolResult::success(vec![]));
        assert!(output.is_success());
        assert!(output.text_content().is_empty());
        assert!(output.images().is_empty());
    }

    #[test]
    fn unusable_images_get_ordered_informative_placeholders() {
        let encode = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
        for (data, mime, reason) in [
            ("invalid!".into(), "image/png", "invalid base64"),
            (encode(b"not an image"), "image/png", "could not decode"),
            (encode(b"<svg/>"), "image/svg+xml", "unsupported image type"),
            (encode(&png()), "image/jpeg", "MIME type does not match"),
            (
                "A".repeat(MAX_IMAGE_BYTES.div_ceil(3) * 4 + 1),
                "image/png",
                "10 MiB",
            ),
        ] {
            let mut result = CallToolResult::success(vec![
                ContentBlock::text("before"),
                ContentBlock::image(data, mime),
                ContentBlock::text("after"),
            ]);
            result.is_error = Some(true);
            let output = tool_output(result);
            assert!(!output.is_success());
            assert!(output.images().is_empty());
            assert!(
                output
                    .text_content()
                    .starts_with("before\n[MCP image omitted:")
            );
            assert!(output.text_content().contains(reason));
            assert!(output.text_content().ends_with("]\nafter"));
        }
    }

    #[test]
    fn excessive_image_count_is_reported_without_dropping_text() {
        let png = png();
        let mut content: Vec<_> = (0..9).map(|_| image(&png)).collect();
        content.push(ContentBlock::text("after"));
        let output = tool_output(CallToolResult::success(content));
        assert!(output.is_success());
        assert_eq!(output.images().len(), 8);
        assert!(output.text_content().contains("8 image limit"));
        assert!(output.text_content().ends_with("after"));
    }

    #[test]
    fn aggregate_image_bytes_are_bounded() {
        // Valid PNG with trailing bytes: each image fits its individual limit,
        // but retaining all three would exceed the whole-result budget.
        let mut bytes = png();
        bytes.resize(7 * 1024 * 1024, 0);
        let output = tool_output(CallToolResult::success(
            (0..3).map(|_| image(&bytes)).collect(),
        ));
        assert_eq!(output.images().len(), 2);
        assert!(output.text_content().contains("20 MiB image budget"));
        assert!(output.is_success());
    }
}
