use std::{collections::HashMap, time::Duration};

use chromiumoxide::Page;
use serde_json::{Value, json};
use tokio::time::{Instant, timeout_at};
use url::Url;

use super::session::{Session, response};
use crate::{Result, error::Error, types::PageInfo};

#[derive(Default)]
struct Frame {
    url: String,
    loader: String,
    started: bool,
    dom_ready: bool,
    parent: String,
    failure: Option<(String, String)>,
}

struct Readiness {
    main: String,
    frames: HashMap<String, Frame>,
    requests: HashMap<String, (String, String)>,
}

impl Readiness {
    fn event(&mut self, message: &Value) {
        let p = &message["params"];
        let frame_id = p["frameId"].as_str().unwrap_or_default();
        match message["method"].as_str().unwrap_or_default() {
            "Network.requestWillBeSent" if p["type"] == "Document" => {
                let frame = self.frames.entry(frame_id.into()).or_default();
                frame.url = p["request"]["url"].as_str().unwrap_or_default().into();
                frame.started = true;
                frame.dom_ready = false;
                frame.failure = None;
                self.requests.insert(
                    p["requestId"].as_str().unwrap_or_default().into(),
                    (
                        frame_id.into(),
                        p["loaderId"].as_str().unwrap_or_default().into(),
                    ),
                );
            }
            "Network.loadingFailed" => {
                if let Some(frame) = self
                    .requests
                    .remove(p["requestId"].as_str().unwrap_or_default())
                    .and_then(|(id, loader)| {
                        self.frames.get_mut(&id).filter(|f| f.loader == loader)
                    })
                {
                    frame.failure = Some((
                        frame.url.clone(),
                        p["errorText"].as_str().unwrap_or("Loading failed").into(),
                    ));
                }
            }
            "Network.loadingFinished" => {
                self.requests
                    .remove(p["requestId"].as_str().unwrap_or_default());
            }
            "Page.frameAttached" => {
                self.frames.entry(frame_id.into()).or_default().parent =
                    p["parentFrameId"].as_str().unwrap_or_default().into();
            }
            "Page.frameStartedNavigating" => {
                let frame = self.frames.entry(frame_id.into()).or_default();
                frame.url = p["url"].as_str().unwrap_or_default().into();
                frame.loader = p["loaderId"].as_str().unwrap_or_default().into();
                frame.started = true;
                frame.dom_ready = false;
                frame.failure = None;
            }
            "Page.frameNavigated" => {
                let id = p["frame"]["id"].as_str().unwrap_or_default();
                // Drop frames belonging to the previous main document.
                if id == self.main {
                    self.frames.retain(|id, _| id == &self.main);
                }
                let frame = self.frames.entry(id.into()).or_default();
                let loader = p["frame"]["loaderId"].as_str().unwrap_or_default();
                if frame.loader != loader {
                    frame.dom_ready = false;
                }
                frame.parent = p["frame"]["parentId"].as_str().unwrap_or_default().into();
                frame.url = p["frame"]["url"].as_str().unwrap_or_default().into();
                frame.loader = loader.into();
            }
            "Page.lifecycleEvent" => {
                let frame = self.frames.entry(frame_id.into()).or_default();
                let loader = p["loaderId"].as_str().unwrap_or_default();
                match p["name"].as_str().unwrap_or_default() {
                    "init" => {
                        frame.loader = loader.into();
                        frame.dom_ready = false;
                    }
                    "DOMContentLoaded" | "load" if frame.loader == loader => frame.dom_ready = true,
                    _ => {}
                }
            }
            "Page.frameDetached" if p["reason"] == "remove" => {
                let mut removed = vec![frame_id.to_owned()];
                while let Some(id) = removed.pop() {
                    removed.extend(
                        self.frames
                            .iter()
                            .filter(|(_, f)| f.parent == id)
                            .map(|(id, _)| id.clone()),
                    );
                    self.frames.remove(&id);
                }
            }
            _ => {}
        }
    }

    fn main_ready(&self, loader: &str) -> bool {
        self.frames
            .get(&self.main)
            .is_some_and(|f| f.loader == loader && f.dom_ready)
    }

    fn effective_url<'a>(&'a self, mut frame: &'a Frame) -> &'a str {
        for _ in 0..self.frames.len() {
            if !matches!(frame.url.as_str(), "about:blank" | "about:srcdoc" | "") {
                break;
            }
            let Some(parent) = self.frames.get(&frame.parent) else {
                break;
            };
            frame = parent;
        }
        &frame.url
    }

    fn frame_warnings(&self) -> Vec<String> {
        let Some(main) = self.frames.get(&self.main) else {
            return Vec::new();
        };
        self.frames
            .iter()
            .filter(|(id, _)| *id != &self.main)
            .filter_map(|(_, f)| {
                let (url, error) = f.failure.as_ref()?;
                same_domain(&main.url, url)
                    .then(|| format!("Iframe failed to load: {url} ({error})"))
            })
            .collect()
    }

    fn pending_frames(&self) -> Vec<String> {
        let Some(main) = self.frames.get(&self.main) else {
            return Vec::new();
        };
        let mut urls: Vec<_> = self
            .frames
            .iter()
            .filter(|(id, f)| {
                *id != &self.main
                    && f.started
                    && !f.dom_ready
                    && f.failure.is_none()
                    && same_domain(&main.url, self.effective_url(f))
            })
            .map(|(_, f)| f.url.clone())
            .collect();
        urls.sort();
        urls.dedup();
        urls
    }
}

// Compare registrable domains, including private suffixes (e.g. github.io).
// IPs and single-label hosts only match exactly; substring matching is unsafe.
fn same_domain(main: &str, frame: &str) -> bool {
    let (Ok(main), Ok(frame)) = (Url::parse(main), Url::parse(frame)) else {
        return false;
    };
    if matches!(main.host(), Some(url::Host::Ipv4(_) | url::Host::Ipv6(_)))
        || matches!(frame.host(), Some(url::Host::Ipv4(_) | url::Host::Ipv6(_)))
    {
        return main.host() == frame.host();
    }
    let (Some(main), Some(frame)) = (main.host_str(), frame.host_str()) else {
        return false;
    };
    let main = main.trim_end_matches('.');
    let frame = frame.trim_end_matches('.');
    if main == frame {
        return true;
    }
    match (psl::domain_str(main), psl::domain_str(frame)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

pub(super) async fn navigate(
    page: &Page,
    url: &str,
    wait: bool,
    timeout: Duration,
    guard: tokio::sync::OwnedMutexGuard<()>,
) -> Result<PageInfo> {
    let deadline = Instant::now() + timeout;
    let mut session = timeout_at(deadline, Session::attach(page, guard))
        .await
        .map_err(|_| Error::Timeout(timeout))??;
    let mut state = Readiness {
        main: String::new(),
        frames: HashMap::new(),
        requests: HashMap::new(),
    };
    let mut navigation = None;
    let operation = async {
        session.command("Page.enable", json!({})).await?;
        session.command("Network.enable", json!({})).await?;
        session
            .command("Page.setLifecycleEventsEnabled", json!({"enabled": true}))
            .await?;
        let tree = session.command("Page.getFrameTree", json!({})).await?;
        state.main = tree["frameTree"]["frame"]["id"]
            .as_str()
            .unwrap_or_default()
            .into();
        let id = session.send("Page.navigate", json!({"url": url})).await?;
        loop {
            let message = session.next().await?;
            if message["id"].as_u64() == Some(id) {
                let result = response(message)?;
                if let Some(error) = result["errorText"].as_str() {
                    return Err(Error::ToolFailed {
                        tool: "browser_navigate",
                        message: error.into(),
                    });
                }
                if result["isDownload"].as_bool() == Some(true) {
                    return Err(Error::ToolFailed {
                        tool: "browser_navigate",
                        message: "Navigation started a download instead of a document".into(),
                    });
                }
                let loader = result["loaderId"].as_str().map(str::to_owned);
                if !wait || loader.is_none() {
                    return Ok(());
                }
                navigation = loader;
            } else {
                state.event(&message);
            }
            if navigation
                .as_deref()
                .is_some_and(|loader| state.main_ready(loader))
                && state.pending_frames().is_empty()
            {
                return Ok(());
            }
        }
    };
    let mut warnings = Vec::new();
    match timeout_at(deadline, operation).await {
        Ok(result) => result?,
        Err(_)
            if navigation
                .as_deref()
                .is_some_and(|loader| state.main_ready(loader)) =>
        {
            for url in state.pending_frames() {
                warnings.push(format!(
                    "Iframe did not become ready before the navigation timeout: {url}"
                ));
            }
        }
        Err(_) => return Err(Error::Timeout(timeout)),
    }
    session.stop_on_drop = false;

    warnings.extend(state.frame_warnings());
    for warning in &warnings {
        tracing::warn!(%warning, "Browser navigation completed with an iframe warning");
    }
    // Use raw Runtime.evaluate: Page.get_title() may itself wait for an execution
    // context, defeating wait_for_load=false while the new DOM is still parsing.
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        session.command(
            "Runtime.evaluate",
            json!({
                "expression": "({url:location.href,title:document.title})", "returnByValue": true,
            }),
        ),
    )
    .await
    .ok()
    .and_then(Result::ok)
    .unwrap_or_default();
    let value = &result["result"]["value"];
    Ok(PageInfo {
        url: value["url"]
            .as_str()
            .or_else(|| state.frames.get(&state.main).map(|f| f.url.as_str()))
            .unwrap_or(url)
            .into(),
        title: value["title"].as_str().unwrap_or_default().into(),
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::same_domain;
    #[test]
    fn registrable_domains_not_host_substrings() {
        for (a, b) in [
            ("https://browser.uber.com", "https://ui.uber.com"),
            ("https://a.example.co.uk", "https://b.example.co.uk"),
            ("http://localhost:8000", "http://localhost:9000"),
        ] {
            assert!(same_domain(a, b), "{a} vs {b}");
        }
        for (a, b) in [
            ("https://uber.com", "https://uber.com.evil.com"),
            ("https://example.co.uk", "https://other.co.uk"),
            ("https://a.github.io", "https://b.github.io"),
            ("http://127.0.0.1", "http://127.0.0.2"),
            ("http://127.0.0.1", "http://128.0.0.1"),
        ] {
            assert!(!same_domain(a, b), "{a} vs {b}");
        }
    }
}
