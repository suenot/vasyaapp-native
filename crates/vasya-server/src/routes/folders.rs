//! Folders and tabs (parity with commands/folders.rs).
//!
//! The desktop app persists these in its local storage; here they live in
//! per-(user, account) JSON files under the data dir so embedded-local mode
//! needs no database. Validation rules are ported verbatim.

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Extension, Json};

use crate::auth::UserId;
use crate::context::ServerContext;
use crate::dto::{FolderRecord, TabRecord};
use crate::error::ApiError;

/// Maximum allowed sort_order value to prevent overflow / abuse
const MAX_SORT_ORDER: i32 = 10_000;

fn validate_account_id(account_id: &str) -> Result<(), ApiError> {
    if account_id.is_empty() {
        return Err(ApiError::BadRequest("account_id must not be empty".into()));
    }
    if account_id.len() > 128 {
        return Err(ApiError::BadRequest("account_id too long".into()));
    }
    if !account_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(ApiError::BadRequest(
            "account_id contains invalid characters".into(),
        ));
    }
    Ok(())
}

fn validate_sort_order(sort_order: i32) -> Result<(), ApiError> {
    if sort_order < 0 || sort_order > MAX_SORT_ORDER {
        return Err(ApiError::BadRequest(format!(
            "sort_order must be between 0 and {MAX_SORT_ORDER}"
        )));
    }
    Ok(())
}

fn store_path(ctx: &ServerContext, user: &str, account_id: &str, kind: &str) -> PathBuf {
    // user ids are uuids or "local"; account ids are validated above.
    ctx.data_dir
        .join("ui-state")
        .join(user)
        .join(format!("{account_id}.{kind}.json"))
}

async fn read_list<T: serde::de::DeserializeOwned>(path: &PathBuf) -> Result<Vec<T>, ApiError> {
    match tokio::fs::read(path).await {
        Ok(raw) => serde_json::from_slice(&raw).map_err(ApiError::internal),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(ApiError::internal(e)),
    }
}

async fn write_list<T: serde::Serialize>(path: &PathBuf, items: &[T]) -> Result<(), ApiError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(ApiError::internal)?;
    }
    let raw = serde_json::to_vec_pretty(items).map_err(ApiError::internal)?;
    tokio::fs::write(path, raw)
        .await
        .map_err(ApiError::internal)
}

fn check(ctx: &ServerContext, user: &UserId, account_id: &str) -> Result<(), ApiError> {
    validate_account_id(account_id)?;
    ctx.accounts.ensure_access(&user.0, account_id)
}

pub(crate) async fn get_folders_op(
    ctx: &ServerContext,
    user: &UserId,
    account_id: &str,
) -> Result<Vec<FolderRecord>, ApiError> {
    check(ctx, user, account_id)?;
    let path = store_path(ctx, &user.0, account_id, "folders");
    read_list(&path).await
}

pub async fn get_folders(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
    Path(account_id): Path<String>,
) -> Result<Json<Vec<FolderRecord>>, ApiError> {
    Ok(Json(get_folders_op(&ctx, &user.0, &account_id).await?))
}

pub(crate) async fn save_folder_op(
    ctx: &ServerContext,
    user: &UserId,
    account_id: &str,
    folder: FolderRecord,
) -> Result<(), ApiError> {
    check(ctx, user, account_id)?;
    validate_sort_order(folder.sort_order)?;

    let path = store_path(ctx, &user.0, account_id, "folders");
    let mut folders: Vec<FolderRecord> = read_list(&path).await?;
    folders.retain(|f| f.id != folder.id);
    folders.push(folder);
    folders.sort_by_key(|f| f.sort_order);
    write_list(&path, &folders).await
}

pub async fn save_folder(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
    Path(account_id): Path<String>,
    Json(folder): Json<FolderRecord>,
) -> Result<StatusCode, ApiError> {
    save_folder_op(&ctx, &user.0, &account_id, folder).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn delete_folder_op(
    ctx: &ServerContext,
    user: &UserId,
    account_id: &str,
    folder_id: &str,
) -> Result<(), ApiError> {
    check(ctx, user, account_id)?;
    let path = store_path(ctx, &user.0, account_id, "folders");
    let mut folders: Vec<FolderRecord> = read_list(&path).await?;
    folders.retain(|f| f.id != folder_id);
    write_list(&path, &folders).await
}

pub async fn delete_folder(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
    Path((account_id, folder_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    delete_folder_op(&ctx, &user.0, &account_id, &folder_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn get_tabs_op(
    ctx: &ServerContext,
    user: &UserId,
    account_id: &str,
) -> Result<Vec<TabRecord>, ApiError> {
    check(ctx, user, account_id)?;
    let path = store_path(ctx, &user.0, account_id, "tabs");
    read_list(&path).await
}

pub async fn get_tabs(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
    Path(account_id): Path<String>,
) -> Result<Json<Vec<TabRecord>>, ApiError> {
    Ok(Json(get_tabs_op(&ctx, &user.0, &account_id).await?))
}

pub(crate) async fn save_tabs_op(
    ctx: &ServerContext,
    user: &UserId,
    account_id: &str,
    tabs: Vec<TabRecord>,
) -> Result<(), ApiError> {
    check(ctx, user, account_id)?;
    for tab in &tabs {
        validate_sort_order(tab.sort_order)?;
    }
    let path = store_path(ctx, &user.0, account_id, "tabs");
    write_list(&path, &tabs).await
}

pub async fn save_tabs(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
    Path(account_id): Path<String>,
    Json(tabs): Json<Vec<TabRecord>>,
) -> Result<StatusCode, ApiError> {
    save_tabs_op(&ctx, &user.0, &account_id, tabs).await?;
    Ok(StatusCode::NO_CONTENT)
}
