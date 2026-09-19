//! Account listing, logout and own avatar (parity with commands/auth.rs
//! logout/get_my_avatar).

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use serde::Serialize;

use crate::auth::UserId;
use crate::context::ServerContext;
use crate::error::ApiError;
use crate::flood::with_flood_wait_retry;
use crate::routes::account_client;

#[derive(Serialize, async_graphql::SimpleObject)]
#[serde(rename_all = "camelCase")]
pub struct AccountSummary {
    pub account_id: String,
    pub phone: String,
    pub connected: bool,
}

pub(crate) async fn list_accounts_op(ctx: &ServerContext, user: &UserId) -> Vec<AccountSummary> {
    let mut out = Vec::new();
    for account_id in ctx.accounts.list_for(&user.0) {
        let wrapper = ctx.manager.get_client(&account_id).await;
        out.push(AccountSummary {
            phone: wrapper
                .as_ref()
                .map(|w| w.phone.clone())
                .unwrap_or_default(),
            connected: wrapper.is_some(),
            account_id,
        });
    }
    out
}

pub async fn list_accounts(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
) -> Json<Vec<AccountSummary>> {
    Json(list_accounts_op(&ctx, &user.0).await)
}

pub(crate) async fn logout_op(
    ctx: &ServerContext,
    user: &UserId,
    account_id: &str,
) -> Result<(), ApiError> {
    tracing::info!(account_id = %account_id, "Logging out");
    ctx.accounts.ensure_access(&user.0, account_id)?;

    ctx.manager
        .remove_client(account_id)
        .await
        .map_err(|e| ApiError::internal(format!("Failed to logout: {e}")))?;
    ctx.accounts.release(account_id)?;
    ctx.chat_cache.write().await.remove(account_id);
    ctx.pending_logins.lock().await.remove(account_id);
    ctx.pending_passwords.lock().await.remove(account_id);
    Ok(())
}

pub async fn logout(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
    Path(account_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    logout_op(&ctx, &user.0, &account_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Own avatar as image bytes (the desktop command returns a local file path;
/// over HTTP the bytes themselves are the useful representation).
pub async fn my_avatar(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
    Path(account_id): Path<String>,
) -> Result<axum::response::Response, ApiError> {
    let wrapper = account_client(&ctx, &user.0, &account_id).await?;

    let avatars_dir = ctx.media_dir.join("avatars");
    tokio::fs::create_dir_all(&avatars_dir)
        .await
        .map_err(ApiError::internal)?;
    let file_path = avatars_dir.join(format!("me_{}.jpg", account_id));

    if !file_path.exists() {
        let me = wrapper
            .client
            .get_me()
            .await
            .map_err(|e| ApiError::telegram(format!("Failed to get user info: {e}")))?;
        let me_peer = grammers_client::types::Peer::User(me);

        let photo = with_flood_wait_retry(|| async {
            let mut photos = wrapper.client.iter_profile_photos(&me_peer);
            photos.next().await
        })
        .await
        .map_err(|e| ApiError::telegram(format!("Error getting own photos: {e}")))?;

        let Some(photo) = photo else {
            return Err(ApiError::NotFound("No profile photo".into()));
        };

        with_flood_wait_retry(|| async { wrapper.client.download_media(&photo, &file_path).await })
            .await
            .map_err(|e| ApiError::telegram(format!("Failed to download own avatar: {e}")))?;
    }

    crate::routes::media::serve_file(&file_path, "image/jpeg", "inline").await
}
