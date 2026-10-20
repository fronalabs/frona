use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chromiumoxide::Page;
use chromiumoxide::browser::Browser;
use chromiumoxide::handler::HandlerConfig;
use futures::StreamExt;
use tokio::task::JoinHandle;

use crate::Result;
use crate::aria::axtree::AxRef;
use crate::error::Error;

struct Handle {
    browser: Browser,
    handler_task: Mutex<Option<JoinHandle<()>>>,
    snapshot_refs: Mutex<Option<Vec<AxRef>>>,
    last_snapshot: Mutex<Option<String>>,
    alive: AtomicBool,
    disconnect_error: Mutex<Option<Error>>,
    /// Set shorter than Browserless's per-job timeout so we self-evict before
    /// Browserless force-closes the WS mid-op.
    expires_at: Instant,
}

impl Drop for Handle {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.handler_task.lock()
            && let Some(task) = guard.take()
        {
            task.abort();
        }
    }
}

#[derive(Clone)]
pub struct BrowserConnection {
    inner: Arc<Handle>,
}

impl BrowserConnection {
    pub async fn connect(ws_url: &str, timeout: Duration, lifetime: Duration) -> Result<Self> {
        let config = HandlerConfig {
            request_timeout: timeout,
            ..Default::default()
        };
        let (browser, mut handler) = Browser::connect_with_config(ws_url, config)
            .await
            .map_err(Error::Cdp)?;

        let inner = Arc::new(Handle {
            browser,
            handler_task: Mutex::new(None),
            snapshot_refs: Mutex::new(None),
            last_snapshot: Mutex::new(None),
            alive: AtomicBool::new(true),
            disconnect_error: Mutex::new(None),
            expires_at: Instant::now() + lifetime,
        });

        let weak = Arc::downgrade(&inner);
        let handler_task = tokio::spawn(async move {
            while let Some(res) = handler.next().await {
                if let Err(e) = res {
                    // chromiumoxide's handler stream re-yields disconnect
                    // errors instead of terminating, so we act on them here.
                    if crate::error::is_cdp_disconnect(&e) {
                        tracing::warn!(error = %e, "chromiumoxide handler reported dead WS; marking connection dead");
                        if let Some(inner) = weak.upgrade() {
                            if let Ok(mut error) = inner.disconnect_error.lock() {
                                *error = Some(Error::Cdp(e));
                            }
                            inner.alive.store(false, Ordering::Release);
                        }
                        return;
                    }
                    tracing::debug!(error = %e, "chromiumoxide handler event error");
                }
            }
            tracing::warn!("chromiumoxide handler stream ended; marking browser connection dead");
            if let Some(inner) = weak.upgrade() {
                inner.alive.store(false, Ordering::Release);
            }
        });
        if let Ok(mut guard) = inner.handler_task.lock() {
            *guard = Some(handler_task);
        }

        let conn = BrowserConnection { inner };
        let initialization = async {
            if conn.pages().await?.is_empty() {
                conn.inner
                    .browser
                    .new_page("about:blank")
                    .await
                    .map_err(Error::Cdp)?;
            }
            Ok::<_, Error>(())
        }
        .await;
        if let Err(error) = initialization {
            // The handler dropping its channels can hide the actual transport error.
            // Preserve it so a permanent protocol failure is not retried as a disconnect.
            return Err(conn
                .inner
                .disconnect_error
                .lock()
                .ok()
                .and_then(|mut error| error.take())
                .unwrap_or(error));
        }
        Ok(conn)
    }

    pub fn is_alive(&self) -> bool {
        self.inner.alive.load(Ordering::Acquire) && Instant::now() < self.inner.expires_at
    }

    pub fn mark_dead(&self) {
        self.inner.alive.store(false, Ordering::Release);
    }

    pub fn expires_at(&self) -> Instant {
        self.inner.expires_at
    }

    pub(crate) async fn active_page(&self) -> Result<Page> {
        let pages = self.inner.browser.pages().await.map_err(Error::Cdp)?;
        if pages.is_empty() {
            return Err(Error::NoActivePage);
        }
        for page in &pages {
            let visible = page
                .evaluate("document.visibilityState === 'visible' && document.hasFocus()")
                .await
                .ok()
                .and_then(|r| r.value().and_then(|v| v.as_bool()))
                .unwrap_or(false);
            if visible {
                return Ok(page.clone());
            }
        }
        Ok(pages.into_iter().next().unwrap())
    }

    pub(crate) async fn pages(&self) -> Result<Vec<Page>> {
        self.inner.browser.pages().await.map_err(Error::Cdp)
    }

    pub(crate) fn browser(&self) -> &Browser {
        &self.inner.browser
    }

    pub(crate) fn store_snapshot_refs(&self, refs: Vec<AxRef>) {
        if let Ok(mut guard) = self.inner.snapshot_refs.lock() {
            *guard = Some(refs);
        }
    }

    pub(crate) fn lookup_snapshot_ref(&self, index: usize) -> Result<AxRef> {
        self.inner
            .snapshot_refs
            .lock()
            .ok()
            .and_then(|g| g.as_ref().and_then(|v| v.get(index).cloned()))
            .ok_or(Error::UnknownSnapshotIndex(index))
    }

    pub(crate) fn take_last_snapshot(&self) -> Option<String> {
        self.inner.last_snapshot.lock().ok().and_then(|g| g.clone())
    }

    pub(crate) fn store_last_snapshot(&self, rendered: String) {
        if let Ok(mut guard) = self.inner.last_snapshot.lock() {
            *guard = Some(rendered);
        }
    }

    pub async fn disconnect(self) -> Result<()> {
        self.mark_dead();
        if let Ok(mut guard) = self.inner.handler_task.lock()
            && let Some(task) = guard.take()
        {
            task.abort();
        }
        Ok(())
    }
}
