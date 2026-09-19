//! Chat operations (parity with commands/chats.rs).
//!
//! Differences from the desktop commands, by design:
//! * `avatar_path` stays `None` — avatars are served on demand by the
//!   /photo endpoint instead of being paths on someone else's disk.
//! * The chat cache is in server memory (replaced per listing) instead of
//!   the app's local SQLite.
//! * Progressive loading emits `chat-loaded` / `chats-loading-complete`
//!   into the event bus with an extra `accountId` field (the bus carries
//!   all accounts; desktop events are implicitly single-app).

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use grammers_client::types::Peer;
use grammers_session::defs::PeerRef;
use grammers_tl_types as tl;
use serde::Deserialize;
use vasya_core::events::EventSink;
use vasya_core::telegram::client_manager::TelegramClientWrapper;

use crate::auth::UserId;
use crate::context::ServerContext;
use crate::dto::Chat;
use crate::error::ApiError;
use crate::peer::resolve_peer;
use crate::routes::account_client;

/// Extract unread_count and is_muted from the raw TL Dialog enum.
fn extract_dialog_meta(raw: &tl::enums::Dialog) -> (i32, bool) {
    match raw {
        tl::enums::Dialog::Dialog(d) => {
            let muted = match &d.notify_settings {
                tl::enums::PeerNotifySettings::Settings(s) => {
                    // mute_until > 0 means muted (i32::MAX = "forever")
                    s.mute_until.map_or(false, |t| t > 0)
                }
            };
            (d.unread_count, muted)
        }
        tl::enums::Dialog::Folder(_) => (0, false),
    }
}

fn dialog_to_chat(dialog: &grammers_client::types::Dialog) -> (i64, Chat) {
    let peer = &dialog.peer;
    let (chat_type, is_forum) = match peer {
        Peer::User(_) => ("user", false),
        Peer::Group(g) => {
            let forum = match &g.raw {
                tl::enums::Chat::Channel(ch) => ch.forum,
                _ => false,
            };
            ("group", forum)
        }
        Peer::Channel(c) => ("channel", c.raw.forum),
    };

    let title = peer.name().unwrap_or("Unknown").to_string();
    let username = match peer {
        Peer::User(u) => u.username().map(|s| s.to_string()),
        Peer::Channel(c) => c.username().map(|s| s.to_string()),
        Peer::Group(g) => g.username().map(|s| s.to_string()),
    };

    let chat_id = PeerRef::from(peer).id.bot_api_dialog_id();

    let last_message = dialog.last_message.as_ref().map(|msg| {
        let text = msg.text();
        if text.chars().count() > 100 {
            let truncated: String = text.chars().take(100).collect();
            format!("{}...", truncated)
        } else {
            text.to_string()
        }
    });

    let (unread_count, is_muted) = extract_dialog_meta(&dialog.raw);

    (
        chat_id,
        Chat {
            id: chat_id,
            title,
            username,
            unread_count,
            chat_type: chat_type.to_string(),
            last_message,
            avatar_path: None,
            is_forum,
            is_muted,
        },
    )
}

/// Iterate all dialogs, fill the wrapper's peer cache, return chats in
/// dialog order. Optionally emits a `chat-loaded` event per chat.
async fn collect_chats(
    ctx: &ServerContext,
    wrapper: &TelegramClientWrapper,
    account_id: &str,
    emit_progress: bool,
) -> Result<Vec<Chat>, ApiError> {
    let mut dialogs = wrapper.client.iter_dialogs();
    let mut chats = Vec::new();

    while let Some(dialog) = dialogs
        .next()
        .await
        .map_err(|e| ApiError::telegram(format!("Failed to get dialogs: {e}")))?
    {
        let (chat_id, chat) = dialog_to_chat(&dialog);

        {
            let mut peers = wrapper.peers.write().await;
            peers.insert(chat_id, dialog.peer.clone());
        }

        if emit_progress {
            if let Ok(mut payload) = serde_json::to_value(&chat) {
                payload["accountId"] = serde_json::Value::String(account_id.to_string());
                ctx.events.emit("chat-loaded", payload);
            }
        }

        chats.push(chat);
    }

    ctx.chat_cache
        .write()
        .await
        .insert(account_id.to_string(), chats.clone());

    Ok(chats)
}

#[derive(Deserialize)]
pub struct ListChatsQuery {
    /// "cache" (default; like get_cached_chats) or "live" (like get_chats —
    /// fresh dialog iteration).
    #[serde(default)]
    pub source: Option<String>,
}

pub(crate) async fn list_chats_op(
    ctx: &Arc<ServerContext>,
    user: &UserId,
    account_id: &str,
    live: bool,
) -> Result<Vec<Chat>, ApiError> {
    let wrapper = account_client(ctx, user, account_id).await?;

    if !live {
        if let Some(cached) = ctx.chat_cache.read().await.get(account_id) {
            return Ok(cached.clone());
        }
    }

    collect_chats(ctx, &wrapper, account_id, false).await
}

pub async fn list_chats(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
    Path(account_id): Path<String>,
    Query(query): Query<ListChatsQuery>,
) -> Result<Json<Vec<Chat>>, ApiError> {
    let live = matches!(query.source.as_deref(), Some("live"));
    Ok(Json(list_chats_op(&ctx, &user.0, &account_id, live).await?))
}

/// Kick off progressive loading: results stream as `chat-loaded` events on
/// the bus and land in the chat cache.
pub(crate) async fn start_loading_chats_op(
    ctx: &Arc<ServerContext>,
    user: &UserId,
    account_id: &str,
) -> Result<(), ApiError> {
    let wrapper = account_client(ctx, user, account_id).await?;

    let ctx = ctx.clone();
    let account_id = account_id.to_string();
    tokio::spawn(async move {
        match collect_chats(&ctx, &wrapper, &account_id, true).await {
            Ok(chats) => {
                ctx.events.emit(
                    "chats-loading-complete",
                    serde_json::json!({ "accountId": account_id, "count": chats.len() }),
                );
            }
            Err(e) => {
                tracing::error!(account_id = %account_id, error = %e, "Chat loading failed");
            }
        }
    });

    Ok(())
}

pub async fn start_loading_chats(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
    Path(account_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    start_loading_chats_op(&ctx, &user.0, &account_id).await?;
    Ok(StatusCode::ACCEPTED)
}

pub(crate) async fn delete_and_leave_chat_op(
    ctx: &Arc<ServerContext>,
    user: &UserId,
    account_id: &str,
    chat_id: i64,
) -> Result<(), ApiError> {
    tracing::info!(account_id = %account_id, chat_id, "Delete and leave chat");
    let wrapper = account_client(ctx, user, account_id).await?;
    ctx.rate.check_mutation(account_id)?;

    let peer = resolve_peer(&wrapper, chat_id).await?;

    // delete_dialog handles all peer types: channels/megagroups ->
    // LeaveChannel, groups -> DeleteChatUser, users -> DeleteHistory
    wrapper
        .client
        .delete_dialog(&peer)
        .await
        .map_err(|e| ApiError::telegram(format!("Failed to delete and leave chat: {e}")))?;

    Ok(())
}

pub async fn delete_and_leave_chat(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
    Path((account_id, chat_id)): Path<(String, i64)>,
) -> Result<StatusCode, ApiError> {
    delete_and_leave_chat_op(&ctx, &user.0, &account_id, chat_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateGroupRequest {
    pub title: String,
    pub user_ids: Vec<i64>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreatedChatResponse {
    pub chat_id: i64,
}

pub(crate) async fn create_group_op(
    ctx: &Arc<ServerContext>,
    user: &UserId,
    account_id: &str,
    title: String,
    user_ids: &[i64],
) -> Result<i64, ApiError> {
    tracing::info!(account_id = %account_id, title = %title, "Creating group");
    let wrapper = account_client(ctx, user, account_id).await?;
    ctx.rate.check_mutation(account_id)?;

    let mut input_users = Vec::new();
    for uid in user_ids {
        let peer = resolve_peer(&wrapper, *uid).await?;
        let peer_ref = PeerRef::from(&peer);
        let input_user: tl::enums::InputUser = peer_ref.into();
        input_users.push(input_user);
    }

    let request = tl::functions::messages::CreateChat {
        users: input_users,
        title,
        ttl_period: None,
    };

    let result = wrapper
        .client
        .invoke(&request)
        .await
        .map_err(|e| ApiError::telegram(format!("Failed to create group: {e}")))?;

    let chat_id = match &result {
        tl::enums::messages::InvitedUsers::Users(invited) => match &invited.updates {
            tl::enums::Updates::Updates(u) => u
                .chats
                .first()
                .map(|c| match c {
                    tl::enums::Chat::Chat(chat) => -(chat.id as i64), // Bot API format for groups
                    tl::enums::Chat::Channel(ch) => {
                        let id = ch.id as i64;
                        -1_000_000_000_000 - id // Bot API format for channels/supergroups
                    }
                    _ => 0,
                })
                .unwrap_or(0),
            _ => 0,
        },
    };

    tracing::info!(account_id = %account_id, chat_id, "Group created");
    Ok(chat_id)
}

pub async fn create_group(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
    Path(account_id): Path<String>,
    Json(req): Json<CreateGroupRequest>,
) -> Result<Json<CreatedChatResponse>, ApiError> {
    let chat_id = create_group_op(&ctx, &user.0, &account_id, req.title, &req.user_ids).await?;
    Ok(Json(CreatedChatResponse { chat_id }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateChannelRequest {
    pub title: String,
    #[serde(default)]
    pub about: String,
    #[serde(default)]
    pub is_megagroup: bool,
}

pub(crate) async fn create_channel_op(
    ctx: &Arc<ServerContext>,
    user: &UserId,
    account_id: &str,
    title: String,
    about: String,
    is_megagroup: bool,
) -> Result<i64, ApiError> {
    tracing::info!(account_id = %account_id, title = %title, is_megagroup, "Creating channel");
    let wrapper = account_client(ctx, user, account_id).await?;
    ctx.rate.check_mutation(account_id)?;

    let request = tl::functions::channels::CreateChannel {
        broadcast: !is_megagroup,
        megagroup: is_megagroup,
        for_import: false,
        forum: false,
        title,
        about,
        geo_point: None,
        address: None,
        ttl_period: None,
    };

    let result = wrapper
        .client
        .invoke(&request)
        .await
        .map_err(|e| ApiError::telegram(format!("Failed to create channel: {e}")))?;

    let chat_id = match &result {
        tl::enums::Updates::Updates(u) => u
            .chats
            .first()
            .map(|c| match c {
                tl::enums::Chat::Channel(ch) => {
                    let id = ch.id as i64;
                    -1_000_000_000_000 - id
                }
                _ => 0,
            })
            .unwrap_or(0),
        _ => 0,
    };

    tracing::info!(account_id = %account_id, chat_id, "Channel created");
    Ok(chat_id)
}

pub async fn create_channel(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
    Path(account_id): Path<String>,
    Json(req): Json<CreateChannelRequest>,
) -> Result<Json<CreatedChatResponse>, ApiError> {
    let chat_id = create_channel_op(
        &ctx,
        &user.0,
        &account_id,
        req.title,
        req.about,
        req.is_megagroup,
    )
    .await?;
    Ok(Json(CreatedChatResponse { chat_id }))
}

/// Contacts = user-type chats from the cache (parity with get_contacts,
/// which filters the app's chat DB the same way).
pub(crate) async fn get_contacts_op(
    ctx: &Arc<ServerContext>,
    user: &UserId,
    account_id: &str,
) -> Result<Vec<Chat>, ApiError> {
    let wrapper = account_client(ctx, user, account_id).await?;

    let cached = ctx.chat_cache.read().await.get(account_id).cloned();
    let chats = match cached {
        Some(chats) => chats,
        None => collect_chats(ctx, &wrapper, account_id, false).await?,
    };

    Ok(chats
        .into_iter()
        .filter(|c| c.chat_type == "user")
        .collect())
}

pub async fn get_contacts(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
    Path(account_id): Path<String>,
) -> Result<Json<Vec<Chat>>, ApiError> {
    Ok(Json(get_contacts_op(&ctx, &user.0, &account_id).await?))
}
