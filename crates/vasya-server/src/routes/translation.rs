//! Human-session authenticated translation. Secrets are encrypted per user.
use crate::{auth::UserId, context::ServerContext, error::ApiError};
use axum::{extract::State, Extension, Json};
use serde::Deserialize;
use std::sync::Arc;
use vasya_core::translation::{
    TranslationResult, TranslationService, TranslationSettings, TranslationSettingsUpdate,
};
fn service(ctx: &ServerContext, user: &UserId) -> TranslationService {
    TranslationService::new(
        ctx.data_dir
            .join("translation")
            .join(&user.0)
            .join("settings.enc"),
        ctx.manager.master_key_provider(),
    )
}
pub async fn get_settings(
    State(ctx): State<Arc<ServerContext>>,
    Extension(user): Extension<UserId>,
) -> Result<Json<TranslationSettings>, ApiError> {
    service(&ctx, &user)
        .settings()
        .await
        .map(Json)
        .map_err(ApiError::internal)
}
pub async fn put_settings(
    State(ctx): State<Arc<ServerContext>>,
    Extension(user): Extension<UserId>,
    Json(settings): Json<TranslationSettingsUpdate>,
) -> Result<Json<TranslationSettings>, ApiError> {
    service(&ctx, &user)
        .update(settings)
        .await
        .map(Json)
        .map_err(|error| ApiError::BadRequest(error.to_string()))
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranslateRequest {
    pub text: String,
    pub target_language: String,
}
pub async fn translate(
    State(ctx): State<Arc<ServerContext>>,
    Extension(user): Extension<UserId>,
    Json(request): Json<TranslateRequest>,
) -> Result<Json<TranslationResult>, ApiError> {
    service(&ctx, &user)
        .translate(&request.text, &request.target_language)
        .await
        .map(Json)
        .map_err(|error| ApiError::BadRequest(error.to_string()))
}
