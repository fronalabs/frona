//! Channel inbound processing — two halves that never call each other directly; they are
//! bridged only by the DB broadcast bus:
//!
//! * `InboundDeliveryService` — persists inbound `ExternalMessage`s as domain `Message`s.
//! * `spawn_inference_dispatcher` — a broadcast subscriber that runs the agent turn on a
//!   new channel-inbound message. It alone needs the broad `AppState`
//!   (harness / agent_service / signal_service / prompts); the pipeline does not.

use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent::models::Agent;
use crate::agent::service::AgentService;
use crate::auth::UserService;
use crate::chat::broadcast::{BroadcastEventKind, EntityAction};
use crate::chat::message::models::{Message, MessageRole};
use crate::chat::service::ChatService;
use crate::contact::models::Contact;
use crate::contact::service::ContactService;
use crate::core::error::AppError;
use crate::core::state::AppState;
use crate::inference::conversation::ChannelConversationBuilder;
use crate::policy::models::{PolicyAction, PolicyContact};
use crate::policy::service::PolicyService;

use super::models::{Channel, ChatType, DispatchMode, ExternalMessage};
use super::service::ChannelService;

/// Stateless engine for the runtime inbound pipeline; one shared instance, like
/// `OutboundDeliveryService` / `HitlDeliveryService`.
pub struct InboundDeliveryService {
    channel_service: ChannelService,
    agent_service: AgentService,
    chat_service: ChatService,
    user_service: UserService,
    contact_service: ContactService,
    policy_service: PolicyService,
}

impl InboundDeliveryService {
    pub fn new(
        channel_service: ChannelService,
        agent_service: AgentService,
        chat_service: ChatService,
        user_service: UserService,
        contact_service: ContactService,
        policy_service: PolicyService,
    ) -> Self {
        Self {
            channel_service,
            agent_service,
            chat_service,
            user_service,
            contact_service,
            policy_service,
        }
    }

    /// Spawned per connect attempt by the supervisor on the attempt's cancel token.
    pub(super) async fn run_pipeline(
        self: Arc<Self>,
        channel_id: String,
        mut rx: mpsc::Receiver<ExternalMessage>,
        cancel: CancellationToken,
    ) -> Result<(), AppError> {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::info!(channel_id = %channel_id, "inbound pipeline cancelled");
                    return Ok(());
                }
                event = rx.recv() => {
                    let Some(event) = event else {
                        tracing::info!(channel_id = %channel_id, "inbound emit channel closed");
                        return Ok(());
                    };
                    if let Err(e) = self.process(&channel_id, event).await {
                        tracing::warn!(
                            channel_id = %channel_id,
                            error = %e,
                            "inbound external message processing failed",
                        );
                    }
                }
            }
        }
    }

    async fn process(
        &self,
        channel_id: &str,
        event: ExternalMessage,
    ) -> Result<Option<Message>, AppError> {
        // Re-fetch: the connect-time snapshot is stale — pairing flips user_address.
        let channel = self.channel_service.find_by_id(channel_id).await?;
        let channel = &channel;

        // Pairing is an overlay (pending code), not a status — check the field.
        let pairing_pending = channel
            .user_address
            .as_ref()
            .and_then(|ua| ua.pairing_code.as_ref())
            .is_some();
        if pairing_pending {
            let _ = self
                .channel_service
                .try_redeem_pairing(&channel.id, &event.sender_address, &event.content)
                .await?;
            return Ok(None);
        }

        if event.content.trim().is_empty() && event.attachments.is_empty() {
            tracing::debug!(
                channel_id = %channel.id,
                sender = %event.sender_address,
                "inbound dropped: empty content with no attachments",
            );
            return Ok(None);
        }

        let agent = self
            .agent_service
            .find_by_id(&channel.agent_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Agent {} not found", channel.agent_id)))?;

        let initial_title = event
            .sender_display_name
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(event.sender_address.as_str());
        let chat = self
            .chat_service
            .upsert_channel_chat(
                &channel.user_id,
                &channel.space_id,
                &channel.agent_id,
                &channel.id,
                &event.external_chat_id,
                Some(initial_title),
            )
            .await?;

        let user = self.user_service.find_by_id(&channel.user_id).await?;
        let address = event.sender_address.as_str();
        let is_self = channel
            .user_address
            .as_ref()
            .and_then(|ua| ua.address.as_deref())
            == Some(address);
        let real_contact: Option<Contact> = if is_self {
            None
        } else if let Some(ext_id) = event.sender_external_id.as_deref() {
            let display = event.sender_display_name.as_deref().unwrap_or(address);
            Some(
                self.contact_service
                    .upsert_by_channel_address(
                        &channel.user_id,
                        &channel.space_id,
                        &channel.provider,
                        ext_id,
                        Some(&channel.id),
                        display,
                    )
                    .await?,
            )
        } else {
            None
        };

        let sender_contact = match (&real_contact, is_self) {
            (Some(c), false) => PolicyContact::from_contact(c, address),
            (None, true) => synthesize_self_contact(user.as_ref(), address),
            (None, false) => PolicyContact::unresolved(&channel.user_id, address),
            (Some(_), true) => unreachable!("self-source never upserts a real Contact"),
        };

        let paired_addresses: Vec<String> = channel
            .user_address
            .as_ref()
            .and_then(|ua| ua.address.clone())
            .map(|a| vec![a])
            .unwrap_or_default();
        let effective = self
            .effective_dispatch_mode(channel, &agent, &sender_contact, &paired_addresses)
            .await?;
        let Some(effective) = effective else {
            tracing::info!(
                user_id = %channel.user_id,
                agent_id = %channel.agent_id,
                provider = %channel.provider,
                space_id = %channel.space_id,
                sender = %event.sender_address,
                contact_id = ?real_contact.as_ref().map(|c| c.id.as_str()),
                is_self = %is_self,
                mode = ?channel.dispatch_mode,
                "Inbound discarded — Cedar denied",
            );
            return Ok(None);
        };
        if effective != channel.dispatch_mode {
            tracing::info!(
                channel_id = %channel.id,
                sender = %event.sender_address,
                channel_mode = ?channel.dispatch_mode,
                effective_mode = ?effective,
                "Inbound authorized as signal fallback on Message-mode channel",
            );
        }

        let builder = Message::builder(&chat.id, MessageRole::User, event.content.clone())
            .from_address(event.sender_address.clone())
            .dispatch_mode(effective);
        let mut msg = builder.build();
        if let Some(c) = &real_contact {
            msg.contact_id = Some(c.id.clone());
        }
        msg.attachments = event.attachments.clone();

        let saved = self.chat_service.persist_inbound_message(&msg).await?;
        Ok(Some(saved))
    }

    /// Message-mode channels fall back to `ReceiveSignal` when `ReceiveMessage`
    /// denies (covers agents with an open watch). Signal-mode only checks `ReceiveSignal`.
    async fn effective_dispatch_mode(
        &self,
        channel: &Channel,
        agent: &Agent,
        sender_contact: &PolicyContact,
        paired_addresses: &[String],
    ) -> Result<Option<DispatchMode>, AppError> {
        let receive_message = || PolicyAction::ReceiveMessage {
            connector_id: channel.space_id.clone(),
            channel_handle: channel.handle.clone(),
            sender: sender_contact.clone(),
            paired_addresses: paired_addresses.to_vec(),
        };
        let receive_signal = || PolicyAction::ReceiveSignal {
            connector_id: channel.space_id.clone(),
            channel_handle: channel.handle.clone(),
            sender: sender_contact.clone(),
            paired_addresses: paired_addresses.to_vec(),
        };

        if channel.dispatch_mode == DispatchMode::Message {
            let decision = self
                .policy_service
                .authorize(&channel.user_id, agent, receive_message())
                .await?;
            if decision.allowed {
                return Ok(Some(DispatchMode::Message));
            }
        }

        let decision = self
            .policy_service
            .authorize(&channel.user_id, agent, receive_signal())
            .await?;
        if decision.allowed {
            return Ok(Some(DispatchMode::Signal));
        }
        Ok(None)
    }
}

fn synthesize_self_contact(user: Option<&crate::auth::User>, address: &str) -> PolicyContact {
    let (id, user_id, name) = match user {
        Some(u) => (u.id.clone(), u.id.clone(), u.name.clone()),
        None => (String::new(), String::new(), String::new()),
    };
    PolicyContact {
        id,
        user_id,
        name,
        address: address.to_string(),
        addresses: vec![address.to_string()],
    }
}

pub fn spawn_inference_dispatcher(state: AppState) {
    let mut events = state.broadcast_service.subscribe_raw();
    let shutdown = state.shutdown_token.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                () = shutdown.cancelled() => {
                    tracing::info!("Channel inbound loop stopping for shutdown");
                    break;
                }
                event = events.recv() => {
                    let Some(event) = event else { break };
                    let BroadcastEventKind::EntityUpdated {
                        table, record_id, action, ..
                    } = &event.kind else { continue };
                    if table != "message" || *action != EntityAction::Created {
                        continue;
                    }
                    if let Err(e) = handle_inbound_message(&state, &event.user_id, record_id).await {
                        tracing::warn!(
                            user_id = %event.user_id,
                            message_id = %record_id,
                            error = %e,
                            "Channel inbound dispatch failed",
                        );
                    }
                }
            }
        }
    });
}

async fn handle_inbound_message(
    state: &AppState,
    user_id: &str,
    message_id: &str,
) -> Result<(), AppError> {
    let Some(msg) = state.chat_service.find_message(message_id).await? else {
        return Ok(());
    };
    if !matches!(msg.role, MessageRole::User) {
        return Ok(());
    }
    // `from_address` distinguishes channel-inbound from web-submitted; the
    // web route already triggers its own inference, fanning out here would
    // run two loops on the same turn.
    if msg.from_address.is_none() {
        return Ok(());
    }

    let Some(chat) = state.chat_service.find_chat(&msg.chat_id).await? else {
        return Ok(());
    };

    let space = if let Some(space_id) = chat.space_id.as_deref() {
        state.space_service.find_by_id(space_id).await?
    } else {
        None
    };

    let channel_row = match space.as_ref() {
        Some(s) => state.channel_service.find_by_space(&s.id).await?,
        None => None,
    };
    let Some(channel_row) = channel_row else {
        tracing::debug!(
            chat_id = %msg.chat_id,
            space_id = ?chat.space_id,
            "no channel bound to chat — inbound message persisted but inference will not fire",
        );
        return Ok(());
    };
    let channel = channel_row.provider.clone();
    // Legacy rows pre-dating `msg.dispatch_mode` fall back to the channel's mode.
    let mode = msg.dispatch_mode.unwrap_or(channel_row.dispatch_mode);

    let chat_type = ChatType::from_chat(&chat);
    let sender = msg.from_address.as_deref();

    let signal_service = state.signal_service();
    let awaiting_categories = signal_service.pending_category_hints(user_id).await;

    if matches!(mode, DispatchMode::Signal) {
        // dispatch_mode=Signal causes `attempt_send` to refuse delivery.
        signal_service
            .process_inbound_extract(
                &state.chat_service,
                state.chat_service.model_providers(),
                &channel_row,
                &chat,
                &msg,
                &awaiting_categories,
            )
            .await?;
        return Ok(());
    }

    let inbound_prompt = compose_inbound_prompt(
        state,
        mode,
        &channel,
        &chat.id,
        chat_type,
        sender,
        &awaiting_categories,
    );

    let agent_msg = state
        .chat_service
        .create_executing_agent_message(&chat.id, &chat.agent_id)
        .await?;

    let cancel_token = CancellationToken::new();

    let builder = Box::new(ChannelConversationBuilder {
        user_service: state.user_service.clone(),
        storage_service: state.storage_service.clone(),
        agent_service: state.agent_service.clone(),
        channel,
        sender: sender.map(String::from),
        inbound_prompt,
    });

    state
        .harness
        .run_turn(
            user_id,
            &chat.id,
            &agent_msg.id,
            cancel_token,
            builder,
            &[],
            None,
        )
        .await;
    Ok(())
}

fn compose_inbound_prompt(
    state: &AppState,
    mode: DispatchMode,
    channel: &str,
    chat_id: &str,
    chat_type: ChatType,
    sender: Option<&str>,
    awaiting_categories: &[(String, String)],
) -> Option<String> {
    if matches!(mode, DispatchMode::Message) && awaiting_categories.is_empty() {
        return None;
    }
    let sender_block = sender.map(|s| format!(" from {s}")).unwrap_or_default();
    let categories_block = if awaiting_categories.is_empty() {
        String::new()
    } else {
        let awaiting_list = awaiting_categories
            .iter()
            .map(|(cat, info)| format!("- {cat}: {info}"))
            .collect::<Vec<_>>()
            .join("\n");
        state
            .prompts
            .read_with_vars(
                "channel/categories.md",
                &[("awaiting_categories", &awaiting_list)],
            )
            .unwrap_or_default()
    };
    let vars: &[(&str, &str)] = &[
        ("channel", channel),
        ("sender_block", &sender_block),
        ("chat_id", chat_id),
        ("chat_type", chat_type.as_str()),
        ("categories_block", &categories_block),
    ];
    let path = match mode {
        DispatchMode::Message => "channel/message.md",
        DispatchMode::Signal => "channel/signal.md",
    };
    Some(state.prompts.read_with_vars(path, vars).unwrap_or_default())
}
