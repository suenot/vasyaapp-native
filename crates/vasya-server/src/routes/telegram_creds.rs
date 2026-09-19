//! Telegram API credentials (api_id / api_hash): per-user (opt-in) + global.
//!
//! Each user may optionally store their OWN api_id/api_hash; when set, their
//! logins use them, otherwise logins fall back to the server's global default
//! (env `TELEGRAM_API_ID`/`TELEGRAM_API_HASH`, runtime-overridable by an admin).
//!
//! Privilege model:
//! - `GET/PUT/DELETE /telegram/credentials` — the caller's OWN credentials.
//! - `PUT /admin/telegram/credentials` — the server-global default; **admin
//!   only** (the admin check is enforced here; agent keys never reach this
//!   route — `policy.rs` makes `/admin/*` and `/telegram/credentials`
//!   human-session-only).
//!
//! The api_hash is a secret: stored on disk but never returned in full (GET
//! masks it), mirroring the STT Deepgram-key handling.

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::State;
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};

use crate::auth::UserId;
use crate::context::ServerContext;
use crate::error::ApiError;

/// Per-user Telegram credentials persisted on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredTgCreds {
    api_id: i32,
    api_hash: String,
}

fn creds_path(ctx: &ServerContext, user: &str) -> PathBuf {
    // `user` is validated (`is_safe_user_id`) at the auth choke point, so it is
    // safe as a path segment here.
    ctx.data_dir
        .join("telegram-creds")
        .join(user)
        .join("creds.json")
}

async fn load_user_creds(
    ctx: &ServerContext,
    user: &str,
) -> Result<Option<StoredTgCreds>, ApiError> {
    match tokio::fs::read(creds_path(ctx, user)).await {
        Ok(raw) => Ok(Some(
            serde_json::from_slice(&raw).map_err(ApiError::internal)?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ApiError::internal(e)),
    }
}

async fn save_user_creds(
    ctx: &ServerContext,
    user: &str,
    creds: &StoredTgCreds,
) -> Result<(), ApiError> {
    let raw = serde_json::to_vec_pretty(creds).map_err(ApiError::internal)?;
    write_private_file(&creds_path(ctx, user), &raw).await
}

/// Write a secret file owner-only: parent dir `0700`, file `0600` (Unix), via a
/// temp file + atomic rename so the api_hash is never briefly world-readable.
async fn write_private_file(path: &std::path::Path, bytes: &[u8]) -> Result<(), ApiError> {
    if let Some(parent) = path.parent() {
        let mut b = tokio::fs::DirBuilder::new();
        b.recursive(true);
        #[cfg(unix)]
        b.mode(0o700);
        b.create(parent).await.map_err(ApiError::internal)?;
    }
    let tmp = path.with_extension("json.tmp");
    {
        let mut opts = tokio::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        opts.mode(0o600);
        let mut f = opts.open(&tmp).await.map_err(ApiError::internal)?;
        use tokio::io::AsyncWriteExt;
        f.write_all(bytes).await.map_err(ApiError::internal)?;
        f.flush().await.map_err(ApiError::internal)?;
    }
    tokio::fs::rename(&tmp, path)
        .await
        .map_err(ApiError::internal)
}

async fn clear_user_creds(ctx: &ServerContext, user: &str) -> Result<(), ApiError> {
    match tokio::fs::remove_file(creds_path(ctx, user)).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(ApiError::internal(e)),
    }
}

/// Resolve the (api_id, api_hash) a login by `user` should use: the user's own
/// credentials if set, otherwise the server's global default. `None` if neither
/// is configured.
pub(crate) async fn resolve_credentials(
    ctx: &ServerContext,
    user: &str,
) -> Result<Option<(i32, String)>, ApiError> {
    if let Some(c) = load_user_creds(ctx, user).await? {
        if c.api_id != 0 && !c.api_hash.is_empty() {
            return Ok(Some((c.api_id, c.api_hash)));
        }
    }
    let (gid, ghash) = (ctx.manager.api_id(), ctx.manager.api_hash());
    if gid != 0 && !ghash.is_empty() {
        return Ok(Some((gid, ghash)));
    }
    Ok(None)
}

fn mask_hash(hash: &str) -> String {
    let visible: String = hash
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("••••{visible}")
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TgCredentialsResponse {
    /// Whether a usable credential exists for this caller (own or global).
    pub configured: bool,
    /// "user" (caller set their own), "global" (falling back), or "none".
    pub source: &'static str,
    /// The api_id in effect — api_id is not secret, safe to return.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_id: Option<i32>,
    /// Masked preview of the caller's OWN stored api_hash, if any. The global
    /// hash is never revealed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_hash_masked: Option<String>,
    /// Whether the caller is a server admin (may set the global default).
    pub is_admin: bool,
}

async fn build_response(ctx: &ServerContext, uid: &str) -> Result<TgCredentialsResponse, ApiError> {
    let is_admin = ctx.is_admin(uid);
    let own = load_user_creds(ctx, uid)
        .await?
        .filter(|c| c.api_id != 0 && !c.api_hash.is_empty());
    if let Some(c) = own {
        return Ok(TgCredentialsResponse {
            configured: true,
            source: "user",
            api_id: Some(c.api_id),
            api_hash_masked: Some(mask_hash(&c.api_hash)),
            is_admin,
        });
    }
    let gid = ctx.manager.api_id();
    let configured = gid != 0 && !ctx.manager.api_hash().is_empty();
    Ok(TgCredentialsResponse {
        configured,
        source: if configured { "global" } else { "none" },
        api_id: configured.then_some(gid),
        api_hash_masked: None, // the global hash is never exposed
        is_admin,
    })
}

pub async fn get_credentials(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
) -> Result<Json<TgCredentialsResponse>, ApiError> {
    Ok(Json(build_response(&ctx, &user.0 .0).await?))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateTgCredentials {
    pub api_id: i32,
    pub api_hash: String,
}

fn validate_creds(c: &UpdateTgCredentials) -> Result<(), ApiError> {
    if c.api_id <= 0 {
        return Err(ApiError::BadRequest(
            "api_id must be a positive integer".into(),
        ));
    }
    let h = c.api_hash.trim();
    if h.len() < 8 || h.len() > 64 || !h.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(ApiError::BadRequest(
            "api_hash must be 8-64 alphanumeric characters".into(),
        ));
    }
    Ok(())
}

/// `PUT /telegram/credentials` — set the CALLER's own credentials (opt-in).
pub async fn put_user_credentials(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
    Json(req): Json<UpdateTgCredentials>,
) -> Result<Json<TgCredentialsResponse>, ApiError> {
    validate_creds(&req)?;
    save_user_creds(
        &ctx,
        &user.0 .0,
        &StoredTgCreds {
            api_id: req.api_id,
            api_hash: req.api_hash.trim().to_string(),
        },
    )
    .await?;
    tracing::info!(user = %user.0 .0, api_id = req.api_id, "Per-user Telegram credentials set");
    Ok(Json(build_response(&ctx, &user.0 .0).await?))
}

/// `DELETE /telegram/credentials` — clear the caller's own credentials (fall
/// back to the global default).
pub async fn delete_user_credentials(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
) -> Result<Json<TgCredentialsResponse>, ApiError> {
    clear_user_creds(&ctx, &user.0 .0).await?;
    tracing::info!(user = %user.0 .0, "Per-user Telegram credentials cleared");
    Ok(Json(build_response(&ctx, &user.0 .0).await?))
}

/// `PUT /admin/telegram/credentials` — set the server-global default
/// credentials. Admin + human only (agent keys can't reach `/admin/*`; the
/// admin check is enforced here as defense-in-depth).
pub async fn put_admin_credentials(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
    Json(req): Json<UpdateTgCredentials>,
) -> Result<Json<TgCredentialsResponse>, ApiError> {
    if !ctx.is_admin(&user.0 .0) {
        return Err(ApiError::Forbidden("Admin privileges required".into()));
    }
    validate_creds(&req)?;
    ctx.manager
        .update_credentials(req.api_id, req.api_hash.trim().to_string());
    tracing::info!(user = %user.0 .0, api_id = req.api_id, "Global Telegram credentials updated by admin");
    Ok(Json(build_response(&ctx, &user.0 .0).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_hash_tail_only() {
        assert_eq!(mask_hash("0123456789abcdef"), "••••cdef");
        assert!(!mask_hash("0123456789abcdef").contains("0123"));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn secret_file_written_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join("telegram-creds")
            .join("u1")
            .join("creds.json");
        write_private_file(&path, b"{\"apiId\":1}").await.unwrap();
        let fmode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(fmode, 0o600, "creds file must be owner-only");
        let dmode = std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dmode, 0o700, "creds dir must be owner-only");
        // No temp file left behind.
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn validates_credentials() {
        assert!(validate_creds(&UpdateTgCredentials {
            api_id: 123,
            api_hash: "abcdef0123456789".into()
        })
        .is_ok());
        assert!(validate_creds(&UpdateTgCredentials {
            api_id: 0,
            api_hash: "abcdef0123456789".into()
        })
        .is_err());
        assert!(validate_creds(&UpdateTgCredentials {
            api_id: -1,
            api_hash: "abcdef0123456789".into()
        })
        .is_err());
        assert!(validate_creds(&UpdateTgCredentials {
            api_id: 1,
            api_hash: "short".into()
        })
        .is_err());
        assert!(validate_creds(&UpdateTgCredentials {
            api_id: 1,
            api_hash: "has space here xx".into()
        })
        .is_err());
    }
}
