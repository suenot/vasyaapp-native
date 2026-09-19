//! Speech-to-text over REST (parity with `commands/stt.rs`).
//!
//! Provider policy on a headless server (issue #4): the default is **cloud
//! Deepgram, bring-your-own-key**. Each user stores their own Deepgram API key
//! (write-only — GET masks it). Local **Whisper** depends on the `stt-sidecar`
//! binary which is excluded from the server image, so selecting it here returns
//! a clear error and the model endpoints report it as unavailable rather than
//! 501-ing the whole surface.
//!
//! The Deepgram call + audio sniffing are shared with the desktop app via
//! [`vasya_core::stt`]; this module adds per-user settings persistence, the
//! two transcription input shapes (raw upload / voice-message reference) and
//! the model-status responses.

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use vasya_core::telegram::client_manager::TelegramClientWrapper;

use crate::agent_keys::AgentIdentity;
use crate::auth::UserId;
use crate::context::ServerContext;
use crate::dto::{SttSettingsResponse, SttSettingsUpdate, TranscriptionResponse};
use crate::error::ApiError;
use crate::flood::with_flood_wait_retry;
use crate::peer::resolve_peer;
use crate::routes::account_client;

/// Max raw audio upload accepted by `/stt/transcribe`.
pub(crate) const STT_BODY_LIMIT: usize = 32 * 1024 * 1024;

/// STT provider selection (snake_case matches `commands/stt.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SttProvider {
    Deepgram,
    LocalWhisper,
}

impl SttProvider {
    fn as_str(self) -> &'static str {
        match self {
            SttProvider::Deepgram => "deepgram",
            SttProvider::LocalWhisper => "local_whisper",
        }
    }

    fn parse(s: &str) -> Result<Self, ApiError> {
        match s {
            "deepgram" => Ok(SttProvider::Deepgram),
            "local_whisper" => Ok(SttProvider::LocalWhisper),
            other => Err(ApiError::BadRequest(format!(
                "Unknown STT provider '{other}'. Use 'deepgram' or 'local_whisper'."
            ))),
        }
    }
}

/// Per-user STT settings persisted on disk. The Deepgram key lives here at
/// rest and is never returned in API responses (only masked).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredSttSettings {
    provider: SttProvider,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    deepgram_api_key: Option<String>,
    whisper_model: String,
    language: String,
}

impl Default for StoredSttSettings {
    fn default() -> Self {
        Self {
            // Headless-server default: cloud Deepgram (bring-your-own-key).
            // Local Whisper needs the desktop-only sidecar.
            provider: SttProvider::Deepgram,
            deepgram_api_key: None,
            whisper_model: "small".into(),
            language: "ru".into(),
        }
    }
}

/// Mask a secret to a short non-reversible preview (last 4 chars).
fn mask_key(key: &str) -> String {
    let visible: String = key
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("••••{visible}")
}

impl StoredSttSettings {
    fn to_response(&self) -> SttSettingsResponse {
        let key = self.deepgram_api_key.as_deref().filter(|k| !k.is_empty());
        SttSettingsResponse {
            provider: self.provider.as_str().to_string(),
            deepgram_api_key_set: key.is_some(),
            deepgram_api_key_masked: key.map(mask_key),
            whisper_model: self.whisper_model.clone(),
            language: self.language.clone(),
        }
    }
}

fn settings_path(ctx: &ServerContext, user: &str) -> PathBuf {
    // user ids are uuids or "local" (same trust model as folders' ui-state).
    ctx.data_dir.join("stt").join(user).join("settings.json")
}

async fn load_settings(ctx: &ServerContext, user: &str) -> Result<StoredSttSettings, ApiError> {
    let path = settings_path(ctx, user);
    match tokio::fs::read(&path).await {
        Ok(raw) => serde_json::from_slice(&raw).map_err(ApiError::internal),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(StoredSttSettings::default()),
        Err(e) => Err(ApiError::internal(e)),
    }
}

async fn save_settings(
    ctx: &ServerContext,
    user: &str,
    settings: &StoredSttSettings,
) -> Result<(), ApiError> {
    let path = settings_path(ctx, user);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(ApiError::internal)?;
    }
    let raw = serde_json::to_vec_pretty(settings).map_err(ApiError::internal)?;
    let temporary = path.with_extension("json.tmp");
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    use tokio::io::AsyncWriteExt;
    let mut file = options.open(&temporary).await.map_err(ApiError::internal)?;
    file.write_all(&raw).await.map_err(ApiError::internal)?;
    file.sync_all().await.map_err(ApiError::internal)?;
    tokio::fs::rename(temporary, path)
        .await
        .map_err(ApiError::internal)
}

// --- Settings -----------------------------------------------------------------

pub(crate) async fn get_settings_op(
    ctx: &ServerContext,
    user: &UserId,
) -> Result<SttSettingsResponse, ApiError> {
    Ok(load_settings(ctx, &user.0).await?.to_response())
}

pub async fn get_settings(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
) -> Result<Json<SttSettingsResponse>, ApiError> {
    Ok(Json(get_settings_op(&ctx, &user.0).await?))
}

pub(crate) async fn update_settings_op(
    ctx: &ServerContext,
    user: &UserId,
    update: SttSettingsUpdate,
) -> Result<SttSettingsResponse, ApiError> {
    let mut settings = load_settings(ctx, &user.0).await?;

    if let Some(provider) = update.provider.as_deref() {
        settings.provider = SttProvider::parse(provider)?;
    }
    if let Some(language) = update.language {
        validate_language(&language)?;
        settings.language = language;
    }
    if let Some(model) = update.whisper_model {
        settings.whisper_model = model;
    }
    // Write-only: an empty string clears the key; absent leaves it unchanged.
    if let Some(key) = update.deepgram_api_key {
        settings.deepgram_api_key = if key.is_empty() { None } else { Some(key) };
    }

    save_settings(ctx, &user.0, &settings).await?;
    tracing::info!(user = %user.0, provider = ?settings.provider, "STT settings updated");
    Ok(settings.to_response())
}

pub async fn update_settings(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
    Json(update): Json<SttSettingsUpdate>,
) -> Result<Json<SttSettingsResponse>, ApiError> {
    Ok(Json(update_settings_op(&ctx, &user.0, update).await?))
}

// --- Transcription ------------------------------------------------------------

/// JSON body for transcribing an existing Telegram voice message.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscribeRef {
    pub account_id: String,
    pub chat_id: i64,
    pub message_id: i32,
    /// Optional language override (defaults to the user's STT settings).
    #[serde(default)]
    pub language: Option<String>,
}

/// Run the configured provider over the given audio bytes. Local Whisper is
/// desktop-only on the server, so it returns a clear error here.
/// Validate a transcription language tag before it is interpolated into the
/// Deepgram request URL (`?language=…`). BCP-47-ish: ASCII letters/digits and
/// hyphen, length 1..=16. Blocks query-parameter injection via the per-request
/// override or the persisted setting.
fn validate_language(lang: &str) -> Result<(), ApiError> {
    let ok = (1..=16).contains(&lang.len())
        && lang.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(ApiError::BadRequest(
            "Invalid language code (expected BCP-47-ish, e.g. 'en', 'ru', 'pt-BR')".into(),
        ))
    }
}

async fn run_transcription(
    settings: &StoredSttSettings,
    audio: Vec<u8>,
    language: &str,
) -> Result<TranscriptionResponse, ApiError> {
    validate_language(language)?;
    match settings.provider {
        SttProvider::Deepgram => {
            let api_key = settings
                .deepgram_api_key
                .as_deref()
                .filter(|k| !k.is_empty())
                .ok_or_else(|| {
                    ApiError::BadRequest(
                        "Deepgram API key not configured. Set it via PUT /stt/settings.".into(),
                    )
                })?;
            let transcript = vasya_core::stt::transcribe_deepgram(api_key, audio, language)
                .await
                .map_err(|e| {
                    // The key is a secret — log the failure, never the key.
                    tracing::error!(error = %e, provider = "deepgram", "STT transcription failed");
                    ApiError::Telegram(e)
                })?;
            Ok(TranscriptionResponse {
                text: transcript.text,
                language: transcript.language,
            })
        }
        SttProvider::LocalWhisper => Err(ApiError::BadRequest(
            "Local Whisper is desktop-only: it requires the whisper.cpp sidecar, which is not \
             part of the server image. Switch the provider to 'deepgram' (PUT /stt/settings)."
                .into(),
        )),
    }
}

/// Download a single message's voice/audio media into memory.
async fn fetch_message_audio(
    wrapper: &TelegramClientWrapper,
    chat_id: i64,
    message_id: i32,
) -> Result<Vec<u8>, ApiError> {
    let chat = resolve_peer(wrapper, chat_id).await?;

    // Same lookup window as the media download route.
    let mut messages_iter = wrapper
        .client
        .iter_messages(&chat)
        .offset_id(message_id + 1)
        .limit(50);

    let mut target = None;
    while let Some(msg) = messages_iter
        .next()
        .await
        .map_err(|e| ApiError::telegram(format!("Failed to get messages: {e}")))?
    {
        if msg.id() == message_id {
            target = Some(msg);
            break;
        }
    }

    let message = target.ok_or_else(|| {
        ApiError::NotFound(format!("Message {message_id} not found in chat {chat_id}"))
    })?;
    let media = message
        .media()
        .ok_or_else(|| ApiError::NotFound("Message has no media".into()))?;

    let tmp_path = std::env::temp_dir().join(format!("stt_{}.audio", uuid::Uuid::new_v4()));
    let download =
        with_flood_wait_retry(|| async { wrapper.client.download_media(&media, &tmp_path).await });
    let result = tokio::time::timeout(std::time::Duration::from_secs(120), download)
        .await
        .map_err(|_| ApiError::Telegram("Audio download timed out".into()))?;
    result.map_err(|e| ApiError::telegram(format!("Failed to download audio: {e}")))?;

    let bytes = tokio::fs::read(&tmp_path)
        .await
        .map_err(ApiError::internal)?;
    let _ = tokio::fs::remove_file(&tmp_path).await;
    Ok(bytes)
}

/// `POST /stt/transcribe`. Two input shapes, chosen by `Content-Type`:
/// - `application/json` → `{ accountId, chatId, messageId, language? }`: fetch
///   the Telegram voice message and transcribe it.
/// - anything else → the request body is the raw audio (optional `x-language`
///   header overrides the configured language).
pub async fn transcribe(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
    agent: Option<Extension<AgentIdentity>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<TranscriptionResponse>, ApiError> {
    let settings = load_settings(&ctx, &user.0 .0).await?;

    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let is_json = content_type
        .split(';')
        .next()
        .map(|s| s.trim().eq_ignore_ascii_case("application/json"))
        .unwrap_or(false);

    if is_json {
        let req: TranscribeRef = serde_json::from_slice(&body)
            .map_err(|e| ApiError::BadRequest(format!("Invalid JSON body: {e}")))?;
        // This shape targets a specific account via the JSON body, so the
        // path-based per-account allowlist in `policy.rs` (which only sees
        // `/stt/transcribe`) can't enforce it — do it here, mirroring REST.
        if let Some(Extension(agent)) = &agent {
            if !agent.allows_account(&req.account_id) {
                return Err(ApiError::Forbidden("account not in key allowlist".into()));
            }
        }
        let language = req
            .language
            .as_deref()
            .filter(|l| !l.trim().is_empty())
            .unwrap_or(&settings.language)
            .to_string();

        let wrapper = account_client(&ctx, &user.0, &req.account_id).await?;
        let audio = fetch_message_audio(&wrapper, req.chat_id, req.message_id).await?;
        tracing::info!(
            user = %user.0 .0, account_id = %req.account_id, chat_id = req.chat_id,
            message_id = req.message_id, size = audio.len(), "Transcribing voice message"
        );
        Ok(Json(run_transcription(&settings, audio, &language).await?))
    } else {
        if body.is_empty() {
            return Err(ApiError::BadRequest(
                "Empty audio body. Send raw audio bytes, or a JSON voice-message reference.".into(),
            ));
        }
        let language = headers
            .get("x-language")
            .and_then(|v| v.to_str().ok())
            .filter(|l| !l.trim().is_empty())
            .unwrap_or(&settings.language)
            .to_string();
        tracing::info!(user = %user.0 .0, size = body.len(), "Transcribing raw audio upload");
        Ok(Json(
            run_transcription(&settings, body.to_vec(), &language).await?,
        ))
    }
}

// --- Whisper models (desktop-only on the server) ------------------------------

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WhisperModelStatus {
    pub name: String,
    pub downloaded: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WhisperModelsResponse {
    /// Always false on the server — local Whisper needs the desktop sidecar.
    pub available: bool,
    pub reason: String,
    pub models: Vec<WhisperModelStatus>,
}

const WHISPER_SERVER_REASON: &str =
    "Local Whisper is desktop-only: it requires the whisper.cpp sidecar (~1 GB RAM), which is \
     not part of the server image. Use the cloud Deepgram provider on the server.";

fn whisper_models_unavailable() -> WhisperModelsResponse {
    WhisperModelsResponse {
        available: false,
        reason: WHISPER_SERVER_REASON.into(),
        models: ["tiny", "base", "small"]
            .iter()
            .map(|name| WhisperModelStatus {
                name: (*name).into(),
                downloaded: false,
            })
            .collect(),
    }
}

/// `GET /stt/models` — Whisper model status. On the server this always reports
/// local Whisper as unavailable (structured, not 501).
pub async fn get_models() -> Json<WhisperModelsResponse> {
    Json(whisper_models_unavailable())
}

/// `POST /stt/models/download` — would download a Whisper model on desktop;
/// on the server it is a no-op with a structured "unavailable" response.
pub async fn download_model() -> (StatusCode, Json<WhisperModelsResponse>) {
    (StatusCode::OK, Json(whisper_models_unavailable()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_keys_without_leaking() {
        let masked = mask_key("dg_secret_abcd1234");
        assert_eq!(masked, "••••1234");
        assert!(!masked.contains("secret"));
        // Short keys still mask the prefix.
        assert_eq!(mask_key("xy"), "••••xy");
    }

    #[test]
    fn response_never_includes_raw_key() {
        let settings = StoredSttSettings {
            provider: SttProvider::Deepgram,
            deepgram_api_key: Some("super-secret-key-9999".into()),
            whisper_model: "small".into(),
            language: "en".into(),
        };
        let resp = settings.to_response();
        assert!(resp.deepgram_api_key_set);
        assert_eq!(resp.deepgram_api_key_masked.as_deref(), Some("••••9999"));
        let json = serde_json::to_string(&resp).unwrap();
        assert!(!json.contains("super-secret-key-9999"));
        assert!(!json.contains("deepgramApiKey\":\"super"));
    }

    #[test]
    fn default_provider_is_deepgram_on_server() {
        let settings = StoredSttSettings::default();
        assert_eq!(settings.provider, SttProvider::Deepgram);
        let resp = settings.to_response();
        assert_eq!(resp.provider, "deepgram");
        assert!(!resp.deepgram_api_key_set);
        assert!(resp.deepgram_api_key_masked.is_none());
    }

    #[test]
    fn provider_parse_roundtrip() {
        assert_eq!(
            SttProvider::parse("deepgram").unwrap(),
            SttProvider::Deepgram
        );
        assert_eq!(
            SttProvider::parse("local_whisper").unwrap(),
            SttProvider::LocalWhisper
        );
        assert!(SttProvider::parse("bogus").is_err());
    }

    #[test]
    fn language_validation_blocks_url_injection() {
        // Valid BCP-47-ish tags.
        assert!(validate_language("en").is_ok());
        assert!(validate_language("ru").is_ok());
        assert!(validate_language("pt-BR").is_ok());
        // Injection / malformed values rejected (would alter the Deepgram URL).
        assert!(validate_language("ru&tier=enhanced").is_err());
        assert!(validate_language("en&model=evil").is_err());
        assert!(validate_language("en ").is_err());
        assert!(validate_language("").is_err());
        assert!(validate_language(&"a".repeat(17)).is_err());
    }

    #[tokio::test]
    async fn local_whisper_rejected_on_server() {
        let settings = StoredSttSettings {
            provider: SttProvider::LocalWhisper,
            ..StoredSttSettings::default()
        };
        let err = run_transcription(&settings, vec![1, 2, 3], "en")
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[tokio::test]
    async fn deepgram_without_key_is_bad_request() {
        let settings = StoredSttSettings::default(); // deepgram, no key
        let err = run_transcription(&settings, vec![1, 2, 3], "en")
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[test]
    fn whisper_models_report_unavailable() {
        let resp = whisper_models_unavailable();
        assert!(!resp.available);
        assert_eq!(resp.models.len(), 3);
        assert!(resp.models.iter().all(|m| !m.downloaded));
    }
}
