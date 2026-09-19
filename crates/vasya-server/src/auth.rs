//! Authentication: two modes behind one middleware.
//!
//! * `EmbeddedLocal` — the desktop app mounts the router in-process on
//!   127.0.0.1 with a single auto-generated bearer token; one implicit
//!   user owns everything. No Postgres, no JWT.
//! * `Jwt` — standalone server: HS256 user JWTs with the same `Claims`
//!   shape the existing sync backend issues (`sub` = user id), so tokens
//!   from `backend/`'s login endpoint work as-is when both share
//!   `JWT_SECRET`. Issuing tokens (email/password login) stays in the
//!   sync backend; this server only validates.
//!
//! Agent API keys (scoped, Postgres-backed) are task #7 and will slot in
//! as a third arm of `AuthMode`.

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use jsonwebtoken::{decode, DecodingKey, Validation};
use serde::{Deserialize, Serialize};

use crate::context::ServerContext;
use crate::error::ApiError;

/// JWT claims — must match the sync backend's token shape.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Claims {
    pub sub: String, // user_id as string
    pub exp: usize,
    pub iat: usize,
}

/// The user id under which all accounts are owned in embedded-local mode.
pub const LOCAL_USER_ID: &str = "local";

#[derive(Clone)]
pub enum AuthMode {
    /// Single-user in-process mode: one shared bearer token.
    EmbeddedLocal { token: String },
    /// Multi-user standalone mode: validate HS256 user JWTs.
    Jwt { secret: String },
}

impl AuthMode {
    /// Generate an embedded-local mode with a random 32-byte hex token.
    pub fn embedded_with_random_token() -> Self {
        use rand::RngCore;
        let mut bytes = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        Self::EmbeddedLocal { token }
    }

    /// The bearer token in embedded mode (so the host app can show it to
    /// local agents); None in JWT mode.
    pub fn embedded_token(&self) -> Option<&str> {
        match self {
            Self::EmbeddedLocal { token } => Some(token),
            Self::Jwt { .. } => None,
        }
    }

    /// Resolve a bearer token to a user id.
    pub fn authenticate(&self, bearer: &str) -> Result<UserId, ApiError> {
        match self {
            Self::EmbeddedLocal { token } => {
                if constant_time_eq(bearer.as_bytes(), token.as_bytes()) {
                    Ok(UserId(LOCAL_USER_ID.to_string()))
                } else {
                    Err(ApiError::Unauthorized)
                }
            }
            Self::Jwt { secret } => {
                let data = decode::<Claims>(
                    bearer,
                    &DecodingKey::from_secret(secret.as_bytes()),
                    &Validation::default(),
                )
                .map_err(|_| ApiError::Unauthorized)?;
                Ok(UserId(data.claims.sub))
            }
        }
    }
}

/// Authenticated caller identity, inserted into request extensions.
#[derive(Debug, Clone)]
pub struct UserId(pub String);

/// Who counts as a server admin (may manage the global Telegram credentials and
/// other server-wide settings). Sourced ONLY from server configuration
/// (embedded owner, or the `VASYA_ADMIN_USERS` env list) — never settable via
/// the API, so a regular user cannot escalate to admin. Agent keys are never
/// admins regardless of their owner (admin routes are human-session-only,
/// enforced in `policy.rs`).
#[derive(Clone, Default)]
pub struct AdminPolicy {
    /// Explicit admin user ids (JWT mode).
    user_ids: std::collections::HashSet<String>,
    /// In embedded-local mode the single owner (`local`) is the admin.
    embedded_local_is_admin: bool,
}

impl AdminPolicy {
    /// JWT mode: admins are the configured user ids (e.g. `VASYA_ADMIN_USERS`).
    pub fn jwt<I: IntoIterator<Item = String>>(user_ids: I) -> Self {
        Self {
            user_ids: user_ids.into_iter().collect(),
            embedded_local_is_admin: false,
        }
    }

    /// Embedded-local desktop mode: the single `local` owner is the admin.
    pub fn embedded_local() -> Self {
        Self {
            user_ids: std::collections::HashSet::new(),
            embedded_local_is_admin: true,
        }
    }

    /// Whether this user id is an admin. (Callers must also ensure the request
    /// is a human session, not an agent key — see `policy.rs`.)
    pub fn is_admin(&self, user_id: &str) -> bool {
        (self.embedded_local_is_admin && user_id == LOCAL_USER_ID)
            || self.user_ids.contains(user_id)
    }
}

/// Whether a resolved user id is safe to use as an on-disk path segment.
///
/// The user id (JWT `sub`, or the embedded `local`) is used verbatim as a
/// directory name for per-user state — STT settings (`data_dir/stt/{user}/…`)
/// and folder/tab UI-state (`data_dir/ui-state/{user}/…`). A `sub` containing
/// `/`, `..`, or an absolute path would escape `data_dir`. The signing secret
/// is shared with the backend, so we don't trust `sub` to be a UUID — we
/// enforce it here, at the one auth choke point, before any handler runs.
/// UUIDs and the literal `local` id pass.
pub(crate) fn is_safe_user_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Constant-time comparison to prevent timing attacks.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut result = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        result |= x ^ y;
    }
    result == 0
}

/// Name of the HttpOnly cookie carrying the human session token (JWT/local) for
/// the browser web build. Set by the issuer (backend); accepted here when no
/// `Authorization` header is present.
pub const SESSION_COOKIE: &str = "vasya_token";
/// Double-submit CSRF token: a JS-readable cookie whose value the client echoes
/// in the [`CSRF_HEADER`] on every state-changing request.
pub const CSRF_COOKIE: &str = "vasya_csrf";
/// Request header carrying the CSRF token for double-submit validation.
pub const CSRF_HEADER: &str = "x-csrf-token";

fn is_mutating(method: &axum::http::Method) -> bool {
    use axum::http::Method;
    matches!(
        *method,
        Method::POST | Method::PUT | Method::DELETE | Method::PATCH
    )
}

/// Extract a single cookie value from the `Cookie` request header.
fn cookie_value(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(axum::http::header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| k.trim() == name)
        .map(|(_, v)| v.trim().to_string())
}

/// Bearer-auth middleware applied to every /api/v1 route except /health
/// and /openapi.json. Accepts either a human session token (JWT / local
/// token, all scopes implicit) or an agent key (`vk_...`, scoped — the
/// agent acts on behalf of its owning user, policy middleware enforces
/// scopes and quotas).
pub async fn require_auth(
    State(ctx): State<std::sync::Arc<ServerContext>>,
    mut req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    // 1. Authorization: Bearer — human token or agent key. NOT CSRF-vulnerable
    //    (a browser never auto-attaches an Authorization header cross-site).
    if let Some(bearer) = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|s| s.to_string())
    {
        if bearer.starts_with("vk_") {
            let (user_id, identity) = ctx
                .agent_keys
                .authenticate(&bearer)
                .ok_or(ApiError::Unauthorized)?;
            // The owner id becomes an on-disk path segment in some handlers.
            if !is_safe_user_id(&user_id) {
                return Err(ApiError::Unauthorized);
            }
            req.extensions_mut().insert(UserId(user_id));
            req.extensions_mut().insert(identity);
        } else {
            let user = ctx.auth.authenticate(&bearer)?;
            // Reject a JWT `sub` that could traverse per-user state paths.
            if !is_safe_user_id(&user.0) {
                return Err(ApiError::Unauthorized);
            }
            req.extensions_mut().insert(user);
        }
        return Ok(next.run(req).await);
    }

    // 2. HttpOnly session cookie (browser web build). Cookies ARE auto-sent
    //    cross-site, so enforce CSRF on state-changing requests via a
    //    double-submit token. Agent keys never authenticate via cookie.
    if let Some(token) = cookie_value(req.headers(), SESSION_COOKIE) {
        if is_mutating(req.method()) {
            let header_csrf = req.headers().get(CSRF_HEADER).and_then(|v| v.to_str().ok());
            let cookie_csrf = cookie_value(req.headers(), CSRF_COOKIE);
            let ok = matches!(
                (header_csrf, cookie_csrf.as_deref()),
                (Some(h), Some(c)) if !c.is_empty() && constant_time_eq(h.as_bytes(), c.as_bytes())
            );
            if !ok {
                return Err(ApiError::Forbidden("Missing or invalid CSRF token".into()));
            }
        }
        let user = ctx.auth.authenticate(&token)?;
        if !is_safe_user_id(&user.0) {
            return Err(ApiError::Unauthorized);
        }
        req.extensions_mut().insert(user);
        return Ok(next.run(req).await);
    }

    Err(ApiError::Unauthorized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_basics() {
        assert!(constant_time_eq(b"hello", b"hello"));
        assert!(!constant_time_eq(b"hello", b"world"));
        assert!(!constant_time_eq(b"hello", b"hell"));
        assert!(constant_time_eq(b"", b""));
        assert!(!constant_time_eq(b"", b"a"));
    }

    #[test]
    fn admin_policy_sources_are_config_only() {
        // Embedded-local: only the `local` owner is admin.
        let p = AdminPolicy::embedded_local();
        assert!(p.is_admin(LOCAL_USER_ID));
        assert!(!p.is_admin("someone-else"));

        // JWT: only explicitly-listed user ids are admins.
        let p = AdminPolicy::jwt(["admin-1".to_string(), "admin-2".to_string()]);
        assert!(p.is_admin("admin-1"));
        assert!(p.is_admin("admin-2"));
        assert!(!p.is_admin("regular-user"));
        assert!(!p.is_admin(LOCAL_USER_ID)); // embedded shortcut does not apply in JWT mode

        // Default (no config) → nobody is admin.
        assert!(!AdminPolicy::default().is_admin("anyone"));
        assert!(!AdminPolicy::default().is_admin(LOCAL_USER_ID));
    }

    #[test]
    fn safe_user_id_accepts_uuid_and_local_rejects_traversal() {
        // Legitimate ids pass.
        assert!(is_safe_user_id("local"));
        assert!(is_safe_user_id("42f00000-0000-0000-0000-000000000042"));
        assert!(is_safe_user_id("user_123"));
        // Path-traversal / escape attempts are rejected.
        assert!(!is_safe_user_id("../../etc/cron.d/x"));
        assert!(!is_safe_user_id("/etc/passwd"));
        assert!(!is_safe_user_id("a/b"));
        assert!(!is_safe_user_id(".."));
        assert!(!is_safe_user_id("a.b")); // '.' not allowed → no dotfiles either
        assert!(!is_safe_user_id(""));
        assert!(!is_safe_user_id(&"x".repeat(129)));
    }

    #[test]
    fn embedded_mode_authenticates_exact_token_only() {
        let mode = AuthMode::EmbeddedLocal {
            token: "secret-token".into(),
        };
        assert_eq!(mode.authenticate("secret-token").unwrap().0, LOCAL_USER_ID);
        assert!(mode.authenticate("wrong").is_err());
        assert!(mode.authenticate("").is_err());
    }

    #[test]
    fn random_embedded_token_is_64_hex_chars() {
        let mode = AuthMode::embedded_with_random_token();
        let token = mode.embedded_token().unwrap();
        assert_eq!(token.len(), 64);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn jwt_mode_roundtrip_and_rejection() {
        let secret = "test-secret";
        let now = chrono::Utc::now().timestamp() as usize;
        let claims = Claims {
            sub: "42f00000-0000-0000-0000-000000000042".into(),
            iat: now,
            exp: now + 3600,
        };
        let token = jsonwebtoken::encode(
            &jsonwebtoken::Header::default(),
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(secret.as_bytes()),
        )
        .unwrap();

        let mode = AuthMode::Jwt {
            secret: secret.into(),
        };
        assert_eq!(mode.authenticate(&token).unwrap().0, claims.sub);
        assert!(mode.authenticate("garbage").is_err());

        let wrong = AuthMode::Jwt {
            secret: "other".into(),
        };
        assert!(wrong.authenticate(&token).is_err());
    }

    #[test]
    fn jwt_mode_rejects_expired_token() {
        let secret = "test-secret";
        let now = chrono::Utc::now().timestamp() as usize;
        let claims = Claims {
            sub: "u".into(),
            iat: now - 7200,
            exp: now - 3600,
        };
        let token = jsonwebtoken::encode(
            &jsonwebtoken::Header::default(),
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(secret.as_bytes()),
        )
        .unwrap();
        let mode = AuthMode::Jwt {
            secret: secret.into(),
        };
        assert!(mode.authenticate(&token).is_err());
    }
}
