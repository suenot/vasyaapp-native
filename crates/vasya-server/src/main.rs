//! Standalone vasya-server binary: env-configured wrapper around the library.
//!
//! Environment:
//!   VASYA_BIND           listen address          (default 127.0.0.1:8787)
//!   VASYA_DATA_DIR       state directory         (default ./vasya-data)
//!   TELEGRAM_API_ID      Telegram api_id         (required for logins)
//!   TELEGRAM_API_HASH    Telegram api_hash       (required for logins)
//!   SESSION_MASTER_KEY   64-hex session key      (required; KMS/secret-manager injected)
//!   AUTH_MODE            "jwt" | "embedded"      (default jwt)
//!   JWT_SECRET           HS256 secret            (required in jwt mode; share with backend/)
//!   VASYA_LOCAL_TOKEN    bearer token            (embedded mode; generated+printed if unset)
//!   VASYA_CORS_ORIGIN    allowed browser origin  (optional, e.g. https://vasya.marketmaker.cc)

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use vasya_core::telegram::master_key::EnvKeyProvider;
use vasya_core::TelegramClientManager;
use vasya_server::{build_context, build_router, start_existing_sessions, AuthMode, ServerOptions};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,vasya_server=debug".into()),
        )
        .init();

    let bind = std::env::var("VASYA_BIND").unwrap_or_else(|_| "127.0.0.1:8787".into());
    let data_dir = std::path::PathBuf::from(
        std::env::var("VASYA_DATA_DIR").unwrap_or_else(|_| "./vasya-data".into()),
    );

    let api_id: i32 = std::env::var("TELEGRAM_API_ID")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let api_hash = std::env::var("TELEGRAM_API_HASH").unwrap_or_default();
    if api_id == 0 || api_hash.is_empty() {
        tracing::warn!("TELEGRAM_API_ID / TELEGRAM_API_HASH not set — logins fall back to nothing until an admin sets the global default (PUT /api/v1/admin/telegram/credentials) or each user sets their own (PUT /api/v1/telegram/credentials)");
    }

    let auth = match std::env::var("AUTH_MODE").as_deref() {
        Ok("embedded") => match std::env::var("VASYA_LOCAL_TOKEN") {
            Ok(token) if !token.is_empty() => AuthMode::EmbeddedLocal { token },
            _ => {
                let mode = AuthMode::embedded_with_random_token();
                tracing::info!("Generated ephemeral local API token; configure VASYA_LOCAL_TOKEN for external clients");
                mode
            }
        },
        _ => {
            let secret =
                std::env::var("JWT_SECRET").context("JWT_SECRET is required in jwt auth mode")?;
            if secret.len() < 32 {
                bail!("JWT_SECRET must be at least 32 characters");
            }
            AuthMode::Jwt { secret }
        }
    };

    let sessions_dir = data_dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir).context("failed to create sessions dir")?;

    // Fail fast when the master key is missing/malformed: better at boot
    // than on the first login.
    let key_provider = Arc::new(EnvKeyProvider::default_var());
    {
        use vasya_core::telegram::master_key::MasterKeyProvider;
        key_provider
            .get_or_create()
            .context("SESSION_MASTER_KEY check failed")?;
    }

    let manager = Arc::new(TelegramClientManager::with_key_provider(
        sessions_dir,
        api_id,
        api_hash,
        key_provider,
    ));

    let mut options = ServerOptions::new(auth, data_dir);
    options.graphql_playground =
        std::env::var("VASYA_GRAPHQL_PLAYGROUND").is_ok_and(|v| v == "1" || v == "true");

    // Admins (may set the global Telegram credentials). Embedded mode already
    // defaults to the local owner; in JWT mode admins come ONLY from
    // VASYA_ADMIN_USERS (never settable via the API, so users can't escalate).
    if matches!(options.auth, AuthMode::Jwt { .. }) {
        let admins: Vec<String> = std::env::var("VASYA_ADMIN_USERS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if admins.is_empty() {
            tracing::warn!("VASYA_ADMIN_USERS not set — no user can change the global Telegram credentials over the API; set it to a comma-separated list of admin user ids");
        } else {
            tracing::info!(
                count = admins.len(),
                "Admin users loaded from VASYA_ADMIN_USERS"
            );
        }
        options.admins = vasya_server::auth::AdminPolicy::jwt(admins);
    }

    let ctx = build_context(manager, options)?;

    let loaded = start_existing_sessions(&ctx).await?;
    tracing::info!(count = loaded.len(), "Sessions loaded from disk");

    let mut app = build_router(ctx.clone());
    if let Ok(origin) = std::env::var("VASYA_CORS_ORIGIN") {
        let origin = origin
            .parse::<axum::http::HeaderValue>()
            .context("VASYA_CORS_ORIGIN is not a valid origin")?;
        app = app.layer(
            tower_http::cors::CorsLayer::new()
                .allow_origin(origin)
                .allow_methods(tower_http::cors::Any)
                .allow_headers(tower_http::cors::Any),
        );
    }

    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("failed to bind {bind}"))?;
    tracing::info!(%bind, "vasya-server listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    // Flush the throttled tail of every open session before exit.
    ctx.manager.flush_all_sessions().await;
    tracing::info!("Sessions flushed, bye");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
