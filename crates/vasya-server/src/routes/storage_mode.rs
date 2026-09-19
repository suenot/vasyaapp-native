//! Storage-mode over REST.
//!
//! `storage-mode` is a desktop-client concept: the app chooses where *it* keeps
//! data — `local` (on-device SQLite) or `remote` (this backend server). On the
//! server the answer is fixed: `vasya-server` always persists state server-side
//! (file-backed on its data volume), so the mode is reported honestly as
//! `server` and is **not** configurable.
//!
//! This replaces the previous blanket `501`, mirroring the STT pattern: a
//! structured, discoverable response instead of "not implemented".

use axum::Json;
use serde::Serialize;

use crate::error::ApiError;

/// The server's fixed storage mode. `configurable` is always false here — the
/// local/remote toggle exists only in the desktop app.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageModeResponse {
    /// Always `"server"` on the server.
    pub mode: &'static str,
    /// Always `false`: the mode cannot be changed over the API.
    pub configurable: bool,
    /// Human-readable explanation of why the mode is fixed.
    pub reason: &'static str,
}

const SERVER_STORAGE_REASON: &str =
    "vasya-server always persists state server-side (file-backed on its data \
     volume). The local/remote storage toggle is a desktop-app setting and does \
     not apply to the server.";

fn server_storage_mode() -> StorageModeResponse {
    StorageModeResponse {
        mode: "server",
        configurable: false,
        reason: SERVER_STORAGE_REASON,
    }
}

/// `GET /storage-mode` — report the server's fixed storage mode (200, not 501).
pub async fn get_storage_mode() -> Json<StorageModeResponse> {
    Json(server_storage_mode())
}

/// `PUT /storage-mode` — the server's storage mode is fixed, so changes are
/// rejected with a clear `400` rather than pretending to switch (or 501-ing).
pub async fn set_storage_mode() -> Result<Json<StorageModeResponse>, ApiError> {
    Err(ApiError::BadRequest(
        "Storage mode is fixed on the server (always server-side); it is \
         configurable only in the desktop app."
            .into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_mode_is_fixed_and_not_configurable() {
        let m = server_storage_mode();
        assert_eq!(m.mode, "server");
        assert!(!m.configurable);
        assert!(!m.reason.is_empty());
    }

    #[tokio::test]
    async fn put_is_rejected_as_bad_request() {
        let err = set_storage_mode().await.unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)));
    }
}
