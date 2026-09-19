//! Server-side Telegram login flow (parity with commands/auth.rs and
//! commands/settings.rs credential commands).

use std::sync::Arc;

use axum::extract::State;
use axum::{Extension, Json};
use grammers_client::SignInError;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::UserId;
use crate::context::ServerContext;
use crate::dto::UserInfo;
use crate::error::ApiError;

/// Mask a phone number for logging — keep only the last 4 digits.
fn mask_phone(phone: &str) -> String {
    let digits: Vec<char> = phone.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() <= 4 {
        return "***".to_string();
    }
    let last4: String = digits[digits.len() - 4..].iter().collect();
    format!("***{}", last4)
}

// Credential management lives in `routes::telegram_creds` (per-user + admin).

#[derive(Deserialize)]
pub struct LoginCodeRequest {
    pub phone: String,
}

#[derive(Serialize, async_graphql::SimpleObject)]
#[serde(rename_all = "camelCase")]
pub struct LoginCodeResponse {
    pub account_id: String,
    pub phone: String,
}

pub(crate) async fn request_login_code_op(
    ctx: &ServerContext,
    user: &UserId,
    phone: String,
) -> Result<LoginCodeResponse, ApiError> {
    // Use the caller's own Telegram credentials if they set any, else the
    // server's global default (see routes::telegram_creds).
    let (api_id, api_hash) = crate::routes::telegram_creds::resolve_credentials(ctx, &user.0)
        .await?
        .ok_or_else(|| ApiError::BadRequest("Telegram API credentials not configured".into()))?;

    tracing::info!(phone = %mask_phone(&phone), api_id, "Requesting login code");

    ctx.rate.check_mutation(&phone)?;

    let account_id = Uuid::new_v4().to_string();
    // The creator owns the account from the start.
    ctx.accounts.ensure_access(&user.0, &account_id)?;

    let wrapper = ctx
        .manager
        .create_client_with_api_id(account_id.clone(), phone.clone(), api_id)
        .await
        .map_err(|e| ApiError::internal(format!("Failed to create client: {e}")))?;

    let token = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        wrapper.client.request_login_code(&phone, &api_hash),
    )
    .await
    .map_err(|_| ApiError::Telegram("Request timed out".into()))?
    .map_err(|e| ApiError::telegram(format!("Failed to request login code: {e}")))?;

    ctx.pending_logins
        .lock()
        .await
        .insert(account_id.clone(), token);

    tracing::info!(account_id = %account_id, "Login code requested");
    Ok(LoginCodeResponse { account_id, phone })
}

pub async fn request_login_code(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
    Json(req): Json<LoginCodeRequest>,
) -> Result<Json<LoginCodeResponse>, ApiError> {
    Ok(Json(request_login_code_op(&ctx, &user.0, req.phone).await?))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyCodeRequest {
    pub account_id: String,
    pub code: String,
}

/// Transport-neutral login step result; REST and GraphQL map it to their
/// own response shapes.
pub(crate) enum LoginResult {
    Authorized(UserInfo),
    PasswordRequired,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase", untagged)]
pub enum LoginOutcome {
    Authorized {
        status: &'static str,
        user: UserInfo,
    },
    PasswordRequired {
        status: &'static str,
    },
}

impl From<LoginResult> for LoginOutcome {
    fn from(result: LoginResult) -> Self {
        match result {
            LoginResult::Authorized(user) => Self::Authorized {
                status: "authorized",
                user,
            },
            LoginResult::PasswordRequired => Self::PasswordRequired {
                status: "password_required",
            },
        }
    }
}

pub(crate) async fn verify_code_op(
    ctx: &ServerContext,
    user: &UserId,
    account_id: String,
    code: String,
) -> Result<LoginResult, ApiError> {
    tracing::info!(account_id = %account_id, "Verifying code");

    ctx.accounts.ensure_access(&user.0, &account_id)?;

    let login_token = ctx
        .pending_logins
        .lock()
        .await
        .remove(&account_id)
        .ok_or_else(|| ApiError::BadRequest("Login session expired or invalid".into()))?;

    let wrapper = ctx
        .manager
        .get_client(&account_id)
        .await
        .ok_or_else(|| ApiError::NotFound("Client not found".into()))?;

    match wrapper.client.sign_in(&login_token, &code).await {
        Ok(_user) => {
            finish_login(ctx, &account_id).await?;
            let user_info = current_user_info(&wrapper).await?;
            tracing::info!(account_id = %account_id, "User signed in successfully");
            Ok(LoginResult::Authorized(user_info))
        }
        Err(SignInError::PasswordRequired(password_token)) => {
            ctx.pending_passwords
                .lock()
                .await
                .insert(account_id.clone(), password_token);
            Ok(LoginResult::PasswordRequired)
        }
        Err(e) => Err(ApiError::telegram(format!("Sign in failed: {e}"))),
    }
}

pub async fn verify_code(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
    Json(req): Json<VerifyCodeRequest>,
) -> Result<Json<LoginOutcome>, ApiError> {
    let result = verify_code_op(&ctx, &user.0, req.account_id, req.code).await?;
    Ok(Json(result.into()))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckPasswordRequest {
    pub account_id: String,
    pub password: String,
}

pub(crate) async fn check_password_op(
    ctx: &ServerContext,
    user: &UserId,
    account_id: String,
    password: String,
) -> Result<UserInfo, ApiError> {
    tracing::info!(account_id = %account_id, "Checking 2FA password");

    ctx.accounts.ensure_access(&user.0, &account_id)?;

    let password_token = ctx
        .pending_passwords
        .lock()
        .await
        .remove(&account_id)
        .ok_or_else(|| ApiError::BadRequest("2FA session expired or invalid".into()))?;

    let wrapper = ctx
        .manager
        .get_client(&account_id)
        .await
        .ok_or_else(|| ApiError::NotFound("Client not found".into()))?;

    wrapper
        .client
        .check_password(password_token, password.as_bytes())
        .await
        .map_err(|e| ApiError::telegram(format!("Password check failed: {e}")))?;

    finish_login(ctx, &account_id).await?;
    let user_info = current_user_info(&wrapper).await?;
    tracing::info!(account_id = %account_id, "User signed in with 2FA");
    Ok(user_info)
}

pub async fn check_password(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
    Json(req): Json<CheckPasswordRequest>,
) -> Result<Json<LoginOutcome>, ApiError> {
    let user_info = check_password_op(&ctx, &user.0, req.account_id, req.password).await?;
    Ok(Json(LoginOutcome::Authorized {
        status: "authorized",
        user: user_info,
    }))
}

/// Persist the session and start the update pump (events → bus).
async fn finish_login(ctx: &ServerContext, account_id: &str) -> Result<(), ApiError> {
    ctx.manager
        .save_session(account_id)
        .await
        .map_err(|e| ApiError::internal(format!("Failed to save session: {e}")))?;

    if let Err(e) = ctx
        .manager
        .start_updates(account_id, ctx.updates_context())
        .await
    {
        tracing::error!(error = %e, "Failed to start updates handler");
    }
    Ok(())
}

async fn current_user_info(
    wrapper: &vasya_core::telegram::client_manager::TelegramClientWrapper,
) -> Result<UserInfo, ApiError> {
    let me = wrapper
        .client
        .get_me()
        .await
        .map_err(|e| ApiError::telegram(format!("Failed to get user info: {e}")))?;
    Ok(UserInfo {
        id: me.raw.id(),
        first_name: me.first_name().unwrap_or("").to_string(),
        last_name: me.last_name().map(|s| s.to_string()),
        username: me.username().map(|s| s.to_string()),
        phone: wrapper.phone.clone(),
    })
}
