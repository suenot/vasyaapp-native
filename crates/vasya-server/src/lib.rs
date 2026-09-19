// The hand-built OpenAPI document is one deep serde_json::json! literal.
#![recursion_limit = "256"]

//! vasya-server — Telegram session host as a library.
//!
//! Structural requirement (plan §1 №6): this is a *library* exposing a
//! router builder over a vasya-core engine handle, plus a thin standalone
//! binary (src/main.rs). The Tauri desktop app can mount the same router
//! on 127.0.0.1 in-process over the sessions its UI already uses
//! (embedded-local AuthMode, no Postgres), so local AI agents/MCP control
//! the running app; the standalone server runs the same code with JWT
//! auth and env-injected session master key.

pub mod accounts;
pub mod agent_keys;
pub mod audit;
pub mod auth;
pub mod context;
pub mod dto;
pub mod error;
pub mod flood;
pub mod graphql;
pub mod idempotency;
pub mod openapi;
pub mod peer;
pub mod policy;
pub mod rate_limit;
pub mod routes;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use vasya_core::events::BroadcastEventSink;
use vasya_core::TelegramClientManager;

pub use auth::AuthMode;
pub use context::ServerContext;
pub use rate_limit::RateLimitConfig;

/// Options for assembling a server context around an existing engine.
pub struct ServerOptions {
    pub auth: AuthMode,
    /// Who may manage global server settings. Defaults from the auth mode in
    /// `new()` (embedded → the local owner; JWT → none). The standalone binary
    /// overrides this from `VASYA_ADMIN_USERS`.
    pub admins: auth::AdminPolicy,
    /// Directory for accounts.json, folder/tab stores and the media cache.
    pub data_dir: PathBuf,
    pub rate_limit: RateLimitConfig,
    /// Stricter per-key mutation quota for agent keys (plan §12).
    pub agent_rate_limit: RateLimitConfig,
    /// How long Idempotency-Key responses are replayable.
    pub idempotency_ttl: std::time::Duration,
    /// Broadcast bus capacity (events buffered per lagging subscriber).
    pub events_capacity: usize,
    /// Serve the GraphQL playground page (dev only).
    pub graphql_playground: bool,
}

impl ServerOptions {
    pub fn new(auth: AuthMode, data_dir: PathBuf) -> Self {
        // Default admin policy follows the auth mode: the embedded desktop owner
        // is the admin; a standalone JWT server has no admins until configured
        // (see main.rs / VASYA_ADMIN_USERS).
        let admins = match &auth {
            AuthMode::EmbeddedLocal { .. } => auth::AdminPolicy::embedded_local(),
            AuthMode::Jwt { .. } => auth::AdminPolicy::default(),
        };
        Self {
            auth,
            admins,
            data_dir,
            rate_limit: RateLimitConfig::default(),
            agent_rate_limit: RateLimitConfig {
                capacity: 5,
                refill_every: std::time::Duration::from_secs(5),
            },
            idempotency_ttl: std::time::Duration::from_secs(24 * 60 * 60),
            events_capacity: 1024,
            graphql_playground: false,
        }
    }
}

/// Build the shared server state around an existing `TelegramClientManager`.
///
/// The desktop app passes its own manager here (embedded mode); the
/// standalone binary creates one first. Update pumps are NOT started here —
/// call [`start_existing_sessions`] (standalone) or wire the pumps yourself
/// (the desktop app already runs them with its own sink).
pub fn build_context(
    manager: Arc<TelegramClientManager>,
    options: ServerOptions,
) -> Result<Arc<ServerContext>> {
    std::fs::create_dir_all(&options.data_dir).context("failed to create data dir")?;
    let media_dir = options.data_dir.join("media");
    std::fs::create_dir_all(&media_dir).context("failed to create media dir")?;

    let accounts = accounts::AccountStore::open(options.data_dir.join("accounts.json"))?;
    let agent_keys = agent_keys::AgentKeyStore::open(options.data_dir.join("agent-keys.json"))?;
    let audit = audit::AuditLog::open(options.data_dir.join("audit.log"))?;

    Ok(Arc::new(ServerContext {
        manager,
        events: Arc::new(BroadcastEventSink::new(options.events_capacity)),
        auth: options.auth,
        admins: options.admins,
        accounts,
        rate: rate_limit::RateLimiter::new(options.rate_limit),
        agent_keys,
        agent_rate: rate_limit::RateLimiter::new(options.agent_rate_limit),
        audit,
        idempotency: idempotency::IdempotencyStore::new(options.idempotency_ttl),
        chat_cache: tokio::sync::RwLock::new(Default::default()),
        pending_logins: tokio::sync::Mutex::new(Default::default()),
        pending_passwords: tokio::sync::Mutex::new(Default::default()),
        active_calls: Arc::new(tokio::sync::RwLock::new(Default::default())),
        active_group_calls: Arc::new(tokio::sync::RwLock::new(Default::default())),
        media_dir,
        data_dir: options.data_dir,
        graphql_playground: options.graphql_playground,
    }))
}

/// The complete axum application (all /api/v1 routes + auth middleware).
pub fn build_router(ctx: Arc<ServerContext>) -> axum::Router {
    routes::api_router(ctx)
}

/// Load sessions from disk and start an update pump (events → bus) for each.
/// Standalone-server boot path; returns the loaded account ids.
pub async fn start_existing_sessions(ctx: &ServerContext) -> Result<Vec<String>> {
    let loaded = ctx
        .manager
        .load_existing_sessions()
        .await
        .context("failed to load sessions")?;

    for account_id in &loaded {
        if let Err(e) = ctx
            .manager
            .start_updates(account_id, ctx.updates_context())
            .await
        {
            tracing::warn!(account_id = %account_id, error = %e, "Failed to start updates for loaded session");
        }
    }
    Ok(loaded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn test_app(token: &str) -> (tempfile::TempDir, axum::Router) {
        let dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(TelegramClientManager::with_key_provider(
            dir.path().join("sessions"),
            1,
            "hash".into(),
            Arc::new(vasya_core::telegram::master_key::FileKeyProvider::new(
                dir.path().join("master.key"),
            )),
        ));
        let ctx = build_context(
            manager,
            ServerOptions::new(
                AuthMode::EmbeddedLocal {
                    token: token.into(),
                },
                dir.path().join("data"),
            ),
        )
        .unwrap();
        (dir, build_router(ctx))
    }

    async fn body_json(response: axum::response::Response) -> serde_json::Value {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn translation_is_human_only_bounded_and_never_exposes_key() {
        let (_dir, app) = test_app("tok");
        let unauth = app
            .clone()
            .oneshot(
                Request::get("/api/v1/translation/settings")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);
        let settings = serde_json::json!({"base_url":"https://provider.example/v1", "model":"translator", "api_key":"private-translation-token"});
        let saved = app
            .clone()
            .oneshot(
                Request::put("/api/v1/translation/settings")
                    .header("authorization", "Bearer tok")
                    .header("content-type", "application/json")
                    .body(Body::from(settings.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(saved.status(), StatusCode::OK);
        let public = body_json(saved).await;
        assert_eq!(public["api_key_set"], true);
        assert!(public.get("api_key").is_none());
        assert!(!public.to_string().contains("private-translation-token"));
        let loaded = app
            .clone()
            .oneshot(
                Request::get("/api/v1/translation/settings")
                    .header("authorization", "Bearer tok")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(body_json(loaded).await, public);
        let (_, agent) = create_agent_key(&app, &["stt:use", "accounts:read"]).await;
        for (method, path, body) in [
            ("GET", "/api/v1/translation/settings", "{}"),
            ("PUT", "/api/v1/translation/settings", "{}"),
            (
                "POST",
                "/api/v1/translation/translate",
                r#"{"text":"Hello","targetLanguage":"French"}"#,
            ),
        ] {
            let res = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .header("authorization", format!("Bearer {agent}"))
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::FORBIDDEN, "{method} {path}");
        }
        for (text, expected) in [
            (String::new(), StatusCode::BAD_REQUEST),
            ("x".repeat(32 * 1024 + 1), StatusCode::BAD_REQUEST),
            ("x".repeat(256 * 1024), StatusCode::PAYLOAD_TOO_LARGE),
        ] {
            let res = app
                .clone()
                .oneshot(
                    Request::post("/api/v1/translation/translate")
                        .header("authorization", "Bearer tok")
                        .header("content-type", "application/json")
                        .body(Body::from(
                            serde_json::json!({"text":text,"targetLanguage":"French"}).to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), expected);
            let bytes = res.into_body().collect().await.unwrap().to_bytes();
            assert!(!String::from_utf8_lossy(&bytes).contains("private-translation-token"));
        }
    }

    #[tokio::test]
    async fn health_is_public() {
        let (_dir, app) = test_app("tok");
        let res = app
            .oneshot(Request::get("/api/v1/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(body_json(res).await["status"], "ok");
    }

    #[tokio::test]
    async fn openapi_is_public_and_lists_paths() {
        let (_dir, app) = test_app("tok");
        let res = app
            .oneshot(
                Request::get("/api/v1/openapi.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let doc = body_json(res).await;
        assert_eq!(doc["openapi"], "3.0.3");
        assert!(doc["paths"]["/api/v1/accounts/{acc}/chats"].is_object());
    }

    #[tokio::test]
    async fn protected_routes_require_token() {
        let (_dir, app) = test_app("tok");

        let res = app
            .clone()
            .oneshot(
                Request::get("/api/v1/accounts")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        let res = app
            .clone()
            .oneshot(
                Request::get("/api/v1/accounts")
                    .header("Authorization", "Bearer wrong")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        let res = app
            .oneshot(
                Request::get("/api/v1/accounts")
                    .header("Authorization", "Bearer tok")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(body_json(res).await, serde_json::json!([]));
    }

    /// storage-mode is no longer a 501 stub: GET reports the server's fixed
    /// storage mode (200), PUT rejects changes with a 400 — the local/remote
    /// toggle is desktop-only. (Mirrors the STT "structured, not 501" pattern.)
    #[tokio::test]
    async fn storage_mode_reports_fixed_not_501() {
        let (_dir, app) = test_app("tok");

        let res = app
            .clone()
            .oneshot(
                Request::get("/api/v1/storage-mode")
                    .header("Authorization", "Bearer tok")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = body_json(res).await;
        assert_eq!(body["mode"], serde_json::json!("server"));
        assert_eq!(body["configurable"], serde_json::json!(false));

        // PUT can't change the fixed server storage mode → 400, never 501.
        let res = app
            .oneshot(
                Request::put("/api/v1/storage-mode")
                    .header("Authorization", "Bearer tok")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"mode":"local"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    /// STT is implemented on the server (cloud Deepgram). The local-Whisper
    /// model catalog is reported as unavailable via a structured 200, NOT a 501.
    #[tokio::test]
    async fn stt_models_report_unavailable_not_501() {
        let (_dir, app) = test_app("tok");
        let res = app
            .oneshot(
                Request::get("/api/v1/stt/models")
                    .header("Authorization", "Bearer tok")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = body_json(res).await;
        assert_eq!(body["available"], serde_json::json!(false));
    }

    /// 1:1 call audio (mute/volume) is client-side only → documented 501 with
    /// an explanation, even though call *signaling* is implemented.
    #[tokio::test]
    async fn call_audio_endpoints_document_501() {
        let (_dir, app) = test_app("tok");
        for path in [
            "/api/v1/accounts/a1/calls/mute",
            "/api/v1/accounts/a1/calls/volume",
        ] {
            let res = app
                .clone()
                .oneshot(
                    Request::post(path)
                        .header("Authorization", "Bearer tok")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::NOT_IMPLEMENTED, "{path}");
            let body = body_json(res).await;
            assert!(
                body["error"].as_str().unwrap().contains("client-side"),
                "expected client-side audio explanation, got {body}"
            );
        }
    }

    /// Call *signaling* routes are implemented (no longer 501): with a valid
    /// body but no live client for the account they resolve to 404, proving
    /// the request reached the handler instead of a stub.
    #[tokio::test]
    async fn call_signaling_routes_are_implemented() {
        let (_dir, app) = test_app("tok");
        let cases = [
            (
                "/api/v1/accounts/a1/calls/request",
                r#"{"userId":42,"isVideo":false}"#,
            ),
            ("/api/v1/accounts/a1/group-calls", r#"{"chatId":42}"#),
        ];
        for (path, json) in cases {
            let res = app
                .clone()
                .oneshot(
                    Request::post(path)
                        .header("Authorization", "Bearer tok")
                        .header("content-type", "application/json")
                        .body(Body::from(json))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::NOT_FOUND, "{path}");
            assert_ne!(res.status(), StatusCode::NOT_IMPLEMENTED, "{path}");
        }
    }

    /// The audit/policy layer maps every /calls/* and /group-calls/* path to
    /// the `calls:use` scope (see policy.rs) — here we assert OpenAPI advertises
    /// the call surface as live (op-style) rather than as 501 stubs.
    #[tokio::test]
    async fn openapi_advertises_call_signaling() {
        let (_dir, app) = test_app("tok");
        let res = app
            .oneshot(
                Request::get("/api/v1/openapi.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let doc = body_json(res).await;
        let request = &doc["paths"]["/api/v1/accounts/{acc}/calls/request"]["post"];
        assert!(
            request["responses"]["200"].is_object(),
            "calls/request should be 200/live"
        );
        assert!(request["responses"]["501"].is_null());
        // Audio-only endpoints stay documented 501.
        let mute = &doc["paths"]["/api/v1/accounts/{acc}/calls/mute"]["post"];
        assert!(mute["responses"]["501"].is_object(), "calls/mute stays 501");
    }

    #[tokio::test]
    async fn unknown_account_is_404_after_claim() {
        let (_dir, app) = test_app("tok");
        // Account gets claimed on first touch, but no client exists -> 404.
        let res = app
            .oneshot(
                Request::get("/api/v1/accounts/nope/chats")
                    .header("Authorization", "Bearer tok")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn graphql_post_requires_auth_and_executes() {
        let (_dir, app) = test_app("tok");

        let res = app
            .clone()
            .oneshot(
                Request::post("/api/v1/graphql")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"query":"{ accounts { accountId } }"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        let res = app
            .oneshot(
                Request::post("/api/v1/graphql")
                    .header("Authorization", "Bearer tok")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"query":"{ accounts { accountId } }"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            body_json(res).await,
            serde_json::json!({ "data": { "accounts": [] } })
        );
    }

    #[tokio::test]
    async fn graphql_sdl_is_public() {
        let (_dir, app) = test_app("tok");
        let res = app
            .oneshot(
                Request::get("/api/v1/graphql/sdl")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = res.into_body().collect().await.unwrap().to_bytes();
        let sdl = String::from_utf8(body.to_vec()).unwrap();
        assert!(sdl.contains("messageReceived"));
    }

    #[tokio::test]
    async fn playground_is_gated_by_option() {
        let (_dir, app) = test_app("tok");
        let res = app
            .oneshot(
                Request::get("/api/v1/graphql/playground")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        // With the dev flag on, the page is served.
        let dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(TelegramClientManager::with_key_provider(
            dir.path().join("sessions"),
            1,
            "hash".into(),
            Arc::new(vasya_core::telegram::master_key::FileKeyProvider::new(
                dir.path().join("master.key"),
            )),
        ));
        let mut options = ServerOptions::new(
            AuthMode::EmbeddedLocal {
                token: "tok".into(),
            },
            dir.path().join("data"),
        );
        options.graphql_playground = true;
        let ctx = build_context(manager, options).unwrap();
        let app = build_router(ctx);
        let res = app
            .oneshot(
                Request::get("/api/v1/graphql/playground")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    async fn create_agent_key(app: &axum::Router, scopes: &[&str]) -> (String, String) {
        let body = serde_json::json!({ "name": "test-bot", "scopes": scopes }).to_string();
        let res = app
            .clone()
            .oneshot(
                Request::post("/api/v1/agent-keys")
                    .header("Authorization", "Bearer tok")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let json = body_json(res).await;
        (
            json["id"].as_str().unwrap().to_string(),
            json["secret"].as_str().unwrap().to_string(),
        )
    }

    #[tokio::test]
    async fn agent_key_scopes_enforced_end_to_end() {
        let (_dir, app) = test_app("tok");
        let (_id, secret) = create_agent_key(&app, &["accounts:read", "chats:read"]).await;

        // In-scope read works (empty account list).
        let res = app
            .clone()
            .oneshot(
                Request::get("/api/v1/accounts")
                    .header("Authorization", format!("Bearer {secret}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // Out-of-scope mutation is rejected with the missing scope named.
        let res = app
            .clone()
            .oneshot(
                Request::post("/api/v1/accounts/a1/chats/5/messages")
                    .header("Authorization", format!("Bearer {secret}"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"text":"hi"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert!(body_json(res).await["error"]
            .as_str()
            .unwrap()
            .contains("messages:send"));

        // Agents still cannot manage keys or read the audit log. (GraphQL is
        // now allowed with per-resolver scope enforcement — see
        // `graphql_respects_agent_scopes`.)
        for (method, path) in [("GET", "/api/v1/agent-keys"), ("GET", "/api/v1/audit")] {
            let res = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .header("Authorization", format!("Bearer {secret}"))
                        .header("content-type", "application/json")
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::FORBIDDEN, "{method} {path}");
        }
    }

    /// POST a GraphQL query with the given bearer; returns the JSON body.
    /// GraphQL always answers 200 (errors live in the `errors` array).
    async fn graphql_query(app: &axum::Router, bearer: &str, query: &str) -> serde_json::Value {
        let body = serde_json::json!({ "query": query }).to_string();
        let res = app
            .clone()
            .oneshot(
                Request::post("/api/v1/graphql")
                    .header("Authorization", format!("Bearer {bearer}"))
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        body_json(res).await
    }

    #[tokio::test]
    async fn graphql_respects_agent_scopes() {
        let (_dir, app) = test_app("tok");
        let (_id, reader) = create_agent_key(&app, &["accounts:read"]).await;

        // In-scope query runs: the accounts list resolves (empty here).
        let json = graphql_query(&app, &reader, "{ accounts { accountId } }").await;
        assert!(json.get("errors").is_none(), "unexpected errors: {json}");
        assert_eq!(json["data"]["accounts"], serde_json::json!([]));

        // A query the key lacks the scope for is rejected, naming the scope.
        let json = graphql_query(&app, &reader, r#"{ chats(accountId: "a1") { id } }"#).await;
        let msg = json["errors"][0]["message"].as_str().unwrap();
        assert!(
            msg.contains("chats:read"),
            "expected scope error, got: {msg}"
        );

        // A mutation the key lacks the scope for is rejected too.
        let json = graphql_query(
            &app,
            &reader,
            r#"mutation { sendMessage(accountId: "a1", chatId: 5, text: "hi") { id } }"#,
        )
        .await;
        let msg = json["errors"][0]["message"].as_str().unwrap();
        assert!(
            msg.contains("messages:send"),
            "expected scope error, got: {msg}"
        );
    }

    #[tokio::test]
    async fn graphql_respects_agent_account_allowlist() {
        let (_dir, app) = test_app("tok");

        // A key restricted to acc-allowed holding chats:read.
        let body = serde_json::json!({
            "name": "scoped-bot",
            "scopes": ["chats:read"],
            "accountIds": ["acc-allowed"],
        })
        .to_string();
        let res = app
            .clone()
            .oneshot(
                Request::post("/api/v1/agent-keys")
                    .header("Authorization", "Bearer tok")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let secret = body_json(res).await["secret"].as_str().unwrap().to_string();

        // Targeting a non-listed account via GraphQL hits the allowlist gate.
        let json =
            graphql_query(&app, &secret, r#"{ chats(accountId: "acc-other") { id } }"#).await;
        let msg = json["errors"][0]["message"].as_str().unwrap();
        assert!(
            msg.contains("allowlist"),
            "expected allowlist error, got: {msg}"
        );

        // The listed account clears the scope + allowlist gates (it then fails
        // later for the missing client, never with a scope/allowlist error).
        let json = graphql_query(
            &app,
            &secret,
            r#"{ chats(accountId: "acc-allowed") { id } }"#,
        )
        .await;
        if let Some(err) = json.get("errors") {
            let msg = err[0]["message"].as_str().unwrap();
            assert!(
                !msg.contains("scope") && !msg.contains("allowlist"),
                "listed account blocked by gate: {msg}"
            );
        }
    }

    /// STT transcribe-by-reference targets an account via the JSON body, not
    /// the URL path, so the path-based allowlist gate in `policy.rs` can't see
    /// it — the `/stt/transcribe` handler enforces the allowlist itself. A key
    /// scoped to one account must not transcribe another account's voice
    /// message, even with `stt:use`.
    #[tokio::test]
    async fn stt_transcribe_respects_agent_account_allowlist() {
        let (_dir, app) = test_app("tok");

        // A key allowed only on acc-allowed, holding stt:use.
        let body = serde_json::json!({
            "name": "stt-bot",
            "scopes": ["stt:use"],
            "accountIds": ["acc-allowed"],
        })
        .to_string();
        let res = app
            .clone()
            .oneshot(
                Request::post("/api/v1/agent-keys")
                    .header("Authorization", "Bearer tok")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let secret = body_json(res).await["secret"].as_str().unwrap().to_string();

        // Transcribing a voice message in a NON-listed account is blocked by
        // the allowlist gate before any Telegram work happens.
        let res = app
            .clone()
            .oneshot(
                Request::post("/api/v1/stt/transcribe")
                    .header("Authorization", format!("Bearer {secret}"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"accountId":"acc-other","chatId":5,"messageId":9}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert!(body_json(res).await["error"]
            .as_str()
            .unwrap()
            .contains("allowlist"));
    }

    /// SSE `/events` is not under `/accounts/{acc}/…`, so the path-based
    /// allowlist gate can't see it — the handler enforces the agent allowlist
    /// itself. A key scoped to one account must not stream another's events.
    #[tokio::test]
    async fn sse_events_respects_agent_account_allowlist() {
        let (_dir, app) = test_app("tok");

        // A key allowed only on acc-allowed, holding events:read.
        let body = serde_json::json!({
            "name": "sse-bot",
            "scopes": ["events:read"],
            "accountIds": ["acc-allowed"],
        })
        .to_string();
        let res = app
            .clone()
            .oneshot(
                Request::post("/api/v1/agent-keys")
                    .header("Authorization", "Bearer tok")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let secret = body_json(res).await["secret"].as_str().unwrap().to_string();

        // A non-listed account is rejected up-front (not a silent empty stream).
        let res = app
            .clone()
            .oneshot(
                Request::get("/api/v1/events?account=acc-other")
                    .header("Authorization", format!("Bearer {secret}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);

        // The listed account opens the stream.
        let res = app
            .oneshot(
                Request::get("/api/v1/events?account=acc-allowed")
                    .header("Authorization", format!("Bearer {secret}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    /// Credential management is human-session-only and the global default is
    /// admin-only. Critically, an agent key whose owner (`local`) is an admin
    /// must NOT inherit admin/credential rights — admin privileges never leak
    /// to agent keys.
    #[tokio::test]
    async fn telegram_credentials_human_only_and_admin_gated() {
        let (_dir, app) = test_app("tok");
        let creds = r#"{"apiId":111,"apiHash":"abcdef0123456789"}"#;

        // Human local session (admin in embedded mode) sets the global default.
        let res = app
            .clone()
            .oneshot(
                Request::put("/api/v1/admin/telegram/credentials")
                    .header("Authorization", "Bearer tok")
                    .header("content-type", "application/json")
                    .body(Body::from(creds))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // Human sets their OWN per-user creds; GET reflects source=user.
        let res = app
            .clone()
            .oneshot(
                Request::put("/api/v1/telegram/credentials")
                    .header("Authorization", "Bearer tok")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"apiId":222,"apiHash":"fedcba9876543210"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let res = app
            .clone()
            .oneshot(
                Request::get("/api/v1/telegram/credentials")
                    .header("Authorization", "Bearer tok")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = body_json(res).await;
        assert_eq!(body["source"], serde_json::json!("user"));
        assert_eq!(body["apiId"], serde_json::json!(222));
        assert_eq!(body["isAdmin"], serde_json::json!(true));

        // An agent key (owner = local, an admin) is BLOCKED from both the admin
        // route and credential management — admin/cred rights don't leak to keys.
        let (_id, secret) = create_agent_key(&app, &["telegram:login"]).await;
        for path in [
            "/api/v1/admin/telegram/credentials",
            "/api/v1/telegram/credentials",
        ] {
            let res = app
                .clone()
                .oneshot(
                    Request::put(path)
                        .header("Authorization", format!("Bearer {secret}"))
                        .header("content-type", "application/json")
                        .body(Body::from(creds))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::FORBIDDEN, "{path}");
        }
    }

    /// The web build authenticates via an HttpOnly session cookie. Safe methods
    /// need no CSRF token; state-changing methods require a matching
    /// double-submit CSRF token (the cookie is auto-sent cross-site, the header
    /// is not — so requiring the header defeats CSRF).
    #[tokio::test]
    async fn cookie_session_auth_with_csrf() {
        let (_dir, app) = test_app("tok");

        // GET with the session cookie authenticates (safe method, no CSRF).
        let res = app
            .clone()
            .oneshot(
                Request::get("/api/v1/accounts")
                    .header("cookie", "vasya_token=tok")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let creds = r#"{"apiId":1,"apiHash":"abcdef0123456789"}"#;

        // Cookie-authed mutation WITHOUT a CSRF token is rejected.
        let res = app
            .clone()
            .oneshot(
                Request::put("/api/v1/telegram/credentials")
                    .header("cookie", "vasya_token=tok")
                    .header("content-type", "application/json")
                    .body(Body::from(creds))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);

        // Matching double-submit CSRF token → passes.
        let res = app
            .clone()
            .oneshot(
                Request::put("/api/v1/telegram/credentials")
                    .header("cookie", "vasya_token=tok; vasya_csrf=csrf-abc")
                    .header("x-csrf-token", "csrf-abc")
                    .header("content-type", "application/json")
                    .body(Body::from(creds))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // Wrong CSRF token → rejected.
        let res = app
            .oneshot(
                Request::put("/api/v1/telegram/credentials")
                    .header("cookie", "vasya_token=tok; vasya_csrf=csrf-abc")
                    .header("x-csrf-token", "WRONG")
                    .header("content-type", "application/json")
                    .body(Body::from(creds))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn split_destructive_scopes_enforced() {
        let (_dir, app) = test_app("tok");

        // A key with login + chat-create but NOT the new destructive scopes
        // can no longer log out an account or delete a chat.
        let (_id, secret) = create_agent_key(&app, &["telegram:login", "chats:write"]).await;

        let forbidden = |method: &'static str, path: &'static str, want_scope: &'static str| {
            let app = app.clone();
            let secret = secret.clone();
            async move {
                let res = app
                    .oneshot(
                        Request::builder()
                            .method(method)
                            .uri(path)
                            .header("Authorization", format!("Bearer {secret}"))
                            .header("content-type", "application/json")
                            .body(Body::from("{}"))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(res.status(), StatusCode::FORBIDDEN, "{method} {path}");
                assert!(
                    body_json(res).await["error"]
                        .as_str()
                        .unwrap()
                        .contains(want_scope),
                    "{method} {path} should name {want_scope}"
                );
            }
        };

        // DELETE account now needs accounts:delete, not telegram:login.
        forbidden("DELETE", "/api/v1/accounts/a1", "accounts:delete").await;
        // DELETE chat now needs chats:delete, not chats:write.
        forbidden("DELETE", "/api/v1/accounts/a1/chats/5", "chats:delete").await;
        // Forward now needs messages:forward, not messages:send.
        let (_id, send_only) = create_agent_key(&app, &["messages:send"]).await;
        let res = app
            .clone()
            .oneshot(
                Request::post("/api/v1/accounts/a1/messages/forward")
                    .header("Authorization", format!("Bearer {send_only}"))
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert!(body_json(res).await["error"]
            .as_str()
            .unwrap()
            .contains("messages:forward"));

        // A key that DOES hold the destructive scope clears the scope gate
        // (it then fails later for the missing account, never with a scope error).
        let (_id, deleter) = create_agent_key(&app, &["accounts:delete"]).await;
        let res = app
            .oneshot(
                Request::delete("/api/v1/accounts/a1")
                    .header("Authorization", format!("Bearer {deleter}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = res.status();
        if status == StatusCode::FORBIDDEN {
            let err = body_json(res).await["error"].as_str().unwrap().to_string();
            assert!(!err.contains("scope"), "scope gate should pass, got: {err}");
        }
    }

    #[tokio::test]
    async fn per_account_allowlist_enforced() {
        let (_dir, app) = test_app("tok");

        // Create a key restricted to a single account via the allowlist.
        let body = serde_json::json!({
            "name": "scoped-bot",
            "scopes": ["chats:read"],
            "accountIds": ["acc-allowed"],
        })
        .to_string();
        let res = app
            .clone()
            .oneshot(
                Request::post("/api/v1/agent-keys")
                    .header("Authorization", "Bearer tok")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let json = body_json(res).await;
        assert_eq!(
            json["accountIds"].as_array().unwrap()[0].as_str().unwrap(),
            "acc-allowed"
        );
        let secret = json["secret"].as_str().unwrap().to_string();

        // A non-listed account is rejected with the allowlist error.
        let res = app
            .clone()
            .oneshot(
                Request::get("/api/v1/accounts/acc-other/chats")
                    .header("Authorization", format!("Bearer {secret}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert!(body_json(res).await["error"]
            .as_str()
            .unwrap()
            .contains("allowlist"));

        // The listed account clears the allowlist gate (no allowlist error).
        let res = app
            .oneshot(
                Request::get("/api/v1/accounts/acc-allowed/chats")
                    .header("Authorization", format!("Bearer {secret}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        if res.status() == StatusCode::FORBIDDEN {
            let err = body_json(res).await["error"].as_str().unwrap().to_string();
            assert!(!err.contains("allowlist"), "listed account blocked: {err}");
        }
    }

    #[tokio::test]
    async fn revoked_agent_key_is_unauthorized() {
        let (_dir, app) = test_app("tok");
        let (id, secret) = create_agent_key(&app, &["accounts:read"]).await;

        let res = app
            .clone()
            .oneshot(
                Request::delete(format!("/api/v1/agent-keys/{id}"))
                    .header("Authorization", "Bearer tok")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);

        let res = app
            .oneshot(
                Request::get("/api/v1/accounts")
                    .header("Authorization", format!("Bearer {secret}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn audit_records_mutations_with_agent_identity() {
        let (_dir, app) = test_app("tok");
        let (id, secret) = create_agent_key(&app, &["folders:write", "folders:read"]).await;

        // Agent performs a mutation (folder save claims acc + writes file).
        let folder = serde_json::json!({
            "id": "f1", "account_id": "acc-a", "name": "Work", "icon": null,
            "included_chat_types": [], "excluded_chat_types": [],
            "included_chat_ids": [], "excluded_chat_ids": [], "sort_order": 1
        });
        let res = app
            .clone()
            .oneshot(
                Request::post("/api/v1/accounts/acc-a/folders")
                    .header("Authorization", format!("Bearer {secret}"))
                    .header("content-type", "application/json")
                    .body(Body::from(folder.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);

        // The audit log has the row, attributed to the agent key.
        let res = app
            .oneshot(
                Request::get("/api/v1/audit?limit=10")
                    .header("Authorization", "Bearer tok")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let entries = body_json(res).await;
        let entry = entries
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["path"] == "/api/v1/accounts/acc-a/folders")
            .expect("audit row for the mutation");
        assert_eq!(entry["method"], "POST");
        assert_eq!(entry["status"], 204);
        assert_eq!(entry["agentKeyId"], id.as_str());
        assert_eq!(entry["userId"], "local");
    }

    #[tokio::test]
    async fn idempotency_key_replays_response() {
        let (_dir, app) = test_app("tok");
        let folder = serde_json::json!({
            "id": "f1", "account_id": "acc-b", "name": "Inbox", "icon": null,
            "included_chat_types": [], "excluded_chat_types": [],
            "included_chat_ids": [], "excluded_chat_ids": [], "sort_order": 1
        })
        .to_string();

        let request = |app: &axum::Router| {
            app.clone().oneshot(
                Request::post("/api/v1/accounts/acc-b/folders")
                    .header("Authorization", "Bearer tok")
                    .header("content-type", "application/json")
                    .header("Idempotency-Key", "same-key-123")
                    .body(Body::from(folder.clone()))
                    .unwrap(),
            )
        };

        let first = request(&app).await.unwrap();
        assert_eq!(first.status(), StatusCode::NO_CONTENT);
        assert!(first.headers().get("idempotency-replayed").is_none());

        let second = request(&app).await.unwrap();
        assert_eq!(second.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            second
                .headers()
                .get("idempotency-replayed")
                .map(|v| v.to_str().unwrap()),
            Some("true")
        );
    }

    #[tokio::test]
    async fn agent_mutation_quota_is_stricter() {
        // Tight agent quota: a single mutation, then 429 — while the human
        // limiter (default burst 10) would still allow more.
        let dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(TelegramClientManager::with_key_provider(
            dir.path().join("sessions"),
            1,
            "hash".into(),
            Arc::new(vasya_core::telegram::master_key::FileKeyProvider::new(
                dir.path().join("master.key"),
            )),
        ));
        let mut options = ServerOptions::new(
            AuthMode::EmbeddedLocal {
                token: "tok".into(),
            },
            dir.path().join("data"),
        );
        options.agent_rate_limit = rate_limit::RateLimitConfig {
            capacity: 1,
            refill_every: std::time::Duration::from_secs(60),
        };
        let app = build_router(build_context(manager, options).unwrap());
        let (_id, secret) = create_agent_key(&app, &["folders:write"]).await;

        let folder = |id: &str| {
            serde_json::json!({
                "id": id, "account_id": "acc-c", "name": "x", "icon": null,
                "included_chat_types": [], "excluded_chat_types": [],
                "included_chat_ids": [], "excluded_chat_ids": [], "sort_order": 1
            })
            .to_string()
        };

        let send = |app: &axum::Router, body: String| {
            app.clone().oneshot(
                Request::post("/api/v1/accounts/acc-c/folders")
                    .header("Authorization", format!("Bearer {secret}"))
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
        };

        assert_eq!(
            send(&app, folder("f1")).await.unwrap().status(),
            StatusCode::NO_CONTENT
        );
        let res = send(&app, folder("f2")).await.unwrap();
        assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(res.headers().get("retry-after").is_some());
    }

    #[tokio::test]
    async fn media_topic_and_voice_headers_are_validated_before_telegram() {
        let (_dir, app) = test_app("tok");
        for (topic, voice, mime, expected) in [
            ("-1", "false", "image/png", StatusCode::BAD_REQUEST),
            (
                "42",
                "true",
                "application/octet-stream",
                StatusCode::BAD_REQUEST,
            ),
            ("42", "true", "audio/ogg", StatusCode::NOT_FOUND),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::post("/api/v1/accounts/missing/chats/123/media")
                        .header("Authorization", "Bearer tok")
                        .header("x-file-name", "voice.ogg")
                        .header("x-topic-id", topic)
                        .header("x-voice", voice)
                        .header("x-mime-type", mime)
                        .body(Body::from("audio"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
        }
    }

    #[tokio::test]
    async fn credentials_status_reflects_manager() {
        let (_dir, app) = test_app("tok");
        let res = app
            .oneshot(
                Request::get("/api/v1/telegram/credentials")
                    .header("Authorization", "Bearer tok")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(body_json(res).await["configured"], true);
    }
}
