//! Wire types. Shapes (field names and casing) are kept byte-identical to
//! the Tauri command results so the existing frontend transport layer and
//! future HttpTransport can share one set of TypeScript types.
//! Note the historical casing split: Chat/search/topic types are camelCase,
//! Message/MediaInfo/UserInfo/folder types are snake_case. (GraphQL output
//! is uniformly camelCase — SimpleObject renames independently of serde.)

use async_graphql::{InputObject, SimpleObject};
use serde::{Deserialize, Serialize};

// --- Chats (camelCase, mirrors commands/chats.rs) -----------------------------

#[derive(Debug, Clone, Serialize, Deserialize, SimpleObject)]
#[serde(rename_all = "camelCase")]
pub struct Chat {
    pub id: i64,
    pub title: String,
    pub username: Option<String>,
    pub unread_count: i32,
    pub chat_type: String, // "user", "group", "channel"
    pub last_message: Option<String>,
    pub avatar_path: Option<String>,
    pub is_forum: bool,
    pub is_muted: bool,
}

// --- Messages (snake_case, mirrors commands/messages.rs) ----------------------

#[derive(Debug, Serialize, Deserialize, SimpleObject)]
pub struct MediaInfo {
    pub media_type: String,
    pub file_path: Option<String>,
    pub file_name: Option<String>,
    pub file_size: Option<u64>,
    pub mime_type: Option<String>,
    // Link preview metadata, populated for `media_type == "webpage"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub webpage_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub webpage_site_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub webpage_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub webpage_description: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, SimpleObject)]
pub struct Message {
    pub id: i32,
    pub chat_id: i64,
    pub from_user_id: Option<i64>,
    pub sender_name: Option<String>,
    pub text: Option<String>,
    pub date: i64,
    pub is_outgoing: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media: Option<Vec<MediaInfo>>,
}

// --- Auth (snake_case, mirrors vasya_core::telegram::auth::UserInfo) ----------

pub use vasya_core::telegram::auth::UserInfo;

// --- Search (camelCase, mirrors commands/search.rs) ---------------------------

#[derive(Debug, Serialize, Deserialize, SimpleObject)]
#[serde(rename_all = "camelCase")]
pub struct GlobalSearchResult {
    pub id: i64,
    pub title: String,
    pub username: Option<String>,
    pub result_type: String, // "user", "group", "channel"
    pub subscribers_count: Option<i32>,
}

#[derive(Debug, Serialize, Deserialize, SimpleObject)]
#[serde(rename_all = "camelCase")]
pub struct GlobalMessageResult {
    pub message_id: i32,
    pub chat_id: i64,
    pub chat_title: String,
    pub sender_name: Option<String>,
    pub text: Option<String>,
    pub date: i64,
}

// --- Topics (camelCase, mirrors commands/topics.rs) ---------------------------

#[derive(Debug, Serialize, Deserialize, SimpleObject)]
#[serde(rename_all = "camelCase")]
pub struct ForumTopic {
    pub id: i32,
    pub title: String,
    pub icon_color: i32,
    pub icon_emoji_id: Option<i64>,
    pub unread_count: i32,
    pub top_message: i32,
    pub is_pinned: bool,
    pub is_closed: bool,
}

// --- Folders / tabs (snake_case, mirrors storage/types.rs) --------------------

#[derive(Debug, Clone, Serialize, Deserialize, SimpleObject, InputObject)]
#[graphql(input_name = "FolderInput")]
pub struct FolderRecord {
    pub id: String,
    pub account_id: String,
    pub name: String,
    pub icon: Option<String>,
    pub included_chat_types: Vec<String>,
    pub excluded_chat_types: Vec<String>,
    pub included_chat_ids: Vec<i64>,
    pub excluded_chat_ids: Vec<i64>,
    pub sort_order: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, SimpleObject, InputObject)]
#[graphql(input_name = "TabInput")]
pub struct TabRecord {
    pub id: String,
    pub account_id: String,
    pub visible: bool,
    pub sort_order: i32,
}

// --- STT (speech-to-text) -----------------------------------------------------

/// STT settings as returned to the caller. The raw Deepgram key is never
/// included — only whether one is set and a masked preview (last 4 chars).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SttSettingsResponse {
    /// "deepgram" or "local_whisper".
    pub provider: String,
    /// Whether a Deepgram API key is stored for this user.
    pub deepgram_api_key_set: bool,
    /// Masked preview of the stored key (e.g. `••••1234`); `None` if unset.
    pub deepgram_api_key_masked: Option<String>,
    /// Whisper model name (desktop-only provider); kept for round-tripping.
    pub whisper_model: String,
    /// Default transcription language (BCP-47-ish, e.g. "ru", "en").
    pub language: String,
}

/// Partial STT settings update (PUT). Only present fields are changed; the
/// Deepgram key is write-only and never echoed back.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SttSettingsUpdate {
    pub provider: Option<String>,
    pub deepgram_api_key: Option<String>,
    pub whisper_model: Option<String>,
    pub language: Option<String>,
}

/// Transcription result: recognized text plus the language used.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptionResponse {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}
