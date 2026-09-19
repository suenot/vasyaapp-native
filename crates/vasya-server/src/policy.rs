//! Agent policy middleware: scope enforcement, stricter agent quotas,
//! audit recording and Idempotency-Key replay.
//!
//! Layer order (outer → inner): require_auth → audit → agent_policy →
//! idempotency → handler. Audit therefore also records scope rejections
//! and replayed responses; idempotency caches only what handlers produced.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{OriginalUri, Request, State};
use axum::http::{header, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::agent_keys::AgentIdentity;
use crate::audit::AuditEntry;
use crate::auth::UserId;
use crate::context::ServerContext;
use crate::error::ApiError;
use crate::idempotency::{Begin, StoredResponse};

fn is_mutation(method: &Method) -> bool {
    matches!(*method, Method::POST | Method::PUT | Method::DELETE)
}

/// Map an endpoint to the agent scope it requires. Human sessions skip
/// this entirely (they hold all scopes implicitly).
///
/// `pub(crate)` so the GraphQL layer can derive each resolver's scope from
/// the very same map (see `graphql.rs`), keeping the two transports from
/// drifting apart.
pub(crate) fn required_scope(method: &Method, segments: &[&str]) -> Option<&'static str> {
    let get = *method == Method::GET;
    match segments {
        ["events"] => Some("events:read"),
        ["telegram", ..] => Some("telegram:login"),
        ["accounts"] => Some("accounts:read"),
        // logout/delete the account — its own high-blast-radius scope,
        // no longer bundled with login.
        ["accounts", _] => Some("accounts:delete"), // DELETE
        ["accounts", _, "avatar"] => Some("accounts:read"),
        ["accounts", _, "chats", "load"] => Some("chats:read"),
        ["accounts", _, "chats"] => Some("chats:read"),
        ["accounts", _, "chats", _] => Some("chats:delete"), // DELETE
        ["accounts", _, "groups"] | ["accounts", _, "channels"] => Some("chats:write"),
        ["accounts", _, "contacts"] => Some("chats:read"),
        ["accounts", _, "chats", _, "photo"] => Some("chats:read"),
        ["accounts", _, "chats", _, "photos", ..] => Some("chats:read"),
        ["accounts", _, "chats", _, "messages", "search"] => Some("messages:read"),
        ["accounts", _, "chats", _, "messages"] => Some(if get {
            "messages:read"
        } else {
            "messages:send"
        }),
        ["accounts", _, "chats", _, "messages", _, "media"] => Some("messages:read"),
        ["accounts", _, "chats", _, "media"] => Some("messages:send"),
        ["accounts", _, "messages", "forward"] => Some("messages:forward"),
        ["accounts", _, "chats", _, "read"] => Some("messages:send"),
        ["accounts", _, "search"] => Some("chats:read"),
        ["accounts", _, "messages", "search"] => Some("messages:read"),
        ["accounts", _, "chats", _, "topics"] => Some("chats:read"),
        ["accounts", _, "folders", ..] | ["accounts", _, "tabs"] => {
            Some(if get { "folders:read" } else { "folders:write" })
        }
        ["accounts", _, "calls", ..] | ["accounts", _, "group-calls", ..] => Some("calls:use"),
        ["stt", ..] => Some("stt:use"),
        ["storage-mode"] => Some("accounts:read"),
        _ => None,
    }
}

fn path_segments(path: &str) -> Vec<&str> {
    path.strip_prefix("/api/v1")
        .unwrap_or(path)
        .trim_start_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect()
}

/// Scope + quota enforcement for agent-key callers; human sessions pass
/// through untouched.
pub async fn agent_policy(
    State(ctx): State<Arc<ServerContext>>,
    req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let Some(agent) = req.extensions().get::<AgentIdentity>().cloned() else {
        return Ok(next.run(req).await);
    };

    let path = req.uri().path().to_string();
    let segments = path_segments(&path);

    match segments.first() {
        // GraphQL bundles every operation behind one `/graphql` path, so the
        // path-based scope gate below can't tell which fields a query touches.
        // Scope + per-account enforcement therefore moves into the resolver
        // layer (`graphql.rs::authorize`), mirroring this same scope map. The
        // agent identity is already in the request extensions; let it through.
        Some(&"graphql") => return Ok(next.run(req).await),
        Some(&"agent-keys") | Some(&"audit") => {
            return Err(ApiError::Forbidden(
                "Agent keys cannot manage keys or read the audit log".into(),
            ))
        }
        // Admin routes (e.g. the global Telegram credentials) are human-only —
        // an agent key whose owner is an admin must NOT inherit admin rights.
        Some(&"admin") => {
            return Err(ApiError::Forbidden(
                "Agent keys cannot access admin routes".into(),
            ))
        }
        _ => {}
    }

    // Credential management (per-user api_id/api_hash) is human-session-only;
    // agents log in with the resolved credentials but cannot change them.
    // (Note: /telegram/login/* stays agent-allowed via the scope map below.)
    if segments.as_slice() == ["telegram", "credentials"] {
        return Err(ApiError::Forbidden(
            "Agent keys cannot manage Telegram credentials".into(),
        ));
    }

    let scope = required_scope(req.method(), &segments)
        .ok_or_else(|| ApiError::Forbidden("No agent scope covers this endpoint".into()))?;
    if !agent.has_scope(scope) {
        return Err(ApiError::Forbidden(format!("Missing scope: {scope}")));
    }

    // Per-account allowlist: for any /accounts/{acc}/… target, the key must
    // be allowed to reach {acc}. Keys without an allowlist reach all of the
    // owner's accounts (unchanged behavior).
    if let ["accounts", acc, ..] = segments.as_slice() {
        if !agent.allows_account(acc) {
            return Err(ApiError::Forbidden("account not in key allowlist".into()));
        }
    }

    // Stricter quota for agent mutations (plan §12), per key, on top of
    // the per-account limiter inside the ops.
    if is_mutation(req.method()) {
        ctx.agent_rate.check_mutation(&agent.key_id)?;
    }

    Ok(next.run(req).await)
}

/// Records every mutating call: caller, agent key (if any), method, full
/// path (carries the account/chat target) and response status.
pub async fn audit_mutations(
    State(ctx): State<Arc<ServerContext>>,
    req: Request,
    next: Next,
) -> Response {
    if !is_mutation(req.method()) {
        return next.run(req).await;
    }

    let method = req.method().to_string();
    let user = req.extensions().get::<UserId>().cloned();
    let agent = req.extensions().get::<AgentIdentity>().cloned();
    let path = req
        .extensions()
        .get::<OriginalUri>()
        .map(|u| u.0.path().to_string())
        .unwrap_or_else(|| req.uri().path().to_string());

    let response = next.run(req).await;

    if let Some(user) = user {
        ctx.audit.record(&AuditEntry {
            ts: chrono::Utc::now().timestamp_millis(),
            user_id: user.0,
            agent_key_id: agent.map(|a| a.key_id),
            method,
            path,
            status: response.status().as_u16(),
        });
    }
    response
}

/// Max response body size the idempotency cache will buffer.
const IDEMPOTENCY_BODY_LIMIT: usize = 16 * 1024 * 1024;

/// Replays mutating responses for repeated Idempotency-Key headers.
pub async fn idempotency(
    State(ctx): State<Arc<ServerContext>>,
    req: Request,
    next: Next,
) -> Response {
    if !is_mutation(req.method()) {
        return next.run(req).await;
    }
    let Some(idem_key) = req
        .headers()
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
    else {
        return next.run(req).await;
    };

    let user = req
        .extensions()
        .get::<UserId>()
        .map(|u| u.0.clone())
        .unwrap_or_default();
    let key = format!("{user}|{}|{}|{idem_key}", req.method(), req.uri().path());

    match ctx.idempotency.begin(&key) {
        Begin::Replay(stored) => {
            let mut response = Response::builder().status(stored.status);
            if let Some(ct) = &stored.content_type {
                response = response.header(header::CONTENT_TYPE, ct);
            }
            response
                .header("idempotency-replayed", "true")
                .body(Body::from(stored.body))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        Begin::InFlight => (
            StatusCode::CONFLICT,
            axum::Json(serde_json::json!({
                "error": "A request with this Idempotency-Key is already in flight"
            })),
        )
            .into_response(),
        Begin::Execute => {
            let response = next.run(req).await;
            if response.status().is_server_error() {
                ctx.idempotency.abandon(&key);
                return response;
            }

            let (parts, body) = response.into_parts();
            let bytes = match axum::body::to_bytes(body, IDEMPOTENCY_BODY_LIMIT).await {
                Ok(bytes) => bytes,
                Err(e) => {
                    // Response too large or stream error: don't cache it.
                    ctx.idempotency.abandon(&key);
                    tracing::warn!(error = %e, "Idempotency cache skipped (body not bufferable)");
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
            };

            let content_type = parts
                .headers
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            ctx.idempotency.complete(
                &key,
                StoredResponse {
                    status: parts.status.as_u16(),
                    content_type,
                    body: bytes.to_vec(),
                },
            );
            Response::from_parts(parts, Body::from(bytes))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_map_covers_the_surface() {
        let cases: &[(&str, &str, Option<&str>)] = &[
            ("GET", "/api/v1/accounts", Some("accounts:read")),
            ("DELETE", "/api/v1/accounts/a1", Some("accounts:delete")),
            (
                "POST",
                "/api/v1/telegram/login/code",
                Some("telegram:login"),
            ),
            ("GET", "/api/v1/accounts/a1/chats", Some("chats:read")),
            ("POST", "/api/v1/accounts/a1/chats/load", Some("chats:read")),
            (
                "DELETE",
                "/api/v1/accounts/a1/chats/5",
                Some("chats:delete"),
            ),
            ("POST", "/api/v1/accounts/a1/groups", Some("chats:write")),
            (
                "GET",
                "/api/v1/accounts/a1/chats/5/messages",
                Some("messages:read"),
            ),
            (
                "POST",
                "/api/v1/accounts/a1/chats/5/messages",
                Some("messages:send"),
            ),
            (
                "POST",
                "/api/v1/accounts/a1/chats/5/media",
                Some("messages:send"),
            ),
            (
                "POST",
                "/api/v1/accounts/a1/messages/forward",
                Some("messages:forward"),
            ),
            (
                "POST",
                "/api/v1/accounts/a1/chats/5/read",
                Some("messages:send"),
            ),
            (
                "GET",
                "/api/v1/accounts/a1/chats/5/messages/9/media",
                Some("messages:read"),
            ),
            (
                "GET",
                "/api/v1/accounts/a1/chats/5/messages/search",
                Some("messages:read"),
            ),
            ("GET", "/api/v1/accounts/a1/search", Some("chats:read")),
            (
                "GET",
                "/api/v1/accounts/a1/messages/search",
                Some("messages:read"),
            ),
            (
                "GET",
                "/api/v1/accounts/a1/chats/5/topics",
                Some("chats:read"),
            ),
            ("GET", "/api/v1/accounts/a1/folders", Some("folders:read")),
            ("POST", "/api/v1/accounts/a1/folders", Some("folders:write")),
            ("PUT", "/api/v1/accounts/a1/tabs", Some("folders:write")),
            ("GET", "/api/v1/events", Some("events:read")),
            (
                "POST",
                "/api/v1/accounts/a1/calls/request",
                Some("calls:use"),
            ),
            ("GET", "/api/v1/stt/models", Some("stt:use")),
            ("GET", "/api/v1/nope", None),
        ];
        for (method, path, expected) in cases {
            let method: Method = method.parse().unwrap();
            let segments = path_segments(path);
            assert_eq!(
                required_scope(&method, &segments),
                *expected,
                "{method} {path}"
            );
        }
    }
}
