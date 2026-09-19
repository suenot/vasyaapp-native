//! Shared server state: the vasya-core engine handle plus everything the
//! HTTP handlers need around it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{Mutex, RwLock};
use vasya_core::events::BroadcastEventSink;
use vasya_core::telegram::call_state::ActiveCalls;
use vasya_core::telegram::group_call_state::ActiveGroupCalls;
use vasya_core::telegram::updates::UpdatesContext;
use vasya_core::TelegramClientManager;

use crate::accounts::AccountStore;
use crate::agent_keys::AgentKeyStore;
use crate::audit::AuditLog;
use crate::auth::{AdminPolicy, AuthMode};
use crate::dto::Chat;
use crate::idempotency::IdempotencyStore;
use crate::rate_limit::RateLimiter;

pub struct ServerContext {
    pub manager: Arc<TelegramClientManager>,
    /// Event bus: every update pump publishes here. GraphQL subscriptions
    /// (task #5) and the /events SSE endpoint fan out from it.
    pub events: Arc<BroadcastEventSink>,
    pub auth: AuthMode,
    /// Who may manage server-global settings (global Telegram credentials).
    /// Config-sourced only; never settable via the API.
    pub admins: AdminPolicy,
    pub accounts: AccountStore,
    pub rate: RateLimiter,
    /// Scoped agent API keys + their stricter per-key mutation quota.
    pub agent_keys: AgentKeyStore,
    pub agent_rate: RateLimiter,
    /// Who/what/when log of every mutating call.
    pub audit: AuditLog,
    /// Idempotency-Key replay cache for mutating routes.
    pub idempotency: IdempotencyStore,
    /// In-memory chat cache per account (server-side analogue of the app's
    /// local chat DB). Replaced wholesale by chat-loading operations.
    pub chat_cache: RwLock<HashMap<String, Vec<Chat>>>,
    /// Pending Telegram login flows (account_id -> token), same lifecycle
    /// as the Tauri AppState fields.
    pub pending_logins: Mutex<HashMap<String, grammers_client::types::LoginToken>>,
    pub pending_passwords: Mutex<HashMap<String, grammers_client::types::PasswordToken>>,
    /// Call registries shared by the update pump and the call API
    /// (`routes::calls`): the same state the desktop app keeps in AppState.
    pub active_calls: Arc<RwLock<ActiveCalls>>,
    pub active_group_calls: Arc<RwLock<ActiveGroupCalls>>,
    /// Server-side media/avatar cache directory.
    pub media_dir: PathBuf,
    /// Per-user folder/tab JSON stores live under this directory.
    pub data_dir: PathBuf,
    /// Serve the GraphQL playground page (dev only).
    pub graphql_playground: bool,
}

impl ServerContext {
    /// Whether the given user id is a server admin (config-sourced).
    pub fn is_admin(&self, user_id: &str) -> bool {
        self.admins.is_admin(user_id)
    }

    /// The updates context wiring an account's update pump to the bus.
    pub fn updates_context(&self) -> UpdatesContext {
        UpdatesContext {
            sink: self.events.clone(),
            active_calls: self.active_calls.clone(),
            active_group_calls: self.active_group_calls.clone(),
        }
    }
}
