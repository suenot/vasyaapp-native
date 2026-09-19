//! Route modules and router assembly.
//!
//! Pathing convention (plan §4.2): /api/v1, accounts scoped as
//! /accounts/{acc}/..., raw bodies for media upload, bytes out for media
//! download. Voice calls (1:1 + group) are implemented in `calls`, STT in
//! `stt` (cloud Deepgram) and storage-mode in `storage_mode` (fixed server
//! storage); only real-time 1:1 call audio remains 501.

use std::sync::Arc;

use axum::extract::DefaultBodyLimit;
use axum::routing::{delete, get, post, put};
use axum::{middleware, Router};

use crate::auth::UserId;
use crate::context::ServerContext;
use crate::error::ApiError;

pub mod accounts;
pub mod agent_keys;
pub mod calls;
pub mod chats;
pub mod events;
pub mod folders;
pub mod graphql_http;
pub mod media;
pub mod messages;
pub mod search;
pub mod storage_mode;
pub mod stt;
pub mod telegram_auth;
pub mod telegram_creds;
pub mod topics;

/// Max raw media upload size.
const MEDIA_BODY_LIMIT: usize = 128 * 1024 * 1024;

/// Ownership check + client lookup used by every account-scoped handler.
pub(crate) async fn account_client(
    ctx: &ServerContext,
    user: &UserId,
    account_id: &str,
) -> Result<std::sync::Arc<vasya_core::telegram::client_manager::TelegramClientWrapper>, ApiError> {
    ctx.accounts.ensure_access(&user.0, account_id)?;
    ctx.manager
        .get_client(account_id)
        .await
        .ok_or_else(|| ApiError::NotFound("Client not found for this account".into()))
}

pub fn api_router(ctx: Arc<ServerContext>) -> Router {
    let schema = crate::graphql::build_schema(ctx.clone());

    let protected = Router::new()
        // GraphQL queries/mutations (same bearer middleware as REST)
        .route("/graphql", post(graphql_http::graphql_post))
        // Telegram API credentials: per-user (opt-in) + admin-only global
        // default (see routes/telegram_creds.rs). Credential management is
        // human-session-only (agent keys are blocked in policy.rs).
        .route(
            "/telegram/credentials",
            get(telegram_creds::get_credentials),
        )
        .route(
            "/telegram/credentials",
            put(telegram_creds::put_user_credentials),
        )
        .route(
            "/telegram/credentials",
            delete(telegram_creds::delete_user_credentials),
        )
        .route(
            "/admin/telegram/credentials",
            put(telegram_creds::put_admin_credentials),
        )
        // Telegram login flow
        .route(
            "/telegram/login/code",
            post(telegram_auth::request_login_code),
        )
        .route("/telegram/login/verify", post(telegram_auth::verify_code))
        .route(
            "/telegram/login/password",
            post(telegram_auth::check_password),
        )
        // Accounts
        .route("/accounts", get(accounts::list_accounts))
        .route("/accounts/{acc}", delete(accounts::logout))
        .route("/accounts/{acc}/avatar", get(accounts::my_avatar))
        // Chats
        .route("/accounts/{acc}/chats", get(chats::list_chats))
        .route(
            "/accounts/{acc}/chats/load",
            post(chats::start_loading_chats),
        )
        .route(
            "/accounts/{acc}/chats/{chat_id}",
            delete(chats::delete_and_leave_chat),
        )
        .route("/accounts/{acc}/groups", post(chats::create_group))
        .route("/accounts/{acc}/channels", post(chats::create_channel))
        .route("/accounts/{acc}/contacts", get(chats::get_contacts))
        .route(
            "/accounts/{acc}/chats/{chat_id}/photo",
            get(media::chat_photo),
        )
        .route(
            "/accounts/{acc}/chats/{chat_id}/photos",
            get(media::user_photos),
        )
        .route(
            "/accounts/{acc}/chats/{chat_id}/photos/{index}",
            get(media::user_photo_by_index),
        )
        // Messages
        .route(
            "/accounts/{acc}/chats/{chat_id}/messages",
            get(messages::get_messages),
        )
        .route(
            "/accounts/{acc}/chats/{chat_id}/messages",
            post(messages::send_message),
        )
        .route(
            "/accounts/{acc}/chats/{chat_id}/media",
            post(messages::send_media).layer(DefaultBodyLimit::max(MEDIA_BODY_LIMIT)),
        )
        .route(
            "/accounts/{acc}/messages/forward",
            post(messages::forward_messages),
        )
        .route(
            "/accounts/{acc}/chats/{chat_id}/read",
            post(messages::mark_messages_read),
        )
        .route(
            "/accounts/{acc}/chats/{chat_id}/messages/{message_id}/media",
            get(media::download_media),
        )
        // Search
        .route(
            "/accounts/{acc}/chats/{chat_id}/messages/search",
            get(messages::search_messages),
        )
        .route("/accounts/{acc}/search", get(search::global_search))
        .route(
            "/accounts/{acc}/messages/search",
            get(search::search_all_messages),
        )
        // Topics
        .route(
            "/accounts/{acc}/chats/{chat_id}/topics",
            get(topics::get_forum_topics),
        )
        // Folders / tabs
        .route("/accounts/{acc}/folders", get(folders::get_folders))
        .route("/accounts/{acc}/folders", post(folders::save_folder))
        .route(
            "/accounts/{acc}/folders/{folder_id}",
            delete(folders::delete_folder),
        )
        .route("/accounts/{acc}/tabs", get(folders::get_tabs))
        .route("/accounts/{acc}/tabs", put(folders::save_tabs))
        // 1:1 voice calls — signaling/control/state (audio stays client-side).
        // volume/mute drive the desktop VoIP sidecar → documented 501 here.
        .route("/accounts/{acc}/calls/request", post(calls::request_call))
        .route("/accounts/{acc}/calls/accept", post(calls::accept_call))
        .route("/accounts/{acc}/calls/confirm", post(calls::confirm_call))
        .route("/accounts/{acc}/calls/discard", post(calls::discard_call))
        .route(
            "/accounts/{acc}/calls/volume",
            post(calls::call_audio_unavailable),
        )
        .route(
            "/accounts/{acc}/calls/mute",
            post(calls::call_audio_unavailable),
        )
        // Group calls — full MTProto signaling (create/join/leave/mute/participants)
        .route(
            "/accounts/{acc}/group-calls",
            post(calls::create_group_call),
        )
        .route(
            "/accounts/{acc}/group-calls/join",
            post(calls::join_group_call),
        )
        .route(
            "/accounts/{acc}/group-calls/leave",
            post(calls::leave_group_call),
        )
        .route(
            "/accounts/{acc}/group-calls/mute",
            post(calls::toggle_group_call_mute),
        )
        .route(
            "/accounts/{acc}/group-calls/participants",
            get(calls::group_call_participants),
        )
        // Speech-to-text: per-user settings + transcription (cloud Deepgram on
        // the server; local Whisper is desktop-only — see routes/stt.rs).
        .route("/stt/settings", get(stt::get_settings))
        .route("/stt/settings", put(stt::update_settings))
        .route(
            "/stt/transcribe",
            post(stt::transcribe).layer(DefaultBodyLimit::max(stt::STT_BODY_LIMIT)),
        )
        .route("/stt/models", get(stt::get_models))
        .route("/stt/models/download", post(stt::download_model))
        // Realtime bus as SSE (same bus feeds the GraphQL subscriptions)
        .route("/events", get(events::sse_events))
        // Agent key management + audit (human sessions only — the agent
        // policy middleware rejects agent keys here)
        .route("/agent-keys", post(agent_keys::create_key))
        .route("/agent-keys", get(agent_keys::list_keys))
        .route("/agent-keys/scopes", get(agent_keys::list_scopes))
        .route("/agent-keys/{key_id}", delete(agent_keys::revoke_key))
        .route("/audit", get(agent_keys::read_audit))
        // Storage-mode: fixed server-side storage (GET reports it, PUT 400s —
        // the local/remote toggle is desktop-only; see routes/storage_mode.rs).
        .route("/storage-mode", get(storage_mode::get_storage_mode))
        .route("/storage-mode", put(storage_mode::set_storage_mode))
        // Layer order (inner→outer as added): idempotency → agent policy →
        // audit → auth. Audit records policy rejections and replays.
        .layer(middleware::from_fn_with_state(
            ctx.clone(),
            crate::policy::idempotency,
        ))
        .layer(middleware::from_fn_with_state(
            ctx.clone(),
            crate::policy::agent_policy,
        ))
        .layer(middleware::from_fn_with_state(
            ctx.clone(),
            crate::policy::audit_mutations,
        ))
        .layer(middleware::from_fn_with_state(
            ctx.clone(),
            crate::auth::require_auth,
        ));

    let mut public = Router::new()
        .route("/health", get(health))
        .route("/openapi.json", get(crate::openapi::openapi_json))
        // Machine-readable contract, public like /openapi.json
        .route("/graphql/sdl", get(graphql_http::graphql_sdl))
        // Subscriptions: auth happens on connection_init, not headers
        .route("/graphql/ws", get(graphql_http::graphql_ws));

    if ctx.graphql_playground {
        public = public.route("/graphql/playground", get(graphql_http::graphql_playground));
    }

    Router::new()
        .nest("/api/v1", public.merge(protected))
        .layer(axum::Extension(schema))
        .with_state(ctx)
}

async fn health() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({ "status": "ok" }))
}
