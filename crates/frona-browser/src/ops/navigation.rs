mod readiness;
mod session;

use crate::Result;
use crate::connection::BrowserConnection;
use crate::error::Error;
use crate::types::PageInfo;
use crate::url::normalize_url;

impl BrowserConnection {
    pub async fn navigate(&self, url: &str, wait_for_load: bool) -> Result<PageInfo> {
        let url = normalize_url(url);
        let guard = self.lock_navigation().await;
        let timeout = self.navigation_timeout();
        let page = tokio::time::timeout(timeout, self.active_page())
            .await
            .map_err(|_| Error::Timeout(timeout))??;
        readiness::navigate(&page, &url, wait_for_load, timeout, guard).await
    }

    pub async fn go_back(&self) -> Result<()> {
        let page = self.active_page().await?;
        page.evaluate("window.history.back()")
            .await
            .map_err(Error::Cdp)?;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        Ok(())
    }

    pub async fn go_forward(&self) -> Result<()> {
        let page = self.active_page().await?;
        page.evaluate("window.history.forward()")
            .await
            .map_err(Error::Cdp)?;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        Ok(())
    }
}
