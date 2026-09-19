//! Media download endpoints (parity with commands/media.rs). The desktop
//! commands return local file paths; over HTTP we stream the bytes and keep
//! a server-side disk cache.

use std::path::Path as FsPath;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::header;
use axum::response::Response;
use axum::{Extension, Json};
use grammers_client::types::Message as GrammersMessage;

use crate::auth::UserId;
use crate::context::ServerContext;
use crate::error::ApiError;
use crate::flood::with_flood_wait_retry;
use crate::peer::resolve_peer;
use crate::routes::account_client;

/// Read a cached file from disk into a bytes response.
///
/// We serve user-supplied bytes from the API origin, so the response is
/// hardened against content-sniffing / stored-XSS: `nosniff` pins the
/// declared type, a locked-down CSP (`default-src 'none'; sandbox`) prevents
/// any markup that slips through from executing script in our origin, and
/// `disposition` ("inline" for known-renderable media, "attachment" for
/// everything else) stops the browser treating a download as a page.
pub(crate) async fn serve_file(
    path: &FsPath,
    content_type: &str,
    disposition: &str,
) -> Result<Response, ApiError> {
    let bytes = tokio::fs::read(path).await.map_err(ApiError::internal)?;
    Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_DISPOSITION, disposition)
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(
            header::CONTENT_SECURITY_POLICY,
            "default-src 'none'; sandbox",
        )
        .body(axum::body::Body::from(bytes))
        .map_err(ApiError::internal)
}

/// Map a raw Telegram mime type to a `(content_type, disposition)` pair that
/// is safe to serve from the API origin. Only image (excluding SVG, which can
/// script), audio and video render inline; anything else — notably
/// `text/html`, `image/svg+xml`, `application/xhtml+xml`, `*/*+xml` — is
/// neutralised to an opaque download.
fn safe_serving(raw: &str) -> (String, &'static str) {
    let base = raw
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let inline = (base.starts_with("image/") && base != "image/svg+xml")
        || base.starts_with("audio/")
        || base.starts_with("video/");
    if inline {
        (base, "inline")
    } else {
        ("application/octet-stream".to_string(), "attachment")
    }
}

/// Get file extension from media type (ported from commands/media.rs).
fn media_extension(media: &grammers_client::types::Media) -> String {
    match media {
        grammers_client::types::Media::Photo(_) => "jpg".to_string(),
        grammers_client::types::Media::Document(doc) => doc
            .mime_type()
            .map(|mime| match mime {
                "audio/ogg" | "audio/opus" | "audio/ogg; codecs=opus" => "ogg",
                m if m.starts_with("video/") => "mp4",
                m if m.starts_with("audio/") => "mp3",
                m if m.starts_with("image/") => m.split('/').nth(1).unwrap_or("dat"),
                _ => "dat",
            })
            .unwrap_or("dat")
            .to_string(),
        _ => "dat".to_string(),
    }
}

fn media_content_type(media: &grammers_client::types::Media) -> String {
    match media {
        grammers_client::types::Media::Photo(_) => "image/jpeg".to_string(),
        grammers_client::types::Media::Document(doc) => doc
            .mime_type()
            .unwrap_or("application/octet-stream")
            .to_string(),
        _ => "application/octet-stream".to_string(),
    }
}

/// Download the media of one message and stream it back.
pub async fn download_media(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
    Path((account_id, chat_id, message_id)): Path<(String, i64, i32)>,
) -> Result<Response, ApiError> {
    tracing::info!(account_id = %account_id, chat_id, message_id, "Download media requested");

    let wrapper = account_client(&ctx, &user.0, &account_id).await?;
    let chat = resolve_peer(&wrapper, chat_id).await?;

    // Find the specific message (same window as the desktop command)
    let mut messages_iter = wrapper
        .client
        .iter_messages(&chat)
        .offset_id(message_id + 1)
        .limit(50);

    let mut target_message: Option<GrammersMessage> = None;
    while let Some(msg) = messages_iter
        .next()
        .await
        .map_err(|e| ApiError::telegram(format!("Failed to get messages: {e}")))?
    {
        if msg.id() == message_id {
            target_message = Some(msg);
            break;
        }
    }

    let message = target_message.ok_or_else(|| {
        ApiError::NotFound(format!("Message {message_id} not found in chat {chat_id}"))
    })?;

    let media = message
        .media()
        .ok_or_else(|| ApiError::NotFound("Message has no media".into()))?;

    // WebPage previews are not downloadable files
    if matches!(media, grammers_client::types::Media::WebPage(_)) {
        return Err(ApiError::NotFound("Message media is a link preview".into()));
    }

    let media_dir = ctx
        .media_dir
        .join(format!("chat_{}", chat_id.unsigned_abs()));
    tokio::fs::create_dir_all(&media_dir)
        .await
        .map_err(ApiError::internal)?;

    let extension = media_extension(&media);
    let timestamp = chrono::Utc::now().timestamp();
    let file_path = media_dir.join(format!("media_{}_{}.{}", message_id, timestamp, extension));

    // Download with FLOOD_WAIT retry + timeout (2 min max)
    let download_future =
        with_flood_wait_retry(|| async { wrapper.client.download_media(&media, &file_path).await });
    let downloaded = tokio::time::timeout(std::time::Duration::from_secs(120), download_future)
        .await
        .map_err(|_| ApiError::Telegram("Media download timed out".into()))?;
    downloaded.map_err(|e| ApiError::telegram(format!("Failed to download media: {e}")))?;

    let (content_type, disposition) = safe_serving(&media_content_type(&media));
    serve_file(&file_path, &content_type, disposition).await
}

/// Chat/user profile photo as image bytes, cached on disk.
pub async fn chat_photo(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
    Path((account_id, chat_id)): Path<(String, i64)>,
) -> Result<Response, ApiError> {
    let wrapper = account_client(&ctx, &user.0, &account_id).await?;

    let avatars_dir = ctx.media_dir.join("avatars");
    tokio::fs::create_dir_all(&avatars_dir)
        .await
        .map_err(ApiError::internal)?;
    let file_path = avatars_dir.join(format!("chat_{}.jpg", chat_id.unsigned_abs()));

    if !file_path.exists() {
        let peer = resolve_peer(&wrapper, chat_id).await?;

        let photo = with_flood_wait_retry(|| async {
            let mut photos = wrapper.client.iter_profile_photos(&peer);
            photos.next().await
        })
        .await
        .map_err(|e| ApiError::telegram(format!("Error getting profile photos: {e}")))?;

        let Some(photo) = photo else {
            return Err(ApiError::NotFound("No profile photo".into()));
        };

        with_flood_wait_retry(|| async { wrapper.client.download_media(&photo, &file_path).await })
            .await
            .map_err(|e| ApiError::telegram(format!("Failed to download profile photo: {e}")))?;
    }

    serve_file(&file_path, "image/jpeg", "inline").await
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserPhotosResponse {
    pub count: usize,
    pub urls: Vec<String>,
}

/// All profile photos: downloads them into the avatar cache and returns
/// URLs of the per-index endpoint (parity with get_user_photos, which
/// returns local file paths on desktop).
pub async fn user_photos(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
    Path((account_id, chat_id)): Path<(String, i64)>,
) -> Result<Json<UserPhotosResponse>, ApiError> {
    let wrapper = account_client(&ctx, &user.0, &account_id).await?;
    let peer = resolve_peer(&wrapper, chat_id).await?;

    let avatars_dir = ctx.media_dir.join("avatars");
    tokio::fs::create_dir_all(&avatars_dir)
        .await
        .map_err(ApiError::internal)?;

    let mut count = 0usize;
    let mut photos = wrapper.client.iter_profile_photos(&peer);
    loop {
        match photos.next().await {
            Ok(Some(photo)) => {
                let file_path = avatars_dir.join(format!(
                    "chat_{}_photo_{}.jpg",
                    chat_id.unsigned_abs(),
                    count
                ));
                if !file_path.exists() {
                    if let Err(e) = wrapper.client.download_media(&photo, &file_path).await {
                        tracing::warn!(error = %e, index = count, "Failed to download profile photo");
                        continue;
                    }
                }
                count += 1;
            }
            Ok(None) => break,
            Err(e) => {
                tracing::warn!(error = %e, "Error iterating profile photos");
                break;
            }
        }
    }

    let urls = (0..count)
        .map(|i| format!("/api/v1/accounts/{account_id}/chats/{chat_id}/photos/{i}"))
        .collect();
    Ok(Json(UserPhotosResponse { count, urls }))
}

pub async fn user_photo_by_index(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
    Path((account_id, chat_id, index)): Path<(String, i64, u32)>,
) -> Result<Response, ApiError> {
    // Ownership check even though we serve from cache.
    let _ = account_client(&ctx, &user.0, &account_id).await?;

    let file_path = ctx.media_dir.join("avatars").join(format!(
        "chat_{}_photo_{}.jpg",
        chat_id.unsigned_abs(),
        index
    ));
    if !file_path.exists() {
        return Err(ApiError::NotFound(
            "Photo not cached — call the photos listing first".into(),
        ));
    }
    serve_file(&file_path, "image/jpeg", "inline").await
}
