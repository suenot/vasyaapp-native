//! Voice call operations (parity with commands/calls.rs + group_calls.rs).
//!
//! These handlers expose the **call signaling / control / state** surface
//! over REST + GraphQL: MTProto call setup/teardown, the DH exchange, and
//! group-call participant state. They are thin wrappers over the shared
//! engine in `vasya_core::telegram::{calls, group_calls}` — one
//! implementation, two transports (the same `*_op` pattern as the rest of
//! the server).
//!
//! Headless-audio caveat: a server has no microphone or speaker, so real-time
//! audio capture/playback cannot run here. The 1:1 *volume* and *mute*
//! endpoints drive the desktop VoIP sidecar only; on the server they return a
//! structured 501 explaining that audio is a client-side concern. Group-call
//! mute is a true MTProto signal (`phone.editGroupCallParticipant`), so it is
//! fully implemented. Call *state changes* (`telegram:call-*`,
//! `telegram:group-call-*`) already flow through the event bus to
//! `/events` (SSE) and GraphQL subscriptions via the shared update pump.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use serde::Deserialize;
use vasya_core::telegram::call_state::CallInfoResponse;
use vasya_core::telegram::group_call_state::{GroupCallInfoResponse, GroupCallParticipant};
use vasya_core::telegram::{calls as call_engine, group_calls as group_engine};

use crate::auth::UserId;
use crate::context::ServerContext;
use crate::error::ApiError;
use crate::routes::account_client;

/// Shared explanation returned by the audio-only 1:1 endpoints.
const AUDIO_CLIENT_SIDE: &str =
    "Real-time call audio is client-side only: a headless server has no \
     microphone or speaker, so 1:1 call mute/volume must be controlled by the \
     client running the audio (VoIP sidecar). Call signaling/state \
     (request/accept/confirm/discard) and events are available over the API.";

// --- 1:1 calls ----------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestCallRequest {
    pub user_id: i64,
    #[serde(default)]
    pub is_video: bool,
}

pub(crate) async fn request_call_op(
    ctx: &Arc<ServerContext>,
    user: &UserId,
    account_id: &str,
    user_id: i64,
    is_video: bool,
) -> Result<CallInfoResponse, ApiError> {
    tracing::info!(account_id = %account_id, user_id, is_video, "Requesting call");
    let wrapper = account_client(ctx, user, account_id).await?;
    ctx.rate.check_mutation(account_id)?;
    call_engine::request_call(&wrapper, &ctx.active_calls, account_id, user_id, is_video)
        .await
        .map_err(ApiError::telegram)
}

pub async fn request_call(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
    Path(account_id): Path<String>,
    Json(req): Json<RequestCallRequest>,
) -> Result<Json<CallInfoResponse>, ApiError> {
    Ok(Json(
        request_call_op(&ctx, &user.0, &account_id, req.user_id, req.is_video).await?,
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallIdRequest {
    pub call_id: i64,
}

pub(crate) async fn accept_call_op(
    ctx: &Arc<ServerContext>,
    user: &UserId,
    account_id: &str,
    call_id: i64,
) -> Result<CallInfoResponse, ApiError> {
    tracing::info!(account_id = %account_id, call_id, "Accepting call");
    let wrapper = account_client(ctx, user, account_id).await?;
    ctx.rate.check_mutation(account_id)?;
    call_engine::accept_call(&wrapper, &ctx.active_calls, call_id)
        .await
        .map_err(ApiError::telegram)
}

pub async fn accept_call(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
    Path(account_id): Path<String>,
    Json(req): Json<CallIdRequest>,
) -> Result<Json<CallInfoResponse>, ApiError> {
    Ok(Json(
        accept_call_op(&ctx, &user.0, &account_id, req.call_id).await?,
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfirmCallRequest {
    pub call_id: i64,
    /// The callee's `g_b` DH value (bytes).
    pub g_b: Vec<u8>,
}

pub(crate) async fn confirm_call_op(
    ctx: &Arc<ServerContext>,
    user: &UserId,
    account_id: &str,
    call_id: i64,
    g_b: Vec<u8>,
) -> Result<CallInfoResponse, ApiError> {
    tracing::info!(account_id = %account_id, call_id, "Confirming call");
    let wrapper = account_client(ctx, user, account_id).await?;
    ctx.rate.check_mutation(account_id)?;
    call_engine::confirm_call(&wrapper, &ctx.active_calls, call_id, g_b)
        .await
        .map_err(ApiError::telegram)
}

pub async fn confirm_call(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
    Path(account_id): Path<String>,
    Json(req): Json<ConfirmCallRequest>,
) -> Result<Json<CallInfoResponse>, ApiError> {
    Ok(Json(
        confirm_call_op(&ctx, &user.0, &account_id, req.call_id, req.g_b).await?,
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscardCallRequest {
    pub call_id: i64,
    /// One of: hangup (default), missed, disconnect, busy.
    #[serde(default)]
    pub reason: Option<String>,
}

pub(crate) async fn discard_call_op(
    ctx: &Arc<ServerContext>,
    user: &UserId,
    account_id: &str,
    call_id: i64,
    reason: &str,
) -> Result<(), ApiError> {
    tracing::info!(account_id = %account_id, call_id, reason, "Discarding call");
    let wrapper = account_client(ctx, user, account_id).await?;
    ctx.rate.check_mutation(account_id)?;
    call_engine::discard_call(&wrapper, &ctx.active_calls, call_id, reason)
        .await
        .map_err(ApiError::telegram)
}

pub async fn discard_call(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
    Path(account_id): Path<String>,
    Json(req): Json<DiscardCallRequest>,
) -> Result<StatusCode, ApiError> {
    let reason = req.reason.as_deref().unwrap_or("hangup");
    discard_call_op(&ctx, &user.0, &account_id, req.call_id, reason).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// 1:1 call audio (volume/mute) is client-side only — see [`AUDIO_CLIENT_SIDE`].
pub async fn call_audio_unavailable() -> ApiError {
    ApiError::NotImplemented(AUDIO_CLIENT_SIDE.into())
}

// --- Group calls --------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateGroupCallRequest {
    pub chat_id: i64,
    #[serde(default)]
    pub title: Option<String>,
}

pub(crate) async fn create_group_call_op(
    ctx: &Arc<ServerContext>,
    user: &UserId,
    account_id: &str,
    chat_id: i64,
    title: Option<String>,
) -> Result<GroupCallInfoResponse, ApiError> {
    tracing::info!(account_id = %account_id, chat_id, "Creating group call");
    let wrapper = account_client(ctx, user, account_id).await?;
    ctx.rate.check_mutation(account_id)?;
    group_engine::create_group_call(
        &wrapper,
        &ctx.active_group_calls,
        account_id,
        chat_id,
        title,
    )
    .await
    .map_err(ApiError::telegram)
}

pub async fn create_group_call(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
    Path(account_id): Path<String>,
    Json(req): Json<CreateGroupCallRequest>,
) -> Result<Json<GroupCallInfoResponse>, ApiError> {
    Ok(Json(
        create_group_call_op(&ctx, &user.0, &account_id, req.chat_id, req.title).await?,
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JoinGroupCallRequest {
    pub call_id: i64,
    pub access_hash: i64,
    pub chat_id: i64,
    #[serde(default)]
    pub muted: bool,
}

pub(crate) async fn join_group_call_op(
    ctx: &Arc<ServerContext>,
    user: &UserId,
    account_id: &str,
    call_id: i64,
    access_hash: i64,
    chat_id: i64,
    muted: bool,
) -> Result<GroupCallInfoResponse, ApiError> {
    tracing::info!(account_id = %account_id, call_id, chat_id, "Joining group call");
    let wrapper = account_client(ctx, user, account_id).await?;
    ctx.rate.check_mutation(account_id)?;
    group_engine::join_group_call(
        &wrapper,
        &ctx.active_group_calls,
        account_id,
        call_id,
        access_hash,
        chat_id,
        muted,
    )
    .await
    .map_err(ApiError::telegram)
}

pub async fn join_group_call(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
    Path(account_id): Path<String>,
    Json(req): Json<JoinGroupCallRequest>,
) -> Result<Json<GroupCallInfoResponse>, ApiError> {
    Ok(Json(
        join_group_call_op(
            &ctx,
            &user.0,
            &account_id,
            req.call_id,
            req.access_hash,
            req.chat_id,
            req.muted,
        )
        .await?,
    ))
}

pub(crate) async fn leave_group_call_op(
    ctx: &Arc<ServerContext>,
    user: &UserId,
    account_id: &str,
    call_id: i64,
) -> Result<(), ApiError> {
    tracing::info!(account_id = %account_id, call_id, "Leaving group call");
    let wrapper = account_client(ctx, user, account_id).await?;
    ctx.rate.check_mutation(account_id)?;
    group_engine::leave_group_call(&wrapper, &ctx.active_group_calls, call_id)
        .await
        .map_err(ApiError::telegram)
}

pub async fn leave_group_call(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
    Path(account_id): Path<String>,
    Json(req): Json<CallIdRequest>,
) -> Result<StatusCode, ApiError> {
    leave_group_call_op(&ctx, &user.0, &account_id, req.call_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupCallMuteRequest {
    pub call_id: i64,
    pub muted: bool,
}

pub(crate) async fn toggle_group_call_mute_op(
    ctx: &Arc<ServerContext>,
    user: &UserId,
    account_id: &str,
    call_id: i64,
    muted: bool,
) -> Result<(), ApiError> {
    tracing::info!(account_id = %account_id, call_id, muted, "Toggle group call mute");
    let wrapper = account_client(ctx, user, account_id).await?;
    ctx.rate.check_mutation(account_id)?;
    group_engine::toggle_group_call_mute(&wrapper, &ctx.active_group_calls, call_id, muted)
        .await
        .map_err(ApiError::telegram)
}

pub async fn toggle_group_call_mute(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
    Path(account_id): Path<String>,
    Json(req): Json<GroupCallMuteRequest>,
) -> Result<StatusCode, ApiError> {
    toggle_group_call_mute_op(&ctx, &user.0, &account_id, req.call_id, req.muted).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParticipantsQuery {
    pub call_id: i64,
    pub access_hash: i64,
}

pub(crate) async fn group_call_participants_op(
    ctx: &Arc<ServerContext>,
    user: &UserId,
    account_id: &str,
    call_id: i64,
    access_hash: i64,
) -> Result<Vec<GroupCallParticipant>, ApiError> {
    tracing::info!(account_id = %account_id, call_id, "Getting group call participants");
    let wrapper = account_client(ctx, user, account_id).await?;
    group_engine::get_group_call_participants(&wrapper, call_id, access_hash)
        .await
        .map_err(ApiError::telegram)
}

pub async fn group_call_participants(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
    Path(account_id): Path<String>,
    Query(query): Query<ParticipantsQuery>,
) -> Result<Json<Vec<GroupCallParticipant>>, ApiError> {
    Ok(Json(
        group_call_participants_op(&ctx, &user.0, &account_id, query.call_id, query.access_hash)
            .await?,
    ))
}
