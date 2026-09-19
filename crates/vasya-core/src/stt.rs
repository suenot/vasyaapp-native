//! Shared speech-to-text core.
//!
//! Hosts the provider-neutral pieces of transcription that both front doors
//! reuse: the cloud **Deepgram** Nova-2 call and audio content-type sniffing.
//! The Tauri desktop command (`commands/stt.rs`) and the server route
//! (`vasya-server/src/routes/stt.rs`) call into here so there is one
//! implementation of the Deepgram path, two transports.
//!
//! Local **Whisper** stays desktop-only: it shells out to the `stt-sidecar`
//! binary (whisper.cpp, ~1 GB RAM), which is excluded from the server image,
//! so it has no place in this Tauri-free crate.
//!
//! Security: the Deepgram API key is a per-user secret. It is never logged
//! here — callers pass it in and are responsible for storing it masked.

use serde::Deserialize;

/// Deepgram listen API base. Overridable via [`transcribe_deepgram_at`] so
/// tests can point at a local mock without touching the network.
const DEEPGRAM_API_BASE: &str = "https://api.deepgram.com";

/// A transcription result: the recognized text and the language used.
#[derive(Debug, Clone)]
pub struct Transcript {
    pub text: String,
    pub language: Option<String>,
}

/// Sniff an audio MIME type from magic bytes — voice files routinely carry the
/// wrong (or no) extension. Defaults to Telegram's voice format (ogg/opus).
pub fn detect_audio_content_type(data: &[u8]) -> &'static str {
    if data.starts_with(b"OggS") {
        "audio/ogg"
    } else if data.len() >= 3 && &data[..3] == b"ID3" {
        "audio/mpeg"
    } else if data.len() >= 4 && &data[..4] == b"RIFF" {
        "audio/wav"
    } else if data.len() >= 2 && data[0] == 0xFF && (data[1] & 0xE0) == 0xE0 {
        "audio/mpeg"
    } else {
        "audio/ogg" // Telegram voice messages default
    }
}

#[derive(Deserialize)]
struct DeepgramResponse {
    results: Option<DeepgramResults>,
}

#[derive(Deserialize)]
struct DeepgramResults {
    channels: Vec<DeepgramChannel>,
}

#[derive(Deserialize)]
struct DeepgramChannel {
    alternatives: Vec<DeepgramAlternative>,
}

#[derive(Deserialize)]
struct DeepgramAlternative {
    transcript: String,
}

/// Pull the flat transcript text out of Deepgram's nested JSON.
fn parse_deepgram_transcript(body: &str) -> Result<String, String> {
    let resp: DeepgramResponse = serde_json::from_str(body)
        .map_err(|e| format!("Failed to parse Deepgram response: {e}"))?;
    Ok(resp
        .results
        .and_then(|r| r.channels.into_iter().next())
        .and_then(|c| c.alternatives.into_iter().next())
        .map(|a| a.transcript)
        .unwrap_or_default())
}

/// Transcribe raw audio bytes via Deepgram Nova-2 with a user-supplied key.
///
/// The `api_key` is a per-user secret and is never logged. Errors come back as
/// strings for the caller to map to an HTTP status / IPC error.
pub async fn transcribe_deepgram(
    api_key: &str,
    audio: Vec<u8>,
    language: &str,
) -> Result<Transcript, String> {
    transcribe_deepgram_at(DEEPGRAM_API_BASE, api_key, audio, language).await
}

/// Like [`transcribe_deepgram`] but against a configurable API base (tests).
pub async fn transcribe_deepgram_at(
    api_base: &str,
    api_key: &str,
    audio: Vec<u8>,
    language: &str,
) -> Result<Transcript, String> {
    if api_key.is_empty() {
        return Err("Deepgram API key not configured".into());
    }
    if audio.is_empty() {
        return Err("Empty audio payload".into());
    }

    let content_type = detect_audio_content_type(&audio);
    let url = format!(
        "{}/v1/listen?model=nova-2&language={}&smart_format=true&punctuate=true",
        api_base.trim_end_matches('/'),
        language
    );

    let client = reqwest::Client::new();
    let response = client
        .post(&url)
        .header("Authorization", format!("Token {api_key}"))
        .header("Content-Type", content_type)
        .body(audio)
        .send()
        .await
        .map_err(|e| format!("Deepgram request failed: {e}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!("Deepgram API error {status}: {body}"));
    }

    let body = response
        .text()
        .await
        .map_err(|e| format!("Failed to read Deepgram response: {e}"))?;
    let text = parse_deepgram_transcript(&body)?;

    Ok(Transcript {
        text,
        language: Some(language.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn sniffs_audio_content_types() {
        assert_eq!(detect_audio_content_type(b"OggS....."), "audio/ogg");
        assert_eq!(detect_audio_content_type(b"ID3 ...."), "audio/mpeg");
        assert_eq!(detect_audio_content_type(b"RIFF...."), "audio/wav");
        assert_eq!(detect_audio_content_type(&[0xFF, 0xFB, 0x00]), "audio/mpeg");
        // Unknown -> Telegram voice default.
        assert_eq!(detect_audio_content_type(&[0x00, 0x01, 0x02]), "audio/ogg");
        assert_eq!(detect_audio_content_type(&[]), "audio/ogg");
    }

    #[test]
    fn parses_deepgram_json() {
        let body = r#"{"results":{"channels":[{"alternatives":[{"transcript":"hello world"}]}]}}"#;
        assert_eq!(parse_deepgram_transcript(body).unwrap(), "hello world");
        // Empty/absent results -> empty string, not an error.
        assert_eq!(parse_deepgram_transcript(r#"{}"#).unwrap(), "");
        assert!(parse_deepgram_transcript("not json").is_err());
    }

    #[tokio::test]
    async fn empty_key_or_audio_rejected_without_network() {
        assert!(
            transcribe_deepgram_at("http://127.0.0.1:1", "", vec![1, 2, 3], "en")
                .await
                .is_err()
        );
        assert!(
            transcribe_deepgram_at("http://127.0.0.1:1", "k", vec![], "en")
                .await
                .is_err()
        );
    }

    /// Minimal one-shot HTTP responder mocking the Deepgram listen endpoint:
    /// reads the request, asserts the auth header, and returns canned JSON.
    async fn spawn_mock_deepgram(json: &'static str) -> (String, tokio::task::JoinHandle<bool>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = socket.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]);
            let auth_ok = req.contains("authorization: Token test-key")
                || req.contains("Authorization: Token test-key");
            let body = json.as_bytes();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.write_all(body).await.unwrap();
            socket.flush().await.unwrap();
            auth_ok
        });
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn transcribes_against_mock_deepgram() {
        let (base, handle) = spawn_mock_deepgram(
            r#"{"results":{"channels":[{"alternatives":[{"transcript":"привет"}]}]}}"#,
        )
        .await;

        let out = transcribe_deepgram_at(&base, "test-key", b"OggS-audio".to_vec(), "ru")
            .await
            .unwrap();

        assert_eq!(out.text, "привет");
        assert_eq!(out.language.as_deref(), Some("ru"));
        assert!(
            handle.await.unwrap(),
            "auth header should carry the user key"
        );
    }
}
