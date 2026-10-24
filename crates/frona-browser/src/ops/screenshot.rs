use std::path::Path;

use chromiumoxide::cdp::browser_protocol::page::CaptureScreenshotFormat;
use chromiumoxide::page::ScreenshotParams;

use crate::Result;
use crate::connection::BrowserConnection;
use crate::error::Error;
use crate::types::ScreenshotResult;

impl BrowserConnection {
    pub async fn screenshot(&self, path: &Path, full_page: bool) -> Result<ScreenshotResult> {
        let bytes = self.screenshot_bytes(full_page).await?;
        std::fs::write(path, &bytes)?;

        Ok(ScreenshotResult {
            path: path.to_string_lossy().into_owned(),
            size_bytes: bytes.len(),
            full_page,
        })
    }

    pub async fn screenshot_bytes(&self, full_page: bool) -> Result<Vec<u8>> {
        let page = self.active_page().await?;

        let params = ScreenshotParams::builder()
            .format(CaptureScreenshotFormat::Png)
            .full_page(full_page)
            .build();

        page.screenshot(params).await.map_err(Error::Cdp)
    }
}
