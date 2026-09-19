//! Message operations (parity with commands/messages.rs).

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::{Extension, Json};
use grammers_client::types::{Media, Message as GrammersMessage};
use grammers_session::defs::PeerRef;
use grammers_tl_types as tl;
use serde::Deserialize;
use vasya_core::media::classify_media_type;

use crate::auth::UserId;
use crate::context::ServerContext;
use crate::dto::{MediaInfo, Message};
use crate::error::ApiError;
use crate::peer::resolve_peer;
use crate::routes::account_client;

/// Extract media information from a message
fn extract_media_info(msg: &GrammersMessage) -> Option<Vec<MediaInfo>> {
    msg.media().map(|media| {
        let media_type = classify_media_type(&media).to_string();
        let (file_size, mime_type) = match &media {
            Media::Document(doc) => (
                Some(doc.size() as u64),
                doc.mime_type().map(|s| s.to_string()),
            ),
            Media::Photo(_) => (None, Some("image/jpeg".to_string())),
            _ => (None, None),
        };
        // Link preview metadata (Telegram-generated webPage), so the client can
        // render a rich card instead of a bare "Link Preview" placeholder.
        let (webpage_url, webpage_site_name, webpage_title, webpage_description) = match &media {
            Media::WebPage(wp) => match &wp.raw.webpage {
                tl::enums::WebPage::Page(page) => (
                    Some(page.url.clone()),
                    page.site_name.clone(),
                    page.title.clone(),
                    page.description.clone(),
                ),
                _ => (None, None, None, None),
            },
            _ => (None, None, None, None),
        };
        vec![MediaInfo {
            media_type,
            file_path: None,
            file_name: None,
            file_size,
            mime_type,
            webpage_url,
            webpage_site_name,
            webpage_title,
            webpage_description,
        }]
    })
}

fn message_to_dto(msg: &GrammersMessage, chat_id: i64) -> Message {
    let sender = msg.sender();
    Message {
        id: msg.id(),
        chat_id,
        from_user_id: sender
            .as_ref()
            .map(|s| PeerRef::from(&**s).id.bot_api_dialog_id()),
        sender_name: sender.and_then(|s| s.name().map(|n| n.to_string())),
        text: if msg.text().is_empty() {
            None
        } else {
            Some(msg.text().to_string())
        },
        date: msg.date().timestamp(),
        is_outgoing: msg.outgoing(),
        media: extract_media_info(msg),
    }
}

#[derive(Deserialize)]
pub struct GetMessagesQuery {
    pub offset_id: Option<i32>,
    pub limit: Option<usize>,
    pub topic_id: Option<i32>,
}

/// Upper bound on how many messages one request may ask for. Caps the
/// caller-supplied `limit` so it can never drive an unbounded
/// `Vec::with_capacity` (a single-request capacity-overflow panic / OOM DoS).
pub(crate) const MAX_MESSAGE_LIMIT: usize = 200;

/// Resolve and clamp a caller-supplied message `limit`: default 50, capped at
/// [`MAX_MESSAGE_LIMIT`]. Untrusted query input must never reach
/// `Vec::with_capacity` unclamped.
fn clamp_message_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(50).min(MAX_MESSAGE_LIMIT)
}

pub(crate) async fn get_messages_op(
    ctx: &Arc<ServerContext>,
    user: &UserId,
    account_id: &str,
    chat_id: i64,
    offset_id: Option<i32>,
    limit: Option<usize>,
    topic_id: Option<i32>,
) -> Result<Vec<Message>, ApiError> {
    let wrapper = account_client(ctx, user, account_id).await?;
    let chat = resolve_peer(&wrapper, chat_id).await?;
    let limit = clamp_message_limit(limit);

    // For forum topics, use messages.getReplies with msg_id = topic_id
    if let Some(tid) = topic_id {
        let input_peer: tl::enums::InputPeer = PeerRef::from(&chat).into();
        let request = tl::functions::messages::GetReplies {
            peer: input_peer,
            msg_id: tid,
            offset_id: offset_id.unwrap_or(0),
            offset_date: 0,
            add_offset: 0,
            limit: limit as i32,
            max_id: 0,
            min_id: 0,
            hash: 0,
        };

        let result = wrapper
            .client
            .invoke(&request)
            .await
            .map_err(|e| ApiError::telegram(format!("Failed to get topic messages: {e}")))?;

        let (raw_messages, raw_users, raw_chats) = match result {
            tl::enums::messages::Messages::Messages(m) => (m.messages, m.users, m.chats),
            tl::enums::messages::Messages::Slice(m) => (m.messages, m.users, m.chats),
            tl::enums::messages::Messages::ChannelMessages(m) => (m.messages, m.users, m.chats),
            tl::enums::messages::Messages::NotModified(_) => (Vec::new(), Vec::new(), Vec::new()),
        };

        // Build user/chat name lookup
        let mut names: std::collections::HashMap<i64, String> = std::collections::HashMap::new();
        for user in &raw_users {
            if let tl::enums::User::User(u) = user {
                let first = u.first_name.as_deref().unwrap_or("");
                let last = u.last_name.as_deref().unwrap_or("");
                let name = if last.is_empty() {
                    first.to_string()
                } else {
                    format!("{} {}", first, last)
                };
                names.insert(u.id, name);
            }
        }
        for chat in &raw_chats {
            match chat {
                tl::enums::Chat::Channel(ch) => {
                    names.insert(ch.id, ch.title.clone());
                }
                tl::enums::Chat::Chat(ch) => {
                    names.insert(ch.id, ch.title.clone());
                }
                _ => {}
            }
        }

        let messages: Vec<Message> = raw_messages
            .into_iter()
            .filter_map(|m| match m {
                tl::enums::Message::Message(msg) => {
                    let peer_to_id = |p: &tl::enums::Peer| match p {
                        tl::enums::Peer::User(u) => u.user_id,
                        tl::enums::Peer::Chat(c) => c.chat_id,
                        tl::enums::Peer::Channel(c) => c.channel_id,
                    };
                    let from_user_id = msg.from_id.as_ref().map(&peer_to_id);
                    let sender_name = msg
                        .from_id
                        .as_ref()
                        .and_then(|p| names.get(&peer_to_id(p)).cloned());
                    Some(Message {
                        id: msg.id,
                        chat_id,
                        from_user_id,
                        sender_name,
                        text: if msg.message.is_empty() {
                            None
                        } else {
                            Some(msg.message)
                        },
                        date: msg.date as i64,
                        is_outgoing: msg.out,
                        media: None, // Raw TL media parsing kept simple, as in the app
                    })
                }
                _ => None,
            })
            .collect();

        return Ok(messages);
    }

    // Regular messages (no topic)
    let mut messages_iter = wrapper.client.iter_messages(&chat);
    if let Some(offset) = offset_id {
        messages_iter = messages_iter.offset_id(offset);
    }

    let mut messages = Vec::with_capacity(limit);
    while let Some(msg) = messages_iter
        .next()
        .await
        .map_err(|e| ApiError::telegram(format!("Failed to get messages: {e}")))?
    {
        messages.push(message_to_dto(&msg, chat_id));
        if messages.len() >= limit {
            break;
        }
    }

    Ok(messages)
}

pub async fn get_messages(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
    Path((account_id, chat_id)): Path<(String, i64)>,
    Query(q): Query<GetMessagesQuery>,
) -> Result<Json<Vec<Message>>, ApiError> {
    Ok(Json(
        get_messages_op(
            &ctx,
            &user.0,
            &account_id,
            chat_id,
            q.offset_id,
            q.limit,
            q.topic_id,
        )
        .await?,
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendMessageRequest {
    pub text: String,
    pub topic_id: Option<i32>,
}

pub(crate) async fn send_message_op(
    ctx: &Arc<ServerContext>,
    user: &UserId,
    account_id: &str,
    chat_id: i64,
    text: String,
    topic_id: Option<i32>,
) -> Result<Message, ApiError> {
    if text.trim().is_empty() {
        return Err(ApiError::BadRequest("Message text cannot be empty".into()));
    }

    let wrapper = account_client(ctx, user, account_id).await?;
    ctx.rate.check_mutation(account_id)?;

    let chat = resolve_peer(&wrapper, chat_id).await?;

    let input_msg = if let Some(tid) = topic_id {
        grammers_client::InputMessage::new()
            .text(&text)
            .reply_to(Some(tid))
    } else {
        grammers_client::InputMessage::new().text(&text)
    };

    let sent = wrapper
        .client
        .send_message(&chat, input_msg)
        .await
        .map_err(|e| ApiError::telegram(format!("Failed to send message: {e}")))?;

    let mut dto = message_to_dto(&sent, chat_id);
    dto.text = Some(text);
    dto.is_outgoing = true;
    Ok(dto)
}

pub async fn send_message(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
    Path((account_id, chat_id)): Path<(String, i64)>,
    Json(req): Json<SendMessageRequest>,
) -> Result<Json<Message>, ApiError> {
    Ok(Json(
        send_message_op(&ctx, &user.0, &account_id, chat_id, req.text, req.topic_id).await?,
    ))
}

/// Send media: raw request body + metadata in headers (same contract as the
/// Tauri raw-IPC command: x-file-name / x-caption are percent-encoded).
pub async fn send_media(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
    Path((account_id, chat_id)): Path<(String, i64)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Message>, ApiError> {
    use percent_encoding::percent_decode_str;

    let header = |name: &str| -> Option<String> {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
    };
    let decoded = |value: String| -> Result<String, ApiError> {
        percent_decode_str(&value)
            .decode_utf8()
            .map(|s| s.to_string())
            .map_err(|_| ApiError::BadRequest("Invalid percent-encoding in header".into()))
    };

    let file_name = decoded(
        header("x-file-name")
            .ok_or_else(|| ApiError::BadRequest("missing x-file-name header".into()))?,
    )?;
    let mime_type = header("x-mime-type").unwrap_or_else(|| "application/octet-stream".into());
    let caption = header("x-caption").map(decoded).transpose()?;
    let topic_id = header("x-topic-id")
        .filter(|v| !v.is_empty())
        .map(|value| {
            value
                .parse::<i32>()
                .map_err(|_| ApiError::BadRequest("Invalid topic ID".into()))
        })
        .transpose()?;
    if topic_id.is_some_and(|id| id <= 0) {
        return Err(ApiError::BadRequest("Topic ID must be positive".into()));
    }
    let voice = header("x-voice").is_some_and(|v| v == "true" || v == "1");
    let duration = header("x-duration")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0)
        .min(24 * 60 * 60);
    if voice && !mime_type.starts_with("audio/") {
        return Err(ApiError::BadRequest(
            "Voice message must have audio MIME type".into(),
        ));
    }

    if body.is_empty() {
        return Err(ApiError::BadRequest("Empty media body".into()));
    }

    tracing::info!(
        account_id = %account_id,
        chat_id,
        file_name = %file_name,
        size = body.len(),
        "Sending media"
    );

    let wrapper = account_client(&ctx, &user.0, &account_id).await?;
    ctx.rate.check_mutation(&account_id)?;

    let chat = resolve_peer(&wrapper, chat_id).await?;

    // Preserve file extension so grammers can detect media type
    let ext = std::path::Path::new(&file_name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("bin");
    let tmp_path = std::env::temp_dir().join(format!("upload_{}.{}", uuid::Uuid::new_v4(), ext));
    tokio::fs::write(&tmp_path, &body)
        .await
        .map_err(|e| ApiError::internal(format!("Failed to write temp file: {e}")))?;

    let result: Result<GrammersMessage, ApiError> = async {
        let uploaded_file = wrapper
            .client
            .upload_file(&tmp_path)
            .await
            .map_err(|e| ApiError::telegram(format!("Failed to upload file: {e}")))?;

        // Images: grammers auto-detects from extension -> inputMediaUploadedPhoto.
        // Other files: explicit mime_type -> inputMediaUploadedDocument.
        let mut input_msg = grammers_client::InputMessage::new()
            .text(caption.unwrap_or_default())
            .file(uploaded_file);
        if !mime_type.starts_with("image/") {
            input_msg = input_msg.mime_type(&mime_type);
        }
        input_msg = input_msg.reply_to(topic_id);
        if voice {
            input_msg = input_msg.attribute(grammers_client::types::Attribute::Voice {
                duration: std::time::Duration::from_secs(duration),
                waveform: None,
            });
        }

        wrapper
            .client
            .send_message(&chat, input_msg)
            .await
            .map_err(|e| ApiError::telegram(format!("Failed to send media: {e}")))
    }
    .await;

    let _ = tokio::fs::remove_file(&tmp_path).await;
    let sent = result?;

    let mut dto = message_to_dto(&sent, chat_id);
    dto.is_outgoing = true;
    Ok(Json(dto))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForwardMessagesRequest {
    pub from_chat_id: i64,
    pub to_chat_id: i64,
    pub message_ids: Vec<i32>,
}

pub(crate) async fn forward_messages_op(
    ctx: &Arc<ServerContext>,
    user: &UserId,
    account_id: &str,
    from_chat_id: i64,
    to_chat_id: i64,
    message_ids: &[i32],
) -> Result<Vec<Option<i32>>, ApiError> {
    tracing::info!(
        account_id = %account_id,
        from_chat_id,
        to_chat_id,
        message_count = message_ids.len(),
        "Forwarding messages"
    );

    let wrapper = account_client(ctx, user, account_id).await?;
    ctx.rate.check_mutation(account_id)?;

    let from_peer = resolve_peer(&wrapper, from_chat_id).await?;
    let to_peer = resolve_peer(&wrapper, to_chat_id).await?;

    let forwarded = wrapper
        .client
        .forward_messages(
            PeerRef::from(&to_peer),
            message_ids,
            PeerRef::from(&from_peer),
        )
        .await
        .map_err(|e| ApiError::telegram(format!("Failed to forward messages: {e}")))?;

    Ok(forwarded
        .into_iter()
        .map(|m| m.map(|msg| msg.id()))
        .collect())
}

pub async fn forward_messages(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
    Path(account_id): Path<String>,
    Json(req): Json<ForwardMessagesRequest>,
) -> Result<Json<Vec<Option<i32>>>, ApiError> {
    Ok(Json(
        forward_messages_op(
            &ctx,
            &user.0,
            &account_id,
            req.from_chat_id,
            req.to_chat_id,
            &req.message_ids,
        )
        .await?,
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarkReadRequest {
    pub max_id: i32,
}

pub(crate) async fn mark_messages_read_op(
    ctx: &Arc<ServerContext>,
    user: &UserId,
    account_id: &str,
    chat_id: i64,
    max_id: i32,
) -> Result<(), ApiError> {
    let wrapper = account_client(ctx, user, account_id).await?;
    let chat = resolve_peer(&wrapper, chat_id).await?;

    match &chat {
        grammers_client::types::Peer::Channel(channel) => {
            // Channels and supergroups use channels.ReadHistory
            let ch = &channel.raw;
            let input_channel = tl::enums::InputChannel::Channel(tl::types::InputChannel {
                channel_id: ch.id,
                access_hash: ch.access_hash.unwrap_or(0),
            });

            wrapper
                .client
                .invoke(&tl::functions::channels::ReadHistory {
                    channel: input_channel,
                    max_id,
                })
                .await
                .map_err(|e| {
                    ApiError::telegram(format!("Failed to mark channel messages as read: {e}"))
                })?;
        }
        _ => {
            // Users and basic groups use messages.ReadHistory
            let input_peer: tl::enums::InputPeer = PeerRef::from(&chat).into();
            wrapper
                .client
                .invoke(&tl::functions::messages::ReadHistory {
                    peer: input_peer,
                    max_id,
                })
                .await
                .map_err(|e| ApiError::telegram(format!("Failed to mark messages as read: {e}")))?;
        }
    }

    Ok(())
}

pub async fn mark_messages_read(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
    Path((account_id, chat_id)): Path<(String, i64)>,
    Json(req): Json<MarkReadRequest>,
) -> Result<StatusCode, ApiError> {
    mark_messages_read_op(&ctx, &user.0, &account_id, chat_id, req.max_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct ChatSearchQuery {
    pub q: String,
    pub limit: Option<usize>,
}

pub(crate) async fn search_messages_op(
    ctx: &Arc<ServerContext>,
    user: &UserId,
    account_id: &str,
    chat_id: i64,
    q: &str,
    limit: Option<usize>,
) -> Result<Vec<Message>, ApiError> {
    if q.trim().is_empty() {
        return Ok(Vec::new());
    }

    let wrapper = account_client(ctx, user, account_id).await?;
    let chat = resolve_peer(&wrapper, chat_id).await?;

    let limit = clamp_message_limit(limit);
    let mut search_iter = wrapper.client.search_messages(&chat).query(q);

    let mut messages = Vec::with_capacity(limit);
    while let Some(msg) = search_iter
        .next()
        .await
        .map_err(|e| ApiError::telegram(format!("Failed to search messages: {e}")))?
    {
        messages.push(message_to_dto(&msg, chat_id));
        if messages.len() >= limit {
            break;
        }
    }

    Ok(messages)
}

pub async fn search_messages(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
    Path((account_id, chat_id)): Path<(String, i64)>,
    Query(query): Query<ChatSearchQuery>,
) -> Result<Json<Vec<Message>>, ApiError> {
    Ok(Json(
        search_messages_op(&ctx, &user.0, &account_id, chat_id, &query.q, query.limit).await?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_limit_is_clamped() {
        // Default when absent.
        assert_eq!(clamp_message_limit(None), 50);
        // Small values pass through.
        assert_eq!(clamp_message_limit(Some(10)), 10);
        // At the cap.
        assert_eq!(
            clamp_message_limit(Some(MAX_MESSAGE_LIMIT)),
            MAX_MESSAGE_LIMIT
        );
        // The DoS input: a huge usize must be capped, never reaching Vec::with_capacity.
        assert_eq!(clamp_message_limit(Some(usize::MAX)), MAX_MESSAGE_LIMIT);
        assert_eq!(clamp_message_limit(Some(500_000_000)), MAX_MESSAGE_LIMIT);
    }
}
