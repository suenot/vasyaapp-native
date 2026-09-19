//! GraphQL schema (Phase 3, plan §4.3): Query/Mutation parity with the
//! REST surface plus Subscriptions fed by the in-process event bus.
//!
//! Resolvers are thin wrappers over the same `op` functions the REST
//! handlers use — one implementation, two transports. Subscription items
//! are `EventPayload { event, payload }` envelopes where `event` is the
//! original Tauri-compatible event name and `payload` is byte-identical
//! to the desktop event payload, so the web frontend's existing event
//! handlers can be reused as-is.

use std::sync::Arc;

use async_graphql::{
    Context, Error, ErrorExtensions, Object, Result, Schema, SimpleObject, Subscription,
};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};

use crate::agent_keys::AgentIdentity;
use crate::auth::UserId;
use crate::context::ServerContext;
use crate::dto::{
    Chat, FolderRecord, ForumTopic, GlobalMessageResult, GlobalSearchResult, Message, TabRecord,
};
use crate::error::ApiError;
use crate::routes;
use crate::routes::accounts::AccountSummary;
use crate::routes::telegram_auth::{LoginCodeResponse, LoginResult};

pub type VasyaSchema = Schema<QueryRoot, MutationRoot, SubscriptionRoot>;

pub fn build_schema(ctx: Arc<ServerContext>) -> VasyaSchema {
    Schema::build(QueryRoot, MutationRoot, SubscriptionRoot)
        .data(ctx)
        // DoS guards: async-graphql applies no depth/complexity limit by
        // default, so a deeply nested or highly-aliased query (reachable by any
        // authenticated caller, incl. a low-priv agent key — `/graphql` is let
        // through the policy gate) could amplify parser/resolver cost. These
        // bounds are generous for the real schema (shallow, flat resolvers).
        .limit_depth(20)
        .limit_complexity(500)
        .finish()
}

// ApiError converts to async_graphql::Error through async-graphql's blanket
// `From<T: Display>` impl, so `?` works on the shared op functions directly.

fn server_ctx<'a>(ctx: &Context<'a>) -> &'a Arc<ServerContext> {
    ctx.data_unchecked::<Arc<ServerContext>>()
}

/// The authenticated caller, injected per HTTP request (auth middleware)
/// or per WS connection (connection_init).
fn auth_user<'a>(ctx: &Context<'a>) -> Result<&'a UserId> {
    ctx.data::<UserId>().map_err(|_| {
        Error::new("Unauthorized").extend_with(|_, ext| ext.set("code", "UNAUTHORIZED"))
    })
}

fn forbidden(message: impl Into<String>) -> Error {
    Error::new(message.into()).extend_with(|_, ext| ext.set("code", "FORBIDDEN"))
}

/// Authenticate the caller and enforce the agent scope (and, when given, the
/// per-account allowlist) this resolver requires.
///
/// GraphQL routes everything through one `/graphql` path, so the REST
/// path-based scope gate in `policy.rs` can't apply here — enforcement lives
/// in the resolver instead. `scope` is the exact same string the analogous
/// REST endpoint maps to via [`crate::policy::required_scope`] (cross-checked
/// in tests, so the two transports can't drift). Human sessions carry no
/// [`AgentIdentity`] and hold every scope implicitly, so they pass straight
/// through — matching `policy.rs`, which skips callers without an identity.
fn authorize<'a>(ctx: &Context<'a>, scope: &str, account_id: Option<&str>) -> Result<&'a UserId> {
    let user = auth_user(ctx)?;
    if let Some(agent) = ctx.data_opt::<AgentIdentity>() {
        if !agent.has_scope(scope) {
            return Err(forbidden(format!("Missing scope: {scope}")));
        }
        // Per-account allowlist mirrors REST: only enforced for ops that
        // actually target a specific account (account_id = Some).
        if let Some(acc) = account_id {
            if !agent.allows_account(acc) {
                return Err(forbidden("account not in key allowlist"));
            }
        }
    }
    Ok(user)
}

// --- Output types specific to GraphQL ------------------------------------------

#[derive(SimpleObject)]
pub struct GqlUserInfo {
    pub id: i64,
    pub first_name: String,
    pub last_name: Option<String>,
    pub username: Option<String>,
    pub phone: String,
}

impl From<vasya_core::telegram::auth::UserInfo> for GqlUserInfo {
    fn from(u: vasya_core::telegram::auth::UserInfo) -> Self {
        Self {
            id: u.id,
            first_name: u.first_name,
            last_name: u.last_name,
            username: u.username,
            phone: u.phone,
        }
    }
}

/// Login step outcome: `status` is "authorized" (user set) or
/// "password_required" (call checkPassword next).
#[derive(SimpleObject)]
pub struct LoginPayload {
    pub status: String,
    pub user: Option<GqlUserInfo>,
}

impl From<LoginResult> for LoginPayload {
    fn from(result: LoginResult) -> Self {
        match result {
            LoginResult::Authorized(user) => Self {
                status: "authorized".into(),
                user: Some(user.into()),
            },
            LoginResult::PasswordRequired => Self {
                status: "password_required".into(),
                user: None,
            },
        }
    }
}

// --- Call output types (mirror the REST/Tauri JSON, camelCase fields) ---------

use vasya_core::telegram::call_state::{CallInfoResponse, CallState};
use vasya_core::telegram::group_call_state::{
    GroupCallInfoResponse, GroupCallParticipant, GroupCallState,
};

fn call_state_str(s: &CallState) -> &'static str {
    match s {
        CallState::Requesting => "requesting",
        CallState::Waiting => "waiting",
        CallState::Ringing => "ringing",
        CallState::Accepted => "accepted",
        CallState::Active => "active",
        CallState::Discarded => "discarded",
        CallState::Error => "error",
    }
}

fn group_call_state_str(s: &GroupCallState) -> &'static str {
    match s {
        GroupCallState::Idle => "idle",
        GroupCallState::Creating => "creating",
        GroupCallState::Joining => "joining",
        GroupCallState::Active => "active",
        GroupCallState::Leaving => "leaving",
    }
}

/// 1:1 call signaling/state (`state` matches the REST `state` string).
#[derive(SimpleObject)]
pub struct GqlCallInfo {
    pub call_id: i64,
    pub access_hash: i64,
    pub peer_user_id: i64,
    pub is_outgoing: bool,
    pub is_video: bool,
    pub state: String,
}

impl From<CallInfoResponse> for GqlCallInfo {
    fn from(c: CallInfoResponse) -> Self {
        Self {
            call_id: c.call_id,
            access_hash: c.access_hash,
            peer_user_id: c.peer_user_id,
            is_outgoing: c.is_outgoing,
            is_video: c.is_video,
            state: call_state_str(&c.state).into(),
        }
    }
}

/// Group call signaling/state.
#[derive(SimpleObject)]
pub struct GqlGroupCallInfo {
    pub call_id: i64,
    pub access_hash: i64,
    pub chat_id: i64,
    pub state: String,
    pub title: Option<String>,
    pub participants_count: i32,
    pub can_start_video: bool,
}

impl From<GroupCallInfoResponse> for GqlGroupCallInfo {
    fn from(g: GroupCallInfoResponse) -> Self {
        Self {
            call_id: g.call_id,
            access_hash: g.access_hash,
            chat_id: g.chat_id,
            state: group_call_state_str(&g.state).into(),
            title: g.title,
            participants_count: g.participants_count,
            can_start_video: g.can_start_video,
        }
    }
}

/// One participant of a group call (mirrors the REST JSON).
#[derive(SimpleObject)]
pub struct GqlGroupCallParticipant {
    pub user_id: i64,
    pub name: Option<String>,
    pub is_muted: bool,
    pub is_self: bool,
    pub is_speaking: bool,
    pub volume: Option<i32>,
    pub can_self_unmute: bool,
    pub video_joined: bool,
    pub about: Option<String>,
    pub raise_hand_rating: Option<i64>,
    pub source: i32,
}

impl From<GroupCallParticipant> for GqlGroupCallParticipant {
    fn from(p: GroupCallParticipant) -> Self {
        Self {
            user_id: p.user_id,
            name: p.name,
            is_muted: p.is_muted,
            is_self: p.is_self,
            is_speaking: p.is_speaking,
            volume: p.volume,
            can_self_unmute: p.can_self_unmute,
            video_joined: p.video_joined,
            about: p.about,
            raise_hand_rating: p.raise_hand_rating,
            source: p.source,
        }
    }
}

/// A realtime event: original Tauri-compatible name + unchanged payload.
#[derive(SimpleObject, Clone)]
pub struct EventPayload {
    pub event: String,
    pub payload: async_graphql::Json<serde_json::Value>,
}

// --- Query ----------------------------------------------------------------------

pub struct QueryRoot;

#[Object]
impl QueryRoot {
    /// The caller's telegram accounts.
    async fn accounts(&self, ctx: &Context<'_>) -> Result<Vec<AccountSummary>> {
        let (sctx, user) = (server_ctx(ctx), authorize(ctx, "accounts:read", None)?);
        Ok(routes::accounts::list_accounts_op(sctx, user).await)
    }

    /// Whether Telegram api_id/api_hash are configured for the caller (their own
    /// per-user credentials if set, otherwise the global default).
    async fn credentials_configured(&self, ctx: &Context<'_>) -> Result<bool> {
        let (sctx, user) = (server_ctx(ctx), authorize(ctx, "telegram:login", None)?);
        Ok(routes::telegram_creds::resolve_credentials(sctx, &user.0)
            .await?
            .is_some())
    }

    /// Chats; `live: true` forces a fresh dialog iteration (default cache).
    async fn chats(
        &self,
        ctx: &Context<'_>,
        account_id: String,
        #[graphql(default = false)] live: bool,
    ) -> Result<Vec<Chat>> {
        let (sctx, user) = (
            server_ctx(ctx),
            authorize(ctx, "chats:read", Some(&account_id))?,
        );
        Ok(routes::chats::list_chats_op(sctx, user, &account_id, live).await?)
    }

    /// User-type chats (contacts).
    async fn contacts(&self, ctx: &Context<'_>, account_id: String) -> Result<Vec<Chat>> {
        let (sctx, user) = (
            server_ctx(ctx),
            authorize(ctx, "chats:read", Some(&account_id))?,
        );
        Ok(routes::chats::get_contacts_op(sctx, user, &account_id).await?)
    }

    /// Messages of a chat; topicId narrows to a forum topic.
    async fn messages(
        &self,
        ctx: &Context<'_>,
        account_id: String,
        chat_id: i64,
        offset_id: Option<i32>,
        limit: Option<i32>,
        topic_id: Option<i32>,
    ) -> Result<Vec<Message>> {
        let (sctx, user) = (
            server_ctx(ctx),
            authorize(ctx, "messages:read", Some(&account_id))?,
        );
        Ok(routes::messages::get_messages_op(
            sctx,
            user,
            &account_id,
            chat_id,
            offset_id,
            limit.map(|l| l.max(0) as usize),
            topic_id,
        )
        .await?)
    }

    /// Search within one chat.
    async fn search_messages(
        &self,
        ctx: &Context<'_>,
        account_id: String,
        chat_id: i64,
        q: String,
        limit: Option<i32>,
    ) -> Result<Vec<Message>> {
        let (sctx, user) = (
            server_ctx(ctx),
            authorize(ctx, "messages:read", Some(&account_id))?,
        );
        Ok(routes::messages::search_messages_op(
            sctx,
            user,
            &account_id,
            chat_id,
            &q,
            limit.map(|l| l.max(0) as usize),
        )
        .await?)
    }

    /// Global search for users/groups/channels.
    async fn global_search(
        &self,
        ctx: &Context<'_>,
        account_id: String,
        q: String,
        limit: Option<i32>,
    ) -> Result<Vec<GlobalSearchResult>> {
        let (sctx, user) = (
            server_ctx(ctx),
            authorize(ctx, "chats:read", Some(&account_id))?,
        );
        Ok(routes::search::global_search_op(sctx, user, &account_id, &q, limit).await?)
    }

    /// Search messages across all chats.
    async fn search_all_messages(
        &self,
        ctx: &Context<'_>,
        account_id: String,
        q: String,
        limit: Option<i32>,
    ) -> Result<Vec<GlobalMessageResult>> {
        let (sctx, user) = (
            server_ctx(ctx),
            authorize(ctx, "messages:read", Some(&account_id))?,
        );
        Ok(routes::search::search_all_messages_op(sctx, user, &account_id, &q, limit).await?)
    }

    /// Forum topics of a supergroup.
    async fn forum_topics(
        &self,
        ctx: &Context<'_>,
        account_id: String,
        chat_id: i64,
    ) -> Result<Vec<ForumTopic>> {
        let (sctx, user) = (
            server_ctx(ctx),
            authorize(ctx, "chats:read", Some(&account_id))?,
        );
        Ok(routes::topics::get_forum_topics_op(sctx, user, &account_id, chat_id).await?)
    }

    async fn folders(&self, ctx: &Context<'_>, account_id: String) -> Result<Vec<FolderRecord>> {
        let (sctx, user) = (
            server_ctx(ctx),
            authorize(ctx, "folders:read", Some(&account_id))?,
        );
        Ok(routes::folders::get_folders_op(sctx, user, &account_id).await?)
    }

    async fn tabs(&self, ctx: &Context<'_>, account_id: String) -> Result<Vec<TabRecord>> {
        let (sctx, user) = (
            server_ctx(ctx),
            authorize(ctx, "folders:read", Some(&account_id))?,
        );
        Ok(routes::folders::get_tabs_op(sctx, user, &account_id).await?)
    }

    /// Participants of a group call (signaling/state only).
    async fn group_call_participants(
        &self,
        ctx: &Context<'_>,
        account_id: String,
        call_id: i64,
        access_hash: i64,
    ) -> Result<Vec<GqlGroupCallParticipant>> {
        let (sctx, user) = (
            server_ctx(ctx),
            authorize(ctx, "calls:use", Some(&account_id))?,
        );
        let parts = routes::calls::group_call_participants_op(
            sctx,
            user,
            &account_id,
            call_id,
            access_hash,
        )
        .await?;
        Ok(parts.into_iter().map(Into::into).collect())
    }
}

// --- Mutation --------------------------------------------------------------------

pub struct MutationRoot;

#[Object]
impl MutationRoot {
    /// Start a new account login: sends a Telegram code to the phone.
    async fn request_login_code(
        &self,
        ctx: &Context<'_>,
        phone: String,
    ) -> Result<LoginCodeResponse> {
        let (sctx, user) = (server_ctx(ctx), authorize(ctx, "telegram:login", None)?);
        Ok(routes::telegram_auth::request_login_code_op(sctx, user, phone).await?)
    }

    /// Verify the login code; may answer password_required (2FA).
    async fn verify_code(
        &self,
        ctx: &Context<'_>,
        account_id: String,
        code: String,
    ) -> Result<LoginPayload> {
        let (sctx, user) = (server_ctx(ctx), authorize(ctx, "telegram:login", None)?);
        Ok(
            routes::telegram_auth::verify_code_op(sctx, user, account_id, code)
                .await?
                .into(),
        )
    }

    /// Complete a 2FA login.
    async fn check_password(
        &self,
        ctx: &Context<'_>,
        account_id: String,
        password: String,
    ) -> Result<LoginPayload> {
        let (sctx, user) = (server_ctx(ctx), authorize(ctx, "telegram:login", None)?);
        let user_info =
            routes::telegram_auth::check_password_op(sctx, user, account_id, password).await?;
        Ok(LoginPayload {
            status: "authorized".into(),
            user: Some(user_info.into()),
        })
    }

    /// Disconnect and delete an account session.
    async fn logout(&self, ctx: &Context<'_>, account_id: String) -> Result<bool> {
        let (sctx, user) = (
            server_ctx(ctx),
            authorize(ctx, "accounts:delete", Some(&account_id))?,
        );
        routes::accounts::logout_op(sctx, user, &account_id).await?;
        Ok(true)
    }

    /// Set Telegram api_id/api_hash.
    /// Set the server-global default Telegram credentials. Admin + human only,
    /// mirroring REST `PUT /admin/telegram/credentials`. An agent key (even one
    /// owned by an admin) is rejected. Per-user credentials are set over REST.
    async fn update_credentials(
        &self,
        ctx: &Context<'_>,
        api_id: i32,
        api_hash: String,
    ) -> Result<bool> {
        let (sctx, user) = (server_ctx(ctx), auth_user(ctx)?);
        if ctx.data_opt::<AgentIdentity>().is_some() {
            return Err(forbidden("Agent keys cannot manage Telegram credentials"));
        }
        if !sctx.is_admin(&user.0) {
            return Err(forbidden("Admin privileges required"));
        }
        sctx.manager.update_credentials(api_id, api_hash);
        Ok(true)
    }

    async fn send_message(
        &self,
        ctx: &Context<'_>,
        account_id: String,
        chat_id: i64,
        text: String,
        topic_id: Option<i32>,
    ) -> Result<Message> {
        let (sctx, user) = (
            server_ctx(ctx),
            authorize(ctx, "messages:send", Some(&account_id))?,
        );
        Ok(
            routes::messages::send_message_op(sctx, user, &account_id, chat_id, text, topic_id)
                .await?,
        )
    }

    async fn forward_messages(
        &self,
        ctx: &Context<'_>,
        account_id: String,
        from_chat_id: i64,
        to_chat_id: i64,
        message_ids: Vec<i32>,
    ) -> Result<Vec<Option<i32>>> {
        let (sctx, user) = (
            server_ctx(ctx),
            authorize(ctx, "messages:forward", Some(&account_id))?,
        );
        Ok(routes::messages::forward_messages_op(
            sctx,
            user,
            &account_id,
            from_chat_id,
            to_chat_id,
            &message_ids,
        )
        .await?)
    }

    async fn mark_messages_read(
        &self,
        ctx: &Context<'_>,
        account_id: String,
        chat_id: i64,
        max_id: i32,
    ) -> Result<bool> {
        let (sctx, user) = (
            server_ctx(ctx),
            authorize(ctx, "messages:send", Some(&account_id))?,
        );
        routes::messages::mark_messages_read_op(sctx, user, &account_id, chat_id, max_id).await?;
        Ok(true)
    }

    /// Kick off progressive chat loading; subscribe to chatUpdated /
    /// chatsLoadingProgress for the results.
    async fn start_loading_chats(&self, ctx: &Context<'_>, account_id: String) -> Result<bool> {
        let (sctx, user) = (
            server_ctx(ctx),
            authorize(ctx, "chats:read", Some(&account_id))?,
        );
        routes::chats::start_loading_chats_op(sctx, user, &account_id).await?;
        Ok(true)
    }

    async fn delete_and_leave_chat(
        &self,
        ctx: &Context<'_>,
        account_id: String,
        chat_id: i64,
    ) -> Result<bool> {
        let (sctx, user) = (
            server_ctx(ctx),
            authorize(ctx, "chats:delete", Some(&account_id))?,
        );
        routes::chats::delete_and_leave_chat_op(sctx, user, &account_id, chat_id).await?;
        Ok(true)
    }

    /// Create a basic group; returns the new chat id (bot-api format).
    async fn create_group(
        &self,
        ctx: &Context<'_>,
        account_id: String,
        title: String,
        user_ids: Vec<i64>,
    ) -> Result<i64> {
        let (sctx, user) = (
            server_ctx(ctx),
            authorize(ctx, "chats:write", Some(&account_id))?,
        );
        Ok(routes::chats::create_group_op(sctx, user, &account_id, title, &user_ids).await?)
    }

    /// Create a channel or megagroup; returns the new chat id.
    async fn create_channel(
        &self,
        ctx: &Context<'_>,
        account_id: String,
        title: String,
        #[graphql(default)] about: String,
        #[graphql(default = false)] is_megagroup: bool,
    ) -> Result<i64> {
        let (sctx, user) = (
            server_ctx(ctx),
            authorize(ctx, "chats:write", Some(&account_id))?,
        );
        Ok(
            routes::chats::create_channel_op(sctx, user, &account_id, title, about, is_megagroup)
                .await?,
        )
    }

    async fn save_folder(
        &self,
        ctx: &Context<'_>,
        account_id: String,
        folder: FolderRecord,
    ) -> Result<bool> {
        let (sctx, user) = (
            server_ctx(ctx),
            authorize(ctx, "folders:write", Some(&account_id))?,
        );
        routes::folders::save_folder_op(sctx, user, &account_id, folder).await?;
        Ok(true)
    }

    async fn delete_folder(
        &self,
        ctx: &Context<'_>,
        account_id: String,
        id: String,
    ) -> Result<bool> {
        let (sctx, user) = (
            server_ctx(ctx),
            authorize(ctx, "folders:write", Some(&account_id))?,
        );
        routes::folders::delete_folder_op(sctx, user, &account_id, &id).await?;
        Ok(true)
    }

    async fn save_tabs(
        &self,
        ctx: &Context<'_>,
        account_id: String,
        tabs: Vec<TabRecord>,
    ) -> Result<bool> {
        let (sctx, user) = (
            server_ctx(ctx),
            authorize(ctx, "folders:write", Some(&account_id))?,
        );
        routes::folders::save_tabs_op(sctx, user, &account_id, tabs).await?;
        Ok(true)
    }

    // --- 1:1 calls: signaling/control/state (audio stays client-side) -----------

    /// Start an outgoing 1:1 call (DH exchange + `phone.requestCall`).
    async fn request_call(
        &self,
        ctx: &Context<'_>,
        account_id: String,
        user_id: i64,
        #[graphql(default = false)] is_video: bool,
    ) -> Result<GqlCallInfo> {
        let (sctx, user) = (
            server_ctx(ctx),
            authorize(ctx, "calls:use", Some(&account_id))?,
        );
        Ok(
            routes::calls::request_call_op(sctx, user, &account_id, user_id, is_video)
                .await?
                .into(),
        )
    }

    /// Accept an incoming 1:1 call (`phone.acceptCall`).
    async fn accept_call(
        &self,
        ctx: &Context<'_>,
        account_id: String,
        call_id: i64,
    ) -> Result<GqlCallInfo> {
        let (sctx, user) = (
            server_ctx(ctx),
            authorize(ctx, "calls:use", Some(&account_id))?,
        );
        Ok(
            routes::calls::accept_call_op(sctx, user, &account_id, call_id)
                .await?
                .into(),
        )
    }

    /// Confirm a 1:1 call after exchanging DH values (`phone.confirmCall`).
    async fn confirm_call(
        &self,
        ctx: &Context<'_>,
        account_id: String,
        call_id: i64,
        g_b: Vec<u8>,
    ) -> Result<GqlCallInfo> {
        let (sctx, user) = (
            server_ctx(ctx),
            authorize(ctx, "calls:use", Some(&account_id))?,
        );
        Ok(
            routes::calls::confirm_call_op(sctx, user, &account_id, call_id, g_b)
                .await?
                .into(),
        )
    }

    /// End a 1:1 call (`phone.discardCall`). `reason`: hangup | missed |
    /// disconnect | busy (default hangup).
    async fn discard_call(
        &self,
        ctx: &Context<'_>,
        account_id: String,
        call_id: i64,
        reason: Option<String>,
    ) -> Result<bool> {
        let (sctx, user) = (
            server_ctx(ctx),
            authorize(ctx, "calls:use", Some(&account_id))?,
        );
        let reason = reason.as_deref().unwrap_or("hangup");
        routes::calls::discard_call_op(sctx, user, &account_id, call_id, reason).await?;
        Ok(true)
    }

    // --- Group calls: full MTProto signaling ------------------------------------

    /// Create a group call in a chat (`phone.createGroupCall`).
    async fn create_group_call(
        &self,
        ctx: &Context<'_>,
        account_id: String,
        chat_id: i64,
        title: Option<String>,
    ) -> Result<GqlGroupCallInfo> {
        let (sctx, user) = (
            server_ctx(ctx),
            authorize(ctx, "calls:use", Some(&account_id))?,
        );
        Ok(
            routes::calls::create_group_call_op(sctx, user, &account_id, chat_id, title)
                .await?
                .into(),
        )
    }

    /// Join a group call (`phone.joinGroupCall`).
    async fn join_group_call(
        &self,
        ctx: &Context<'_>,
        account_id: String,
        call_id: i64,
        access_hash: i64,
        chat_id: i64,
        #[graphql(default = false)] muted: bool,
    ) -> Result<GqlGroupCallInfo> {
        let (sctx, user) = (
            server_ctx(ctx),
            authorize(ctx, "calls:use", Some(&account_id))?,
        );
        Ok(routes::calls::join_group_call_op(
            sctx,
            user,
            &account_id,
            call_id,
            access_hash,
            chat_id,
            muted,
        )
        .await?
        .into())
    }

    /// Leave a group call (`phone.leaveGroupCall`).
    async fn leave_group_call(
        &self,
        ctx: &Context<'_>,
        account_id: String,
        call_id: i64,
    ) -> Result<bool> {
        let (sctx, user) = (
            server_ctx(ctx),
            authorize(ctx, "calls:use", Some(&account_id))?,
        );
        routes::calls::leave_group_call_op(sctx, user, &account_id, call_id).await?;
        Ok(true)
    }

    /// Mute/unmute self in a group call (`phone.editGroupCallParticipant`).
    async fn toggle_group_call_mute(
        &self,
        ctx: &Context<'_>,
        account_id: String,
        call_id: i64,
        muted: bool,
    ) -> Result<bool> {
        let (sctx, user) = (
            server_ctx(ctx),
            authorize(ctx, "calls:use", Some(&account_id))?,
        );
        routes::calls::toggle_group_call_mute_op(sctx, user, &account_id, call_id, muted).await?;
        Ok(true)
    }
}

// --- Subscription ------------------------------------------------------------------

pub struct SubscriptionRoot;

/// Builds a filtered stream over the event bus: only events whose payload
/// `accountId` matches an account the caller owns; optional chatId filter.
fn event_stream(
    ctx: &Context<'_>,
    account_id: String,
    matcher: impl Fn(&str) -> bool + Send + 'static,
    chat_id: Option<i64>,
) -> Result<impl Stream<Item = EventPayload>> {
    let sctx = server_ctx(ctx).clone();
    // Subscriptions consume the same realtime bus as SSE `/events`, so they
    // require the same `events:read` scope (plus the per-account allowlist).
    let user = authorize(ctx, "events:read", Some(&account_id))?;

    // Ownership is checked once at subscribe time; accounts are never
    // re-owned while live (logout releases, but then the stream just goes
    // silent), so per-event re-checks are unnecessary.
    if !sctx.accounts.is_owner(&user.0, &account_id) {
        return Err(ApiError::Forbidden("This account belongs to another user".into()).into());
    }

    let rx = sctx.events.subscribe();
    Ok(BroadcastStream::new(rx).filter_map(move |item| {
        // Lagged subscribers skip missed events (broadcast semantics).
        let event = item.ok()?;
        if !matcher(&event.name) {
            return None;
        }
        if event.payload.get("accountId").and_then(|v| v.as_str()) != Some(account_id.as_str()) {
            return None;
        }
        if let Some(want) = chat_id {
            if event.payload.get("chatId").and_then(|v| v.as_i64()) != Some(want) {
                return None;
            }
        }
        Some(EventPayload {
            event: event.name,
            payload: async_graphql::Json(event.payload),
        })
    }))
}

fn name_in(names: &'static [&'static str]) -> impl Fn(&str) -> bool + Send + 'static {
    move |name| names.contains(&name)
}

#[Subscription]
impl SubscriptionRoot {
    /// `telegram:new-message` events for one account, optionally one chat.
    async fn message_received(
        &self,
        ctx: &Context<'_>,
        account_id: String,
        chat_id: Option<i64>,
    ) -> Result<impl Stream<Item = EventPayload>> {
        event_stream(ctx, account_id, name_in(&["telegram:new-message"]), chat_id)
    }

    /// `telegram:message-edited` events.
    async fn message_edited(
        &self,
        ctx: &Context<'_>,
        account_id: String,
        chat_id: Option<i64>,
    ) -> Result<impl Stream<Item = EventPayload>> {
        event_stream(
            ctx,
            account_id,
            name_in(&["telegram:message-edited"]),
            chat_id,
        )
    }

    /// `telegram:message-deleted` events.
    async fn message_deleted(
        &self,
        ctx: &Context<'_>,
        account_id: String,
        chat_id: Option<i64>,
    ) -> Result<impl Stream<Item = EventPayload>> {
        event_stream(
            ctx,
            account_id,
            name_in(&["telegram:message-deleted"]),
            chat_id,
        )
    }

    /// `chat-loaded` and `chat-avatar-updated` events.
    async fn chat_updated(
        &self,
        ctx: &Context<'_>,
        account_id: String,
    ) -> Result<impl Stream<Item = EventPayload>> {
        event_stream(
            ctx,
            account_id,
            name_in(&["chat-loaded", "chat-avatar-updated"]),
            None,
        )
    }

    /// `chats-loading-complete` events.
    async fn chats_loading_progress(
        &self,
        ctx: &Context<'_>,
        account_id: String,
    ) -> Result<impl Stream<Item = EventPayload>> {
        event_stream(ctx, account_id, name_in(&["chats-loading-complete"]), None)
    }

    /// `connection-status` events (connected / reconnecting / disconnected).
    async fn connection_status(
        &self,
        ctx: &Context<'_>,
        account_id: String,
    ) -> Result<impl Stream<Item = EventPayload>> {
        event_stream(ctx, account_id, name_in(&["connection-status"]), None)
    }

    /// 1:1 call events: `telegram:incoming-call`, `telegram:call-*`.
    async fn call_event(
        &self,
        ctx: &Context<'_>,
        account_id: String,
    ) -> Result<impl Stream<Item = EventPayload>> {
        event_stream(
            ctx,
            account_id,
            |name| name == "telegram:incoming-call" || name.starts_with("telegram:call-"),
            None,
        )
    }

    /// Group call events: `telegram:group-call-*`.
    async fn group_call_event(
        &self,
        ctx: &Context<'_>,
        account_id: String,
    ) -> Result<impl Stream<Item = EventPayload>> {
        event_stream(
            ctx,
            account_id,
            |name| name.starts_with("telegram:group-call-"),
            None,
        )
    }

    /// STT progress events (loading_model, transcribing, whisper-progress, done, …).
    async fn stt_progress(
        &self,
        ctx: &Context<'_>,
        account_id: String,
    ) -> Result<impl Stream<Item = EventPayload>> {
        event_stream(
            ctx,
            account_id,
            name_in(&[
                "loading_model",
                "model_loaded",
                "transcribing",
                "extracting_text",
                "converting_audio",
                "whisper-progress",
                "audio_ready",
                "done",
            ]),
            None,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthMode;
    use crate::ServerOptions;
    use vasya_core::events::EventSink;
    use vasya_core::TelegramClientManager;

    fn test_schema() -> (tempfile::TempDir, Arc<ServerContext>, VasyaSchema) {
        let dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(TelegramClientManager::with_key_provider(
            dir.path().join("sessions"),
            1,
            "hash".into(),
            Arc::new(vasya_core::telegram::master_key::FileKeyProvider::new(
                dir.path().join("master.key"),
            )),
        ));
        let ctx = crate::build_context(
            manager,
            ServerOptions::new(
                AuthMode::EmbeddedLocal { token: "t".into() },
                dir.path().join("data"),
            ),
        )
        .unwrap();
        let schema = build_schema(ctx.clone());
        (dir, ctx, schema)
    }

    #[tokio::test]
    async fn query_without_user_context_is_unauthorized() {
        let (_dir, _ctx, schema) = test_schema();
        let response = schema.execute("{ accounts { accountId } }").await;
        assert!(!response.errors.is_empty());
        assert!(response.errors[0].message.contains("Unauthorized"));
    }

    #[tokio::test]
    async fn accounts_query_executes_with_user() {
        let (_dir, _ctx, schema) = test_schema();
        let request = async_graphql::Request::new("{ accounts { accountId phone } }")
            .data(UserId("local".into()));
        let response = schema.execute(request).await;
        assert!(response.errors.is_empty(), "{:?}", response.errors);
        assert_eq!(
            response.data.into_json().unwrap(),
            serde_json::json!({ "accounts": [] })
        );
    }

    #[tokio::test]
    async fn sdl_lists_subscriptions() {
        let (_dir, _ctx, schema) = test_schema();
        let sdl = schema.sdl();
        for field in [
            "messageReceived",
            "messageEdited",
            "chatUpdated",
            "callEvent",
            "sttProgress",
        ] {
            assert!(sdl.contains(field), "SDL is missing {field}");
        }
    }

    #[tokio::test]
    async fn sdl_lists_call_signaling() {
        let (_dir, _ctx, schema) = test_schema();
        let sdl = schema.sdl();
        // Mutations + query parity with the REST /calls and /group-calls routes.
        for field in [
            "requestCall",
            "acceptCall",
            "confirmCall",
            "discardCall",
            "createGroupCall",
            "joinGroupCall",
            "leaveGroupCall",
            "toggleGroupCallMute",
            "groupCallParticipants",
        ] {
            assert!(sdl.contains(field), "SDL is missing {field}");
        }
    }

    #[tokio::test]
    async fn message_received_filters_by_account_and_chat() {
        let (_dir, ctx, schema) = test_schema();
        ctx.accounts.ensure_access("local", "acc-1").unwrap();

        let request = async_graphql::Request::new(
            r#"subscription { messageReceived(accountId: "acc-1", chatId: 7) { event payload } }"#,
        )
        .data(UserId("local".into()));
        let mut stream = schema.execute_stream(request);

        // Emit repeatedly until the (lazily-subscribed) stream picks it up;
        // decoys must be filtered out: wrong account, wrong chat, wrong name.
        let emitter_ctx = ctx.clone();
        let emitter = tokio::spawn(async move {
            loop {
                emitter_ctx.events.emit(
                    "telegram:new-message",
                    serde_json::json!({"accountId": "acc-2", "chatId": 7, "id": 1}),
                );
                emitter_ctx.events.emit(
                    "telegram:new-message",
                    serde_json::json!({"accountId": "acc-1", "chatId": 5, "id": 2}),
                );
                emitter_ctx.events.emit(
                    "telegram:message-edited",
                    serde_json::json!({"accountId": "acc-1", "chatId": 7, "id": 3}),
                );
                emitter_ctx.events.emit(
                    "telegram:new-message",
                    serde_json::json!({"accountId": "acc-1", "chatId": 7, "id": 4}),
                );
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        });

        let response = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
            .await
            .expect("subscription timed out")
            .expect("subscription stream ended");
        emitter.abort();

        assert!(response.errors.is_empty(), "{:?}", response.errors);
        let data = response.data.into_json().unwrap();
        assert_eq!(data["messageReceived"]["event"], "telegram:new-message");
        assert_eq!(data["messageReceived"]["payload"]["accountId"], "acc-1");
        assert_eq!(data["messageReceived"]["payload"]["chatId"], 7);
        assert_eq!(data["messageReceived"]["payload"]["id"], 4);
    }

    #[tokio::test]
    async fn subscription_rejects_foreign_account() {
        let (_dir, ctx, schema) = test_schema();
        ctx.accounts.ensure_access("someone-else", "acc-1").unwrap();

        let request = async_graphql::Request::new(
            r#"subscription { messageReceived(accountId: "acc-1") { event } }"#,
        )
        .data(UserId("local".into()));
        let mut stream = schema.execute_stream(request);

        let response = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
            .await
            .expect("timed out")
            .expect("stream ended");
        assert!(!response.errors.is_empty());
        assert!(response.errors[0]
            .message
            .contains("belongs to another user"));
    }

    fn agent(scopes: &[&str], account_ids: Option<Vec<String>>) -> AgentIdentity {
        AgentIdentity {
            key_id: "test-key".into(),
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
            account_ids,
        }
    }

    #[tokio::test]
    async fn agent_query_scope_enforced() {
        let (_dir, _ctx, schema) = test_schema();

        // Without the accounts:read scope the query is rejected, scope named.
        let request = async_graphql::Request::new("{ accounts { accountId } }")
            .data(UserId("local".into()))
            .data(agent(&[], None));
        let response = schema.execute(request).await;
        assert!(
            response
                .errors
                .iter()
                .any(|e| e.message.contains("accounts:read")),
            "{:?}",
            response.errors
        );

        // With the scope it runs (empty account list).
        let request = async_graphql::Request::new("{ accounts { accountId } }")
            .data(UserId("local".into()))
            .data(agent(&["accounts:read"], None));
        let response = schema.execute(request).await;
        assert!(response.errors.is_empty(), "{:?}", response.errors);
    }

    #[tokio::test]
    async fn agent_subscription_requires_events_scope() {
        let (_dir, ctx, schema) = test_schema();
        ctx.accounts.ensure_access("local", "acc-1").unwrap();

        let request = async_graphql::Request::new(
            r#"subscription { messageReceived(accountId: "acc-1") { event } }"#,
        )
        .data(UserId("local".into()))
        .data(agent(&[], None));
        let mut stream = schema.execute_stream(request);

        let response = stream.next().await.expect("stream ended");
        assert!(
            response
                .errors
                .iter()
                .any(|e| e.message.contains("events:read")),
            "{:?}",
            response.errors
        );
    }

    #[tokio::test]
    async fn agent_subscription_enforces_account_allowlist() {
        let (_dir, ctx, schema) = test_schema();
        ctx.accounts.ensure_access("local", "acc-1").unwrap();

        // Holds events:read but the account is outside the key's allowlist.
        let request = async_graphql::Request::new(
            r#"subscription { messageReceived(accountId: "acc-1") { event } }"#,
        )
        .data(UserId("local".into()))
        .data(agent(&["events:read"], Some(vec!["acc-2".into()])));
        let mut stream = schema.execute_stream(request);

        let response = stream.next().await.expect("stream ended");
        assert!(
            response
                .errors
                .iter()
                .any(|e| e.message.contains("allowlist")),
            "{:?}",
            response.errors
        );
    }

    /// The GraphQL resolver scopes must match the REST `required_scope` map
    /// for the analogous endpoint, so the two transports can't drift apart.
    #[test]
    fn graphql_scopes_mirror_rest() {
        use crate::policy::required_scope;
        use axum::http::Method;
        let cases: &[(&str, &str, &str, &[&str])] = &[
            ("accounts", "accounts:read", "GET", &["accounts"]),
            (
                "credentialsConfigured",
                "telegram:login",
                "GET",
                &["telegram", "credentials"],
            ),
            ("chats", "chats:read", "GET", &["accounts", "a", "chats"]),
            (
                "contacts",
                "chats:read",
                "GET",
                &["accounts", "a", "contacts"],
            ),
            (
                "messages",
                "messages:read",
                "GET",
                &["accounts", "a", "chats", "5", "messages"],
            ),
            (
                "searchMessages",
                "messages:read",
                "GET",
                &["accounts", "a", "chats", "5", "messages", "search"],
            ),
            (
                "globalSearch",
                "chats:read",
                "GET",
                &["accounts", "a", "search"],
            ),
            (
                "searchAllMessages",
                "messages:read",
                "GET",
                &["accounts", "a", "messages", "search"],
            ),
            (
                "forumTopics",
                "chats:read",
                "GET",
                &["accounts", "a", "chats", "5", "topics"],
            ),
            (
                "folders",
                "folders:read",
                "GET",
                &["accounts", "a", "folders"],
            ),
            ("tabs", "folders:read", "GET", &["accounts", "a", "tabs"]),
            (
                "groupCallParticipants",
                "calls:use",
                "GET",
                &["accounts", "a", "group-calls", "participants"],
            ),
            (
                "requestLoginCode",
                "telegram:login",
                "POST",
                &["telegram", "login", "code"],
            ),
            ("logout", "accounts:delete", "DELETE", &["accounts", "a"]),
            (
                "sendMessage",
                "messages:send",
                "POST",
                &["accounts", "a", "chats", "5", "messages"],
            ),
            (
                "forwardMessages",
                "messages:forward",
                "POST",
                &["accounts", "a", "messages", "forward"],
            ),
            (
                "markMessagesRead",
                "messages:send",
                "POST",
                &["accounts", "a", "chats", "5", "read"],
            ),
            (
                "startLoadingChats",
                "chats:read",
                "POST",
                &["accounts", "a", "chats", "load"],
            ),
            (
                "deleteAndLeaveChat",
                "chats:delete",
                "DELETE",
                &["accounts", "a", "chats", "5"],
            ),
            (
                "createGroup",
                "chats:write",
                "POST",
                &["accounts", "a", "groups"],
            ),
            (
                "createChannel",
                "chats:write",
                "POST",
                &["accounts", "a", "channels"],
            ),
            (
                "saveFolder",
                "folders:write",
                "POST",
                &["accounts", "a", "folders"],
            ),
            (
                "saveTabs",
                "folders:write",
                "PUT",
                &["accounts", "a", "tabs"],
            ),
            (
                "requestCall",
                "calls:use",
                "POST",
                &["accounts", "a", "calls", "request"],
            ),
            (
                "createGroupCall",
                "calls:use",
                "POST",
                &["accounts", "a", "group-calls"],
            ),
            ("messageReceived", "events:read", "GET", &["events"]),
        ];
        for (label, scope, method, segments) in cases {
            let method: Method = method.parse().unwrap();
            assert_eq!(required_scope(&method, segments), Some(*scope), "{label}");
        }
    }
}
