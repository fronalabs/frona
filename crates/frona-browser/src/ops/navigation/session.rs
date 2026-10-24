//! A short-lived CDP session for navigation with caller-controlled readiness.
//!
//! chromiumoxide 0.9 intercepts Page.navigate and waits for every descendant's
//! load event, including unstarted lazy frames. A nested session lets us send
//! the native command and consume ordered events without that implicit wait.
//! Attach through the existing page session so its primary session stays intact.
use chromiumoxide::cdp::browser_protocol::target::{
    AttachToTargetParams, DetachFromTargetParams, EventReceivedMessageFromTarget, SessionId,
};
use chromiumoxide::{Command, Method, Page, types::MethodId};
use futures::{StreamExt, stream::BoxStream};
use serde::Serialize;
use serde_json::{Value, json};
use std::sync::Arc;

use crate::{Result, error::Error};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SendMessage {
    session_id: SessionId,
    message: String,
}
// Still supported by Chrome, but omitted from the generated CDP bindings.
impl Method for SendMessage {
    fn identifier(&self) -> MethodId {
        "Target.sendMessageToTarget".into()
    }
}
impl Command for SendMessage {
    type Response = Value;
}

pub(super) struct Session {
    page: Page,
    id: SessionId,
    events: BoxStream<'static, Arc<EventReceivedMessageFromTarget>>,
    sequence: u64,
    pub stop_on_drop: bool,
    guard: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl Session {
    pub async fn attach(page: &Page, guard: tokio::sync::OwnedMutexGuard<()>) -> Result<Self> {
        let events = page
            .event_listener::<EventReceivedMessageFromTarget>()
            .await?
            .boxed();
        let id = page
            .execute(AttachToTargetParams::new(page.target_id().clone()))
            .await?
            .result
            .session_id;
        Ok(Self {
            page: page.clone(),
            id,
            events,
            sequence: 0,
            stop_on_drop: false,
            guard: Some(guard),
        })
    }

    pub async fn send(&mut self, method: &str, params: Value) -> Result<u64> {
        self.sequence += 1;
        if method == "Page.navigate" {
            self.stop_on_drop = true;
        }
        self.page
            .execute(SendMessage {
                session_id: self.id.clone(),
                message: json!({"id": self.sequence, "method": method, "params": params})
                    .to_string(),
            })
            .await?;
        Ok(self.sequence)
    }

    pub async fn next(&mut self) -> Result<Value> {
        while let Some(event) = self.events.next().await {
            if event.session_id == self.id {
                return Ok(serde_json::from_str(&event.message)?);
            }
        }
        Err(Error::Disconnected)
    }

    pub async fn command(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.send(method, params).await?;
        loop {
            let message = self.next().await?;
            if message["id"].as_u64() == Some(id) {
                return response(message);
            }
        }
    }
}

pub(super) fn response(message: Value) -> Result<Value> {
    if let Some(error) = message.get("error") {
        return Err(Error::ToolFailed {
            tool: "browser_navigate",
            message: error.to_string(),
        });
    }
    message.get("result").cloned().ok_or(Error::Disconnected)
}

impl Drop for Session {
    fn drop(&mut self) {
        let page = self.page.clone();
        let command = DetachFromTargetParams::builder()
            .session_id(self.id.clone())
            .build();
        let stop = self.stop_on_drop;
        let guard = self.guard.take();
        // Keep the navigation lock until cleanup completes. A cancelled request
        // must not stop a newer navigation that has already taken over the page.
        tokio::spawn(async move {
            let _guard = guard;
            let cleanup = async {
                if stop {
                    let _ = page
                        .execute(
                            chromiumoxide::cdp::browser_protocol::page::StopLoadingParams::default(
                            ),
                        )
                        .await;
                }
                let _ = page.execute(command).await;
            };
            let _ = tokio::time::timeout(std::time::Duration::from_secs(1), cleanup).await;
        });
    }
}
