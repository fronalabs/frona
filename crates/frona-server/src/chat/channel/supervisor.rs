use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::http::Request;
use axum::response::Response;
use chrono::Utc;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

use crate::agent::service::AgentService;
use crate::auth::UserService;
use crate::chat::broadcast::{BroadcastEvent, BroadcastEventKind, BroadcastService, EntityAction};
use crate::chat::message::models::{DeliveryState, MessageRole, MessageStatus};
use crate::chat::message::repository::MessageRepository;
use crate::chat::service::ChatService;
use crate::contact::service::ContactService;
use crate::core::config::Config;
use crate::core::error::AppError;
use crate::core::supervisor::Supervisor;
use crate::credential::share::service::ShareService;
use crate::inference::tool_loop::InferenceEventKind;
use crate::policy::service::PolicyService;
use crate::space::service::SpaceService;
use crate::storage::StorageService;

use super::hitl::HitlDeliveryService;
use super::inbound::InboundDeliveryService;
use super::models::{
    Channel, ChannelAdapter, ChannelCtx, ChannelStatus, DispatchMode, ExternalMessage,
};
use super::outbound::OutboundDeliveryService;
use super::registry::ChannelRegistry;
use super::service::ChannelService;

const INBOUND_BUFFER: usize = 64;

const DELIVERY_RETRY_BATCH: u32 = 50;

/// Connect-readiness deadline for a normal (non-setup) attempt.
const CONNECT_DEADLINE: Duration = Duration::from_secs(30);
/// Longer deadline once a `SetupReady` (QR) has been shown — a human must scan.
const SETUP_DEADLINE: Duration = Duration::from_secs(180);
/// Cap on awaiting `on_disconnect` (which may join a blocking worker thread).
const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(10);

pub(super) fn channel_data_dir(
    storage: &crate::storage::service::StorageService,
    user_handle: &crate::core::Handle,
    channel_handle: &crate::core::Handle,
) -> std::path::PathBuf {
    storage.channel_data_path(user_handle, channel_handle)
}

// Lifecycle-only writes must not trigger a restart loop via the watcher.
fn channel_needs_restart(prior: &Channel, next: &Channel) -> bool {
    prior.provider != next.provider
        || prior.agent_id != next.agent_id
        || prior.dispatch_mode != next.dispatch_mode
        || prior.config != next.config
}

/// The live adapter + its per-attempt `ChannelCtx`, published by a running
/// `ChannelManager` once its connect attempt reaches `Connected`, so
/// `running_adapter` / webhook dispatch can route to it.
struct Live {
    adapter: Arc<dyn ChannelAdapter>,
    ctx: ChannelCtx,
}

type ChannelMap = Arc<Mutex<HashMap<String, Arc<ChannelManager>>>>;

/// One per running channel: the event-driven connection state machine. Stored as
/// `Arc<ChannelManager>` in the supervisor's map **and** driven as its own task
/// (`run`). Owns the per-channel cancel token + the published live handle.
pub(super) struct ChannelManager {
    channel_id: String,
    channel_cancel: CancellationToken,
    /// Root process-shutdown token (parent of `channel_cancel`) — lets `finish_stopped`
    /// tell a whole-process shutdown apart from a single-channel stop.
    shutdown_token: CancellationToken,
    config: Arc<Config>,
    channel_service: ChannelService,
    channel_registry: Arc<ChannelRegistry>,
    space_service: SpaceService,
    user_service: UserService,
    storage_service: StorageService,
    chat_service: ChatService,
    share_service: ShareService,
    broadcast_service: BroadcastService,
    outbound: Arc<OutboundDeliveryService>,
    hitl: Arc<HitlDeliveryService>,
    inbound: Arc<InboundDeliveryService>,
    live: Mutex<Option<Live>>,
}

/// Removes a channel's map entry when its `run` task ends — normal return OR panic —
/// but only if the map still holds *this* manager (`Arc::ptr_eq`), so a config-restart
/// that already swapped in a fresh manager is never clobbered. This is what lets
/// `find_dead` (= active − live map keys) detect a crashed supervisor task.
struct SlotGuard {
    channels: ChannelMap,
    id: String,
    manager: std::sync::Weak<ChannelManager>,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        let channels = self.channels.clone();
        let id = self.id.clone();
        let manager = self.manager.clone();
        tokio::spawn(async move {
            let mut map = channels.lock().await;
            let still_ours = map
                .get(&id)
                .zip(manager.upgrade())
                .is_some_and(|(cur, mgr)| Arc::ptr_eq(cur, &mgr));
            if still_ours {
                map.remove(&id);
            }
        });
    }
}

impl ChannelManager {
    /// The live adapter + its per-attempt ctx, if this channel currently has an active
    /// connection (published between `Connected` and the next teardown).
    pub(super) async fn live_handle(&self) -> Option<(Arc<dyn ChannelAdapter>, ChannelCtx)> {
        let live = self.live.lock().await;
        let live = live.as_ref()?;
        Some((live.adapter.clone(), live.ctx.clone()))
    }

    async fn mark(&self, status: ChannelStatus, error: Option<String>) {
        if let Err(e) = self
            .channel_service
            .mark_status(&self.channel_id, status, error)
            .await
        {
            tracing::warn!(channel_id = %self.channel_id, error = %e, "supervisor: mark_status failed");
        }
    }

    /// Cancel this attempt's pipelines + adapter worker, then run `on_disconnect`
    /// (bounded — it may join a blocking worker thread).
    async fn teardown(
        &self,
        attempt_cancel: &CancellationToken,
        adapter: &Arc<dyn ChannelAdapter>,
        ctx: &ChannelCtx,
    ) {
        attempt_cancel.cancel();
        if tokio::time::timeout(TEARDOWN_TIMEOUT, adapter.on_disconnect(ctx))
            .await
            .is_err()
        {
            tracing::warn!(channel_id = %self.channel_id, "on_disconnect timed out during teardown");
        }
    }

    /// Final cleanup when cancelled (disable / config-restart / shutdown): write the
    /// final `Disconnected` unless the process is shutting down. The `SlotGuard` removes
    /// the map entry when `run` returns.
    async fn finish_stopped(&self) {
        if !self.shutdown_token.is_cancelled() {
            self.mark(ChannelStatus::Disconnected, None).await;
            let _ = self.channel_service.clear_setup(&self.channel_id).await;
        }
    }

    /// The per-channel lifecycle state machine: connect → await `Connected` (with a
    /// deadline) → maintain → on `Disconnected` reconnect with global backoff
    /// (transient, forever) or `Failed` (terminal). Sole writer of `status` + the setup
    /// overlay; consumes the adapter's buffered `ChannelSignal` stream, publishing the
    /// live adapter into `self.live`. The `SlotGuard` handles map removal on exit.
    async fn run(self: Arc<Self>, channels: ChannelMap) {
        use super::signal::ChannelSignal;
        let _guard = SlotGuard {
            channels,
            id: self.channel_id.clone(),
            manager: Arc::downgrade(&self),
        };
        let cfg = self.config.channel.retry.clone();
        let channel_cancel = self.channel_cancel.clone();
        let mut attempt: u32 = 0;

        loop {
            let channel = match self.channel_service.find_by_id(&self.channel_id).await {
                Ok(c) => c,
                Err(_) => return,
            };

            self.mark(ChannelStatus::Connecting, None).await;

            let attempt_cancel = channel_cancel.child_token();
            let (sig_tx, mut sig_rx) = mpsc::channel::<ChannelSignal>(super::signal::SIGNAL_BUF);

            let (adapter, ctx) = match self.build_attempt(&channel, &attempt_cancel, sig_tx).await {
                Ok(v) => v,
                Err(e) => {
                    attempt_cancel.cancel();
                    self.mark(ChannelStatus::Failed, Some(e.to_string())).await;
                    let _ = self.channel_service.clear_setup(&self.channel_id).await;
                    return;
                }
            };

            // Publish the live adapter/ctx for webhook dispatch + running_adapter.
            *self.live.lock().await = Some(Live {
                adapter: adapter.clone(),
                ctx: ctx.clone(),
            });

            let mut disconnect: Option<(super::FailureKind, String)> =
                match adapter.on_connect(&ctx).await {
                    Ok(()) => None,
                    Err(e) => Some((super::FailureKind::Terminal, format!("on_connect: {e}"))),
                };

            if disconnect.is_none() {
                let mut connected = false;
                let deadline = tokio::time::sleep(CONNECT_DEADLINE);
                tokio::pin!(deadline);
                loop {
                    tokio::select! {
                        sig = sig_rx.recv() => match sig {
                            Some(ChannelSignal::SetupReady { config }) => {
                                let _ = self.channel_service.set_setup(&self.channel_id, config).await;
                                deadline.as_mut().reset(tokio::time::Instant::now() + SETUP_DEADLINE);
                            }
                            Some(ChannelSignal::Linked) => {
                                let _ = self.channel_service.clear_setup(&self.channel_id).await;
                            }
                            Some(ChannelSignal::Connected) => { connected = true; break; }
                            Some(ChannelSignal::Disconnected { kind, reason }) => {
                                disconnect = Some((kind, reason)); break;
                            }
                            None => {
                                disconnect = Some((super::FailureKind::Transient, "signal channel closed".into()));
                                break;
                            }
                        },
                        _ = &mut deadline => {
                            disconnect = Some((super::FailureKind::Transient, "ready timeout".into()));
                            break;
                        }
                        _ = channel_cancel.cancelled() => {
                            self.teardown(&attempt_cancel, &adapter, &ctx).await;
                            self.finish_stopped().await;
                            return;
                        }
                    }
                }

                if connected {
                    self.mark(ChannelStatus::Connected, None).await;
                    let _ = self.channel_service.clear_setup(&self.channel_id).await;
                    if let Err(e) = self.outbound.resume_deliveries(&channel).await {
                        tracing::warn!(channel_id = %self.channel_id, error = %e, "resume_deliveries at connect");
                    }
                    if let Err(e) = self.outbound.reconcile_message_delivery(&channel).await {
                        tracing::warn!(channel_id = %self.channel_id, error = %e, "reconcile_message_delivery at connect");
                    }
                    attempt = 0;

                    loop {
                        tokio::select! {
                            sig = sig_rx.recv() => match sig {
                                Some(ChannelSignal::Connected) => {}
                                Some(ChannelSignal::SetupReady { config }) => {
                                    let _ = self.channel_service.set_setup(&self.channel_id, config).await;
                                }
                                Some(ChannelSignal::Linked) => {
                                    let _ = self.channel_service.clear_setup(&self.channel_id).await;
                                }
                                Some(ChannelSignal::Disconnected { kind, reason }) => {
                                    disconnect = Some((kind, reason)); break;
                                }
                                None => {
                                    disconnect = Some((super::FailureKind::Transient, "signal channel closed".into()));
                                    break;
                                }
                            },
                            _ = channel_cancel.cancelled() => {
                                self.teardown(&attempt_cancel, &adapter, &ctx).await;
                                self.finish_stopped().await;
                                return;
                            }
                        }
                    }
                }
            }

            self.teardown(&attempt_cancel, &adapter, &ctx).await;
            *self.live.lock().await = None;

            let (kind, reason) =
                disconnect.unwrap_or((super::FailureKind::Transient, "unknown".into()));
            match kind {
                super::FailureKind::Terminal => {
                    self.mark(ChannelStatus::Failed, Some(reason)).await;
                    let _ = self.channel_service.clear_setup(&self.channel_id).await;
                    return;
                }
                super::FailureKind::Transient => {
                    self.mark(ChannelStatus::Reconnecting, Some(reason)).await;
                    attempt = attempt.saturating_add(1);
                    let factor = cfg
                        .backoff_multiplier
                        .powi(attempt.saturating_sub(1) as i32);
                    let delay = (cfg.initial_backoff_ms as f64 * factor)
                        .min(cfg.max_backoff_ms as f64) as u64;
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_millis(delay)) => {}
                        _ = channel_cancel.cancelled() => {
                            self.finish_stopped().await;
                            return;
                        }
                    }
                }
            }
        }
    }

    /// Builds one connect attempt: adapter + `ChannelCtx` (carrying `sig_tx`) and spawns
    /// the owned inbound/outbound pipelines on `attempt_cancel`. Does NOT call
    /// `on_connect` — `run` does that and consumes signals.
    async fn build_attempt(
        &self,
        channel: &Channel,
        attempt_cancel: &CancellationToken,
        sig_tx: mpsc::Sender<super::signal::ChannelSignal>,
    ) -> Result<(Arc<dyn ChannelAdapter>, ChannelCtx), AppError> {
        let factory = self
            .channel_registry
            .get_factory(&channel.provider)
            .ok_or_else(|| {
                AppError::Validation(format!(
                    "no in-process factory registered for provider {:?}",
                    channel.provider,
                ))
            })?;
        let config = self.channel_service.resolve_config(channel).await?;
        let adapter: Arc<dyn ChannelAdapter> = Arc::from(factory.create(config)?);
        let space = self
            .space_service
            .find_by_id(&channel.space_id)
            .await?
            .ok_or_else(|| {
                AppError::Validation(format!(
                    "channel {:?} references missing space {:?}",
                    channel.id, channel.space_id,
                ))
            })?;

        let (emit, rx) = mpsc::channel::<ExternalMessage>(INBOUND_BUFFER);

        let webhook_base = self.config.server.external_or_local_base_url();
        let webhook_url = format!(
            "{}{}/{}/{}",
            webhook_base.trim_end_matches('/'),
            super::WEBHOOK_PATH_PREFIX,
            channel.provider,
            channel.id,
        );

        let handle = self
            .user_service
            .find_by_id(&channel.user_id)
            .await?
            .map(|u| u.handle)
            .ok_or_else(|| {
                AppError::Validation(format!(
                    "channel {:?} references missing user {:?}",
                    channel.id, channel.user_id,
                ))
            })?;
        let data_dir = channel_data_dir(&self.storage_service, &handle, &channel.handle);
        if let Err(e) = std::fs::create_dir_all(&data_dir) {
            return Err(AppError::Internal(format!(
                "could not create channel data dir {}: {e}",
                data_dir.display(),
            )));
        }

        let ctx = ChannelCtx {
            space,
            channel: channel.clone(),
            emit,
            webhook_url,
            hitl: self.hitl.clone(),
            outbound: self.outbound.clone(),
            chat_service: self.chat_service.clone(),
            user_service: self.user_service.clone(),
            storage_service: self.storage_service.clone(),
            share_service: self.share_service.clone(),
            data_dir,
            base_url: self.config.server.external_or_local_base_url(),
            share_ttl_secs: self.config.share.ttl_secs,
            cancel: attempt_cancel.clone(),
            signals: super::signal::ChannelSignalSink::new(sig_tx, channel.id.clone()),
        };

        {
            let adapter = adapter.clone();
            let ctx = ctx.clone();
            let broadcast = self.broadcast_service.clone();
            let cancel = attempt_cancel.clone();
            let channel_id = channel.id.clone();
            tokio::spawn(async move {
                if let Err(e) = run_outbound(adapter, ctx, broadcast, cancel).await {
                    tracing::warn!(channel_id = %channel_id, error = %e, "channel outbound task exited with error");
                }
            });
        }
        {
            let inbound = self.inbound.clone();
            let cancel = attempt_cancel.clone();
            let channel_id = channel.id.clone();
            tokio::spawn(async move {
                if let Err(e) = inbound.run_pipeline(channel_id.clone(), rx, cancel).await {
                    tracing::warn!(channel_id = %channel_id, error = %e, "channel inbound pipeline exited with error");
                }
            });
        }

        Ok((adapter, ctx))
    }
}

pub struct ChannelSupervisor {
    channels: ChannelMap,
    config: Arc<Config>,
    shutdown_token: CancellationToken,
    channel_service: ChannelService,
    channel_registry: Arc<ChannelRegistry>,
    space_service: SpaceService,
    user_service: UserService,
    storage_service: StorageService,
    chat_service: ChatService,
    share_service: ShareService,
    broadcast_service: BroadcastService,
    message_repo: Arc<dyn MessageRepository>,
    outbound: Arc<OutboundDeliveryService>,
    hitl: Arc<HitlDeliveryService>,
    inbound: Arc<InboundDeliveryService>,
}

impl ChannelSupervisor {
    /// Builds the three delivery engines (`outbound` / `hitl` / `inbound`) internally so
    /// `hitl` can hold a `Weak` ref to the (module-private) channels map for re-delivery.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: Arc<Config>,
        shutdown_token: CancellationToken,
        channel_service: ChannelService,
        channel_registry: Arc<ChannelRegistry>,
        space_service: SpaceService,
        user_service: UserService,
        storage_service: StorageService,
        chat_service: ChatService,
        share_service: ShareService,
        broadcast_service: BroadcastService,
        message_repo: Arc<dyn MessageRepository>,
        agent_service: AgentService,
        contact_service: ContactService,
        policy_service: PolicyService,
        harness: Arc<crate::agent::harness::Harness>,
        task_executor: Arc<crate::agent::task::executor::TaskExecutor>,
    ) -> Self {
        let channels: ChannelMap = Arc::new(Mutex::new(HashMap::new()));
        let outbound = Arc::new(OutboundDeliveryService::new(
            message_repo.clone(),
            chat_service.clone(),
        ));
        let hitl = Arc::new(HitlDeliveryService::new(
            chat_service.clone(),
            harness,
            task_executor,
            outbound.clone(),
            Arc::downgrade(&channels),
        ));
        let inbound = Arc::new(InboundDeliveryService::new(
            channel_service.clone(),
            agent_service,
            chat_service.clone(),
            user_service.clone(),
            contact_service,
            policy_service,
        ));
        Self {
            channels,
            config,
            shutdown_token,
            channel_service,
            channel_registry,
            space_service,
            user_service,
            storage_service,
            chat_service,
            share_service,
            broadcast_service,
            message_repo,
            outbound,
            hitl,
            inbound,
        }
    }

    /// Boot-time channel-specific setup that `core::supervisor::run` does not cover:
    /// revert orphaned pairings + arm the intent-driven broadcast watcher. Initial arming
    /// of active channels is handled by `core::supervisor::run`'s restore pass (via the
    /// `Supervisor::find_running` + `start` trait methods). Named `boot` to avoid clashing
    /// with the trait's `start(&self, id)`.
    pub async fn boot(self: Arc<Self>) -> Result<(), AppError> {
        if let Err(e) = self.channel_service.revert_orphaned_pairings().await {
            tracing::warn!(error = %e, "ChannelSupervisor: failed to revert orphaned pairings");
        }
        self.clone().spawn_broadcast_watcher();
        Ok(())
    }

    pub async fn dispatch_inbound_webhook(
        &self,
        channel_id: &str,
        request: Request<Bytes>,
    ) -> Result<Response, AppError> {
        // Not running (never started, mid-(re)connect, or Failed) → 500 so the
        // provider retries later, rather than 404 (which many treat as permanent).
        let (adapter, ctx) = self.running_adapter(channel_id).await.ok_or_else(|| {
            AppError::Internal(format!(
                "channel {channel_id} is not currently connected — retry later"
            ))
        })?;
        adapter
            .on_webhook(&ctx, request)
            .await
            .map_err(|e| AppError::Internal(e.to_string()))
    }

    pub async fn running_adapter(
        &self,
        channel_id: &str,
    ) -> Option<(std::sync::Arc<dyn ChannelAdapter>, ChannelCtx)> {
        let manager = {
            let map = self.channels.lock().await;
            map.get(channel_id).cloned()?
        };
        manager.live_handle().await
    }

    fn spawn_broadcast_watcher(self: Arc<Self>) {
        let mut events = self.broadcast_service.subscribe_raw();
        let supervisor = self;
        tokio::spawn(async move {
            loop {
                let Some(event) = events.recv().await else {
                    break;
                };
                let BroadcastEventKind::EntityUpdated {
                    table,
                    record_id,
                    action,
                    ..
                } = &event.kind
                else {
                    continue;
                };
                if table != "channel" {
                    continue;
                }
                let supervisor = supervisor.clone();
                let record_id = record_id.clone();
                let action = *action;
                tokio::spawn(async move {
                    match action {
                        EntityAction::Created => {
                            if let Ok(c) = supervisor.channel_service.find_by_id(&record_id).await
                                && c.enabled
                            {
                                let _ = supervisor.start(&record_id).await;
                            }
                        }
                        EntityAction::Updated => {
                            let new_channel =
                                match supervisor.channel_service.find_by_id(&record_id).await {
                                    Ok(c) => c,
                                    Err(_) => {
                                        let _ = supervisor.stop(&record_id).await;
                                        return;
                                    }
                                };
                            // Compare against the running attempt's snapshot (if any).
                            let prior = {
                                let manager = {
                                    let map = supervisor.channels.lock().await;
                                    map.get(&record_id).cloned()
                                };
                                match manager {
                                    Some(manager) => manager
                                        .live
                                        .lock()
                                        .await
                                        .as_ref()
                                        .map(|l| l.ctx.channel.clone()),
                                    None => None,
                                }
                            };
                            let running = {
                                let map = supervisor.channels.lock().await;
                                map.contains_key(&record_id)
                            };
                            if new_channel.enabled && !running {
                                // Intent flipped on (or a not-yet-tracked enabled row).
                                let _ = supervisor.start(&record_id).await;
                            } else if !new_channel.enabled && running {
                                // Intent flipped off.
                                let _ = supervisor.stop(&record_id).await;
                            } else if let Some(prior) = prior {
                                // Config change on a running channel → restart.
                                if channel_needs_restart(&prior, &new_channel) {
                                    let _ = supervisor.stop(&record_id).await;
                                    let _ = supervisor.start(&record_id).await;
                                }
                                // Else a supervisor-authored status/overlay write → no-op.
                            }
                        }
                        EntityAction::Deleted => {
                            let _ = supervisor.stop(&record_id).await;
                        }
                    }
                });
            }
        });
    }

    /// Scheduler entry point: retry all messages whose delivery is due, over each
    /// channel's live adapter.
    pub async fn retry_due_deliveries(&self) -> Result<u64, AppError> {
        let due = self
            .message_repo
            .find_due_deliveries(Utc::now(), DELIVERY_RETRY_BATCH)
            .await?;
        let count = due.len() as u64;
        for msg in due {
            let chat = match self.chat_service.find_chat(&msg.chat_id).await? {
                Some(c) => c,
                None => continue,
            };
            let Some(channel_id) = chat.channel_id.as_deref() else {
                continue;
            };
            let Some((adapter, ctx)) = self.running_adapter(channel_id).await else {
                continue;
            };
            self.outbound
                .attempt_all_segments(msg, chat, adapter, ctx)
                .await;
        }
        Ok(count)
    }
}

/// Adopts the shared `core::supervisor` framework for boot-restore + level-triggered
/// re-arming + operator notifications (same as App/Mcp). The *inner* per-channel
/// event-driven reconnect loop (transient-forever / terminal→Failed) is unaffected —
/// this outer loop only ever re-arms a channel that has **no** live supervisor task.
#[async_trait::async_trait]
impl crate::core::supervisor::Supervisor for ChannelSupervisor {
    fn label(&self) -> &'static str {
        "channel"
    }

    async fn find_running(&self) -> Result<Vec<String>, AppError> {
        Ok(self
            .channel_service
            .find_active()
            .await?
            .into_iter()
            .map(|c| c.id)
            .collect())
    }

    /// Idempotent: spawn a per-channel `ChannelManager` task unless one is already
    /// tracked. Fire-and-forget (returns as soon as the task is spawned).
    async fn start(&self, id: &str) -> Result<(), AppError> {
        let channels = self.channels.clone();
        let channel_id = id.to_string();
        let manager = ChannelManager {
            channel_id: channel_id.clone(),
            channel_cancel: self.shutdown_token.child_token(),
            shutdown_token: self.shutdown_token.clone(),
            config: self.config.clone(),
            channel_service: self.channel_service.clone(),
            channel_registry: self.channel_registry.clone(),
            space_service: self.space_service.clone(),
            user_service: self.user_service.clone(),
            storage_service: self.storage_service.clone(),
            chat_service: self.chat_service.clone(),
            share_service: self.share_service.clone(),
            broadcast_service: self.broadcast_service.clone(),
            outbound: self.outbound.clone(),
            hitl: self.hitl.clone(),
            inbound: self.inbound.clone(),
            live: Mutex::new(None),
        };
        tokio::spawn(async move {
            let manager = {
                let mut map = channels.lock().await;
                if map.contains_key(&channel_id) {
                    return;
                }
                let manager = Arc::new(manager);
                map.insert(channel_id, manager.clone());
                manager
            };
            manager.run(channels).await;
        });
        Ok(())
    }

    async fn stop(&self, id: &str) -> Result<(), AppError> {
        let manager = {
            let mut map = self.channels.lock().await;
            map.remove(id)
        };
        if let Some(manager) = manager {
            manager.channel_cancel.cancel();
        }
        Ok(())
    }

    /// Enabled, non-`Failed` channels with **no** live supervisor entry (crashed or
    /// never-armed). `find_active` already excludes `Failed`.
    async fn find_dead(&self) -> Result<Vec<String>, AppError> {
        let active = self.channel_service.find_active().await?;
        let map = self.channels.lock().await;
        Ok(active
            .into_iter()
            .map(|c| c.id)
            .filter(|id| !map.contains_key(id))
            .collect())
    }

    /// Connection resilience is the inner loop's job (retry transient forever); the outer
    /// loop never gives up on re-arming an absent task, so no restart budget applies.
    async fn restart_count(&self, _id: &str) -> u32 {
        0
    }

    async fn mark_failed(&self, id: &str, reason: &str) -> Result<(), AppError> {
        self.channel_service
            .mark_status(id, ChannelStatus::Failed, Some(reason.to_string()))
            .await
            .map(|_| ())
    }

    async fn record_access(&self, _id: &str) {}

    async fn find_idle(&self, _idle_threshold: Duration) -> Result<Vec<String>, AppError> {
        Ok(Vec::new())
    }

    async fn mark_hibernated(&self, _id: &str) -> Result<(), AppError> {
        Ok(())
    }

    async fn owner_of(&self, id: &str) -> Result<String, AppError> {
        Ok(self.channel_service.find_by_id(id).await?.user_id)
    }

    async fn display_name(&self, id: &str) -> String {
        self.channel_service
            .find_by_id(id)
            .await
            .map(|c| c.provider)
            .unwrap_or_else(|_| id.to_string())
    }

    async fn notification_data(
        &self,
        id: &str,
        action: &str,
    ) -> crate::notification::models::NotificationData {
        crate::notification::models::NotificationData::Channel {
            channel_id: id.to_string(),
            action: action.to_string(),
        }
    }
}

async fn run_outbound(
    adapter: Arc<dyn ChannelAdapter>,
    ctx: ChannelCtx,
    broadcast: BroadcastService,
    cancel: CancellationToken,
) -> Result<(), AppError> {
    let space_id = ctx.space.id.clone();
    let mut events = broadcast.subscribe_raw();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                // Teardown (incl. `on_disconnect`) is owned by the supervisor.
                return Ok(());
            }
            event = events.recv() => {
                let Some(event) = event else {
                    tracing::info!(channel_id = %ctx.channel.id, "broadcast channel closed; exiting dispatcher");
                    return Ok(());
                };

                if let Err(e) = handle_outbound_event(adapter.clone(), &space_id, &ctx, &event).await {
                    tracing::warn!(channel_id = %ctx.channel.id, error = %e, "channel event dispatch failed");
                }
            }
        }
    }
}

async fn handle_outbound_event(
    adapter: Arc<dyn ChannelAdapter>,
    space_id: &str,
    ctx: &ChannelCtx,
    event: &BroadcastEvent,
) -> Result<(), AppError> {
    match &event.kind {
        BroadcastEventKind::EntityUpdated {
            table,
            record_id,
            action,
            space_id: ev_space_id,
            ..
        } if table == "message"
            && matches!(action, EntityAction::Created | EntityAction::Updated)
            && ev_space_id.as_deref() == Some(space_id) =>
        {
            let msg = ctx
                .chat_service
                .get_message(&event.user_id, record_id)
                .await?;
            if !matches!(msg.role, MessageRole::Agent | MessageRole::TaskCompletion) {
                return Ok(());
            }
            if !matches!(msg.status, Some(MessageStatus::Completed) | None) {
                tracing::debug!(
                    channel_id = %ctx.channel.id,
                    msg_id = %msg.id,
                    status = ?msg.status,
                    "outbound skip: message status not deliverable",
                );
                return Ok(());
            }
            let delivery_state = msg.delivery.as_ref().map(|d| d.state);
            if matches!(
                delivery_state,
                Some(DeliveryState::Sent) | Some(DeliveryState::Failed)
            ) {
                tracing::debug!(
                    channel_id = %ctx.channel.id,
                    msg_id = %msg.id,
                    delivery_state = ?delivery_state,
                    "outbound skip: delivery already terminal",
                );
                return Ok(());
            }
            // Per-message override: Message-mode channel may carry a Signal-mode reply.
            let effective_mode = msg.dispatch_mode.unwrap_or(ctx.channel.dispatch_mode);
            if effective_mode != DispatchMode::Message {
                tracing::debug!(
                    channel_id = %ctx.channel.id,
                    msg_id = %msg.id,
                    msg_mode = ?msg.dispatch_mode,
                    channel_mode = ?ctx.channel.dispatch_mode,
                    "outbound skip: effective mode is not Message",
                );
                return Ok(());
            }
            let chat = ctx
                .chat_service
                .get_chat(&event.user_id, &msg.chat_id)
                .await?;
            if chat.channel_id.is_none() {
                tracing::debug!(
                    channel_id = %ctx.channel.id,
                    msg_id = %msg.id,
                    chat_id = %chat.id,
                    "outbound skip: chat is not channel-bound",
                );
                return Ok(());
            }
            ctx.outbound.ensure_pending_delivery(&msg.id).await?;
            let msg = ctx
                .chat_service
                .get_message(&event.user_id, &msg.id)
                .await?;
            tracing::info!(
                channel_id = %ctx.channel.id,
                msg_id = %msg.id,
                chat_id = %chat.id,
                "outbound dispatch: starting segmented send loop",
            );
            ctx.outbound
                .attempt_all_segments(msg, chat, adapter.clone(), ctx.clone())
                .await;
            Ok(())
        }
        BroadcastEventKind::Inference(kind) => {
            if event.space_id.as_deref() != Some(space_id) {
                return Ok(());
            }
            let chat_id = match event.chat_id.as_deref() {
                Some(id) => id,
                None => return Ok(()),
            };
            let chat = match ctx.chat_service.get_chat(&event.user_id, chat_id).await {
                Ok(c) => c,
                Err(_) => return Ok(()),
            };
            if chat.channel_id.is_none() {
                return Ok(());
            }
            // Streaming hooks are best-effort. Log the classified failure but
            // don't bubble — one failed typing-indicator or token-edit
            // shouldn't kill the whole event loop.
            macro_rules! log_streaming_err {
                ($result:expr, $hook:literal) => {
                    if let Err(e) = $result {
                        tracing::warn!(
                            channel_id = %ctx.channel.id,
                            chat_id = %chat.id,
                            kind = ?e.kind,
                            error = %e.message,
                            "{} failed", $hook,
                        );
                    }
                };
            }
            match kind {
                InferenceEventKind::Start | InferenceEventKind::Resume { .. } => {
                    log_streaming_err!(
                        adapter.on_inference_start(&chat, ctx).await,
                        "on_inference_start"
                    );
                }
                InferenceEventKind::Text(text) => {
                    log_streaming_err!(adapter.on_text(&chat, text, ctx).await, "on_text");
                }
                InferenceEventKind::Reasoning(text) => {
                    log_streaming_err!(
                        adapter.on_reasoning(&chat, text, ctx).await,
                        "on_reasoning"
                    );
                }
                InferenceEventKind::ToolCall {
                    name, arguments, ..
                } => {
                    log_streaming_err!(
                        adapter.on_tool_call(&chat, name, arguments, ctx).await,
                        "on_tool_call"
                    );
                }
                InferenceEventKind::ToolResult {
                    id: _,
                    name,
                    success,
                    result,
                } => {
                    log_streaming_err!(
                        adapter
                            .on_tool_result(&chat, name, *success, result, ctx)
                            .await,
                        "on_tool_result"
                    );
                }
                InferenceEventKind::Done { .. }
                | InferenceEventKind::Cancelled { .. }
                | InferenceEventKind::Failed { .. } => {
                    log_streaming_err!(
                        adapter.on_inference_done(&chat, ctx).await,
                        "on_inference_done"
                    );
                }
                InferenceEventKind::Paused { reason, message } => {
                    log_streaming_err!(
                        adapter.on_inference_done(&chat, ctx).await,
                        "on_inference_done"
                    );
                    match reason {
                        crate::inference::tool_loop::PauseReason::Hitl => {
                            if let Err(e) = ctx
                                .outbound
                                .deliver_pending_hitls(&chat, &message.id, adapter.as_ref(), ctx)
                                .await
                            {
                                tracing::warn!(
                                    channel_id = %ctx.channel.id,
                                    chat_id = %chat.id,
                                    kind = ?e.kind,
                                    error = %e.message,
                                    "deliver_pending_hitls during Paused event failed",
                                );
                            }
                        }
                    }
                }
                // No adapter hook for infra-level events.
                InferenceEventKind::EntityUpdated { .. } | InferenceEventKind::Retry { .. } => {}
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn channel_error_kind_terminality() {
        use super::super::ChannelErrorKind;
        assert!(!ChannelErrorKind::Transient.is_terminal());
        assert!(ChannelErrorKind::Forbidden.is_terminal());
        assert!(ChannelErrorKind::NotFound.is_terminal());
        assert!(ChannelErrorKind::PayloadInvalid.is_terminal());
        assert!(ChannelErrorKind::PayloadTooLarge.is_terminal());
        assert!(ChannelErrorKind::Unauthorized.is_terminal());
        assert!(ChannelErrorKind::Other.is_terminal());
    }
}
