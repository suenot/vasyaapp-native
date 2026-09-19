//! GUI-independent, asynchronous access to the complete Telegram API.
use anyhow::{bail, Context, Result};
use axum::{
    body::{to_bytes, Body},
    http::Request,
    Router,
};
use futures_util::StreamExt;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::{broadcast, Mutex, RwLock};
use tower::ServiceExt;
use vasya_core::{
    events::{BroadcastEventSink, Event, EventSink},
    TelegramClientManager,
};
use vasya_server::{AuthMode, ServerContext, ServerOptions};

const LIMIT: usize = 128 * 1024 * 1024;
#[derive(Clone)]
struct Remote {
    base: String,
    token: String,
}
struct Inner {
    ctx: Arc<ServerContext>,
    router: Router,
    token: String,
    dir: PathBuf,
    remote: RwLock<Option<Remote>>,
    generation: AtomicU64,
    events: Arc<BroadcastEventSink>,
    client: reqwest::Client,
    stream_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    listener: Mutex<Option<(std::net::SocketAddr, tokio::task::JoinHandle<()>)>>,
    stt_lock: Mutex<()>,
    connection_lock: Mutex<()>,
    storage: std::sync::RwLock<Option<Remote>>,
    storage_sync: Arc<tokio::sync::Semaphore>,
    local_api_error: RwLock<Option<String>>,
    key_provider: Arc<dyn vasya_core::telegram::master_key::MasterKeyProvider>,
}
#[derive(Clone)]
pub struct Backend(Arc<Inner>);
impl Backend {
    pub async fn new(data_dir: PathBuf) -> Result<Self> {
        tokio::fs::create_dir_all(&data_dir).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o700)).await?;
        }
        let sessions = data_dir.join("sessions");
        tokio::fs::create_dir_all(&sessions).await?;
        let provider = vasya_core::telegram::master_key::KeychainKeyProvider::new(
            format!(
                "cc.marketmaker.vasya.native.{}",
                digest(data_dir.to_string_lossy().as_bytes())
            ),
            "session-master",
            &sessions,
        );
        let provider: Arc<dyn vasya_core::telegram::master_key::MasterKeyProvider> =
            Arc::new(provider);
        let creds: Value = read_json(&data_dir.join("telegram-creds/local/creds.json"))
            .await?
            .unwrap_or(Value::Null);
        let manager = Arc::new(TelegramClientManager::with_key_provider(
            sessions,
            creds["apiId"].as_i64().unwrap_or(0) as i32,
            creds["apiHash"].as_str().unwrap_or("").into(),
            provider.clone(),
        ));
        let local_api = connection::load_secret(provider.clone(), &data_dir.join("local-api.enc"))
            .await?
            .unwrap_or(Value::Null);
        let auth = match local_api["token"].as_str() {
            Some(token) if token.len() >= 32 => AuthMode::EmbeddedLocal {
                token: token.into(),
            },
            _ => AuthMode::embedded_with_random_token(),
        };
        let token = auth.embedded_token().unwrap().to_string();
        let mut options = ServerOptions::new(auth, data_dir.clone());
        // Native UI is trusted in-process; normal API still has ownership and token checks.
        options.rate_limit = vasya_server::RateLimitConfig {
            capacity: 1000,
            refill_every: Duration::from_millis(1),
        };
        let ctx = vasya_server::build_context(manager, options)?;
        let backend = Self(Arc::new(Inner {
            router: vasya_server::build_router(ctx.clone()),
            ctx: ctx.clone(),
            token,
            dir: data_dir,
            remote: RwLock::new(None),
            generation: AtomicU64::new(0),
            events: Arc::new(BroadcastEventSink::new(2048)),
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(15))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            stream_task: Mutex::new(None),
            listener: Mutex::new(None),
            stt_lock: Mutex::new(()),
            connection_lock: Mutex::new(()),
            storage: std::sync::RwLock::new(None),
            storage_sync: Arc::new(tokio::sync::Semaphore::new(1)),
            local_api_error: RwLock::new(None),
            key_provider: provider,
        }));
        let weak = Arc::downgrade(&backend.0);
        let mut rx = ctx.events.subscribe();
        tokio::spawn(async move {
            loop {
                let event = rx.recv().await;
                let Some(inner) = weak.upgrade() else { break };
                if inner.remote.read().await.is_some() {
                    continue;
                }
                match event {
                    Ok(e) => inner.events.emit(&e.name, e.payload),
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        inner.events.emit("native:resync", json!({}))
                    }
                    Err(_) => break,
                }
            }
        });
        let weak = Arc::downgrade(&backend.0);
        tokio::spawn(async move {
            let result = vasya_server::start_existing_sessions(&ctx).await;
            if let Some(inner) = weak.upgrade() {
                match result {
                    Ok(_) => inner.events.emit("native:resync", json!({})),
                    Err(_) => inner.events.emit(
                        "native:error",
                        json!({"message":"Session restore failed; sign in again"}),
                    ),
                }
            }
        });
        if let Some(value) = connection::load_secret(
            backend.0.key_provider.clone(),
            &backend.0.dir.join("storage.enc"),
        )
        .await?
        {
            backend.configure_storage(value, false).await?;
        }
        if local_api["enabled"].as_bool() == Some(true) {
            if let Err(error) = backend
                .native(
                    "POST",
                    "/native/local-api",
                    json!({"port":local_api["port"]}),
                )
                .await
            {
                *backend.0.local_api_error.write().await = Some(error.to_string());
            }
        }
        if let Some(remote) = backend.load_connection().await? {
            backend.set_remote(remote.base, remote.token).await?;
        }
        Ok(backend)
    }
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.0.events.subscribe()
    }
    pub async fn set_remote(&self, base_url: String, token: String) -> Result<()> {
        let _connection = self.0.connection_lock.lock().await;
        let url = reqwest::Url::parse(&base_url)?;
        anyhow::ensure!(
            url.scheme() == "https"
                || (url.scheme() == "http"
                    && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1"))),
            "Remote server requires HTTPS (HTTP allowed only on loopback)"
        );
        anyhow::ensure!(
            url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "Server URL must not contain credentials, query, or fragment"
        );
        anyhow::ensure!(!token.trim().is_empty(), "Server token is required");
        let remote = Remote {
            base: base_url.trim_end_matches('/').into(),
            token,
        };
        self.save_connection(&remote).await?;
        if let Some(task) = self.0.stream_task.lock().await.take() {
            task.abort();
        }
        *self.0.remote.write().await = Some(remote.clone());
        let generation = self.0.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let weak = Arc::downgrade(&self.0);
        *self.0.stream_task.lock().await = Some(tokio::spawn(async move {
            let mut delay = 1;
            loop {
                let Some(inner) = weak.upgrade() else { break };
                if inner.generation.load(Ordering::SeqCst) != generation {
                    break;
                }
                let request = inner
                    .client
                    .get(format!("{}/api/v1/events", remote.base))
                    .bearer_auth(&remote.token);
                let events = inner.events.clone();
                drop(inner);
                if let Ok(response) = request.send().await {
                    if response.status().is_success() {
                        events.emit("native:resync", json!({}));
                        delay = 1;
                        let mut bytes = response.bytes_stream();
                        let mut buffer = Vec::new();
                        while let Some(Ok(chunk)) = bytes.next().await {
                            buffer.extend_from_slice(&chunk);
                            if buffer.len() > 4 * 1024 * 1024 {
                                break;
                            }
                            while let Some((end, separator)) = frame_boundary(&buffer) {
                                let frame: Vec<_> = buffer.drain(..end + separator).collect();
                                if let Some(event) = parse_sse(&frame) {
                                    events.emit(&event.name, event.payload);
                                }
                            }
                        }
                    }
                }
                events.emit("native:connection", json!({"connected":false}));
                tokio::time::sleep(Duration::from_secs(delay)).await;
                delay = (delay * 2).min(30);
            }
        }));
        Ok(())
    }
    pub async fn set_embedded(&self) -> Result<()> {
        let _connection = self.0.connection_lock.lock().await;
        match tokio::fs::remove_file(self.0.dir.join("connection.enc")).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        self.0.generation.fetch_add(1, Ordering::SeqCst);
        if let Some(task) = self.0.stream_task.lock().await.take() {
            task.abort();
        }
        *self.0.remote.write().await = None;
        self.0.events.emit("native:resync", json!({}));
        Ok(())
    }
    pub async fn request(&self, method: &str, path: &str, body: Value) -> Result<Value> {
        if path.starts_with("/native/") {
            return self.native(method, path, body).await;
        }
        validate_path(path)?;
        let remote = self.0.remote.read().await.clone();
        let scope = self.cache_scope(&remote);
        if remote.is_none() {
            if let Some(value) = self.storage_request(method, path, &body).await? {
                return Ok(value);
            }
        }
        let embedded = remote.is_none();
        let bytes = self
            .send(
                method,
                path,
                if body.is_null() {
                    Vec::new()
                } else {
                    serde_json::to_vec(&body)?
                },
                "application/json",
                remote,
            )
            .await?;
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).context("API returned invalid JSON")?
        };
        if method == "GET" && cacheable(path) {
            private_json(&self.cache_path(&scope, path), &value).await?;
            prune_cache(&self.0.dir.join("cache").join(&scope), 256 * 1024 * 1024).await?;
        }
        if embedded && method == "GET" {
            self.mirror_chats(path, &value);
        }
        Ok(value)
    }
    async fn send(
        &self,
        method: &str,
        path: &str,
        body: Vec<u8>,
        content_type: &str,
        remote: Option<Remote>,
    ) -> Result<Vec<u8>> {
        self.send_typed(method, path, body, content_type, remote)
            .await
            .map(|(bytes, _)| bytes)
    }
    async fn send_typed(
        &self,
        method: &str,
        path: &str,
        body: Vec<u8>,
        content_type: &str,
        remote: Option<Remote>,
    ) -> Result<(Vec<u8>, String)> {
        validate_path(path)?;
        if let Some(remote) = remote {
            let response = self
                .0
                .client
                .request(method.parse()?, format!("{}{}", remote.base, path))
                .bearer_auth(remote.token)
                .header("content-type", content_type)
                .body(body)
                .timeout(Duration::from_secs(180))
                .send()
                .await?;
            let status = response.status();
            let mime = response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("application/octet-stream")
                .to_string();
            let mut stream = response.bytes_stream();
            let mut bytes = Vec::new();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                anyhow::ensure!(
                    bytes.len() + chunk.len() <= LIMIT,
                    "API response exceeds 128 MiB"
                );
                bytes.extend_from_slice(&chunk);
            }
            if !status.is_success() {
                bail!("API {status}: {}", String::from_utf8_lossy(&bytes));
            }
            Ok((bytes, mime))
        } else {
            let request = Request::builder()
                .method(method)
                .uri(path)
                .header("Authorization", format!("Bearer {}", self.0.token))
                .header("content-type", content_type)
                .body(Body::from(body))?;
            let response = self.0.router.clone().oneshot(request).await?;
            let status = response.status();
            let mime = response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("application/octet-stream")
                .to_string();
            let bytes = to_bytes(response.into_body(), LIMIT).await?;
            if !status.is_success() {
                bail!("API {status}: {}", String::from_utf8_lossy(&bytes));
            }
            Ok((bytes.to_vec(), mime))
        }
    }
    pub async fn download(&self, path: &str) -> Result<PathBuf> {
        validate_path(path)?;
        let remote = self.0.remote.read().await.clone();
        let base = self.0.dir.join("downloads").join(digest(
            format!("{}:{path}", self.cache_scope(&remote)).as_bytes(),
        ));
        for extension in DOWNLOAD_EXTENSIONS {
            let cached = base.with_extension(extension);
            if cached.is_file() {
                return Ok(cached);
            }
        }
        let (bytes, mime) = if base.is_file() {
            (tokio::fs::read(&base).await?, String::new())
        } else {
            self.send_typed("GET", path, vec![], "application/octet-stream", remote)
                .await?
        };
        let extension = download_extension(&mime, &bytes);
        let output = base.with_extension(extension);
        tokio::fs::create_dir_all(output.parent().unwrap()).await?;
        let temporary = base.with_extension(format!(
            "{}.tmp",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        tokio::fs::write(&temporary, bytes).await?;
        tokio::fs::rename(&temporary, &output).await?;
        if base.is_file() {
            let _ = tokio::fs::remove_file(base).await;
        }
        prune_cache(output.parent().unwrap(), 512 * 1024 * 1024).await?;
        Ok(output)
    }
    pub async fn upload(&self, path: &str, file: PathBuf) -> Result<Value> {
        let remote = self.0.remote.read().await.clone();
        anyhow::ensure!(
            tokio::fs::metadata(&file).await?.len() <= LIMIT as u64,
            "File exceeds 128 MiB"
        );
        validate_path(path)?;
        let filename = file
            .file_name()
            .context("Missing filename")?
            .to_string_lossy();
        let filename =
            percent_encoding::utf8_percent_encode(&filename, percent_encoding::NON_ALPHANUMERIC)
                .to_string();
        let content_type = match file
            .extension()
            .and_then(|v| v.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str()
        {
            "jpg" | "jpeg" => "image/jpeg",
            "png" => "image/png",
            "webp" => "image/webp",
            "mp4" => "video/mp4",
            "m4a" => "audio/mp4",
            "ogg" | "opus" => "audio/ogg",
            "wav" => "audio/wav",
            "mp3" => "audio/mpeg",
            _ => "application/octet-stream",
        };
        let query_url = reqwest::Url::parse(&format!("http://localhost{path}"))?;
        let fields: std::collections::HashMap<_, _> =
            query_url.query_pairs().into_owned().collect();
        let caption = percent_encoding::utf8_percent_encode(
            fields.get("caption").map(String::as_str).unwrap_or(""),
            percent_encoding::NON_ALPHANUMERIC,
        )
        .to_string();
        let topic = fields.get("topic_id").map(String::as_str).unwrap_or("");
        let voice = fields.get("voice").map(String::as_str).unwrap_or("false");
        let duration = fields.get("duration").map(String::as_str).unwrap_or("0");
        let body = tokio::fs::read(file).await?;
        let (status, bytes) = if let Some(remote) = remote {
            let response = self
                .0
                .client
                .post(format!("{}{}", remote.base, path))
                .bearer_auth(remote.token)
                .header("content-type", content_type)
                .header("x-file-name", &filename)
                .header("x-mime-type", content_type)
                .header("x-caption", &caption)
                .header("x-topic-id", topic)
                .header("x-voice", voice)
                .header("x-duration", duration)
                .body(body)
                .timeout(Duration::from_secs(180))
                .send()
                .await?;
            (response.status(), response.bytes().await?.to_vec())
        } else {
            let request = Request::post(path)
                .header("Authorization", format!("Bearer {}", self.0.token))
                .header("content-type", content_type)
                .header("x-file-name", &filename)
                .header("x-mime-type", content_type)
                .header("x-caption", &caption)
                .header("x-topic-id", topic)
                .header("x-voice", voice)
                .header("x-duration", duration)
                .body(Body::from(body))?;
            let response = self.0.router.clone().oneshot(request).await?;
            (
                response.status(),
                to_bytes(response.into_body(), LIMIT).await?.to_vec(),
            )
        };
        if !status.is_success() {
            bail!("API {status}: {}", String::from_utf8_lossy(&bytes));
        }
        Ok(if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)?
        })
    }
    fn cache_scope(&self, remote: &Option<Remote>) -> String {
        remote
            .as_ref()
            .map(|r| digest(format!("{}:{}", r.base, r.token).as_bytes()))
            .unwrap_or_else(|| {
                self.0
                    .storage
                    .read()
                    .unwrap()
                    .as_ref()
                    .map(|r| {
                        format!(
                            "embedded-sync-{}",
                            digest(format!("{}:{}", r.base, r.token).as_bytes())
                        )
                    })
                    .unwrap_or_else(|| "embedded".into())
            })
    }
    fn cache_path(&self, scope: &str, path: &str) -> PathBuf {
        self.0
            .dir
            .join("cache")
            .join(scope)
            .join(format!("{}.json", digest(path.as_bytes())))
    }
    pub async fn shutdown(&self) {
        if let Some(task) = self.0.stream_task.lock().await.take() {
            task.abort();
        }
        if let Some((_, task)) = self.0.listener.lock().await.take() {
            task.abort();
            let _ = task.await;
        }
        self.0.ctx.manager.flush_all_sessions().await;
    }
}
const DOWNLOAD_EXTENSIONS: &[&str] = &[
    "jpg", "png", "gif", "webp", "mp4", "m4a", "mp3", "ogg", "wav", "pdf", "zip", "txt", "bin",
];
fn download_extension(mime: &str, bytes: &[u8]) -> &'static str {
    // Prefer signatures: servers intentionally serve documents as octet-stream.
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return "png";
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return "jpg";
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return "gif";
    }
    if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        return "webp";
    }
    if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WAVE") {
        return "wav";
    }
    if bytes.starts_with(b"OggS") {
        return "ogg";
    }
    if bytes.starts_with(b"%PDF-") {
        return "pdf";
    }
    if bytes.starts_with(b"PK\x03\x04") {
        return "zip";
    }
    if bytes.get(4..8) == Some(b"ftyp") {
        return if bytes.get(8..11) == Some(b"M4A") || mime.starts_with("audio/") {
            "m4a"
        } else {
            "mp4"
        };
    }
    if bytes.starts_with(b"ID3") || (bytes.len() > 1 && bytes[0] == 0xff && bytes[1] & 0xe0 == 0xe0)
    {
        return "mp3";
    }
    match mime.split(';').next().unwrap_or("").trim() {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "video/mp4" => "mp4",
        "audio/mp4" | "audio/x-m4a" => "m4a",
        "audio/mpeg" => "mp3",
        "audio/ogg" | "audio/opus" => "ogg",
        "audio/wav" | "audio/x-wav" => "wav",
        "application/pdf" => "pdf",
        "application/zip" => "zip",
        "text/plain" => "txt",
        _ => "bin",
    }
}
async fn prune_cache(directory: &Path, maximum: u64) -> Result<()> {
    let mut entries = tokio::fs::read_dir(directory).await?;
    let mut files = Vec::new();
    let mut total = 0;
    while let Some(entry) = entries.next_entry().await? {
        let Ok(metadata) = entry.metadata().await else {
            continue;
        };
        if !metadata.is_file() || entry.path().extension().is_some_and(|v| v == "tmp") {
            continue;
        }
        total += metadata.len();
        files.push((
            metadata.modified().unwrap_or(std::time::UNIX_EPOCH),
            metadata.len(),
            entry.path(),
        ));
    }
    if total > maximum {
        files.sort_unstable_by_key(|(modified, _, _)| *modified);
        for (_, size, path) in files {
            if total <= maximum {
                break;
            }
            if tokio::fs::remove_file(path).await.is_ok() {
                total = total.saturating_sub(size);
            }
        }
    }
    Ok(())
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn validate_path(path: &str) -> Result<()> {
    anyhow::ensure!(
        path.starts_with("/api/v1/")
            && !path.contains("..")
            && !path.contains('#')
            && !path.contains('\r')
            && !path.contains('\n'),
        "Invalid API path"
    );
    Ok(())
}
fn cacheable(path: &str) -> bool {
    path == "/api/v1/accounts"
        || (path.starts_with("/api/v1/accounts/")
            && (path.contains("/chats")
                || path.contains("/messages")
                || path.contains("/folders")
                || path.contains("/tabs")))
}
async fn read_json(path: &Path) -> Result<Option<Value>> {
    match tokio::fs::read(path).await {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
async fn private_json(path: &Path, value: &Value) -> Result<()> {
    tokio::fs::create_dir_all(path.parent().unwrap()).await?;
    let temp = path.with_extension(format!(
        "{}.tmp",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    use tokio::io::AsyncWriteExt;
    let mut file = options.open(&temp).await?;
    file.write_all(&serde_json::to_vec(value)?).await?;
    file.sync_all().await?;
    tokio::fs::rename(temp, path).await?;
    Ok(())
}
fn frame_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    let lf = buffer.windows(2).position(|w| w == b"\n\n").map(|i| (i, 2));
    let crlf = buffer
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| (i, 4));
    [lf, crlf].into_iter().flatten().min_by_key(|(i, _)| *i)
}
fn parse_sse(frame: &[u8]) -> Option<Event> {
    let text = std::str::from_utf8(frame).ok()?;
    let mut name = "message";
    let mut data = Vec::new();
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("event:") {
            name = value.trim();
        }
        if let Some(value) = line.strip_prefix("data:") {
            data.push(value.trim_start());
        }
    }
    Some(Event {
        name: name.into(),
        payload: serde_json::from_str(&data.join("\n")).ok()?,
    })
}
mod connection;
mod native;
mod storage;

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn embedded_rest_graphql_cache_and_preferences_restore() {
        let dir = tempfile::tempdir().unwrap();
        let backend = Backend::new(dir.path().into()).await.unwrap();
        assert_eq!(
            backend
                .request("GET", "/api/v1/health", Value::Null)
                .await
                .unwrap()["status"],
            "ok"
        );
        assert_eq!(
            backend
                .request("GET", "/api/v1/accounts", Value::Null)
                .await
                .unwrap(),
            json!([])
        );
        assert_eq!(
            backend
                .request(
                    "POST",
                    "/api/v1/graphql",
                    json!({"query":"{ accounts { accountId } }"})
                )
                .await
                .unwrap()["data"]["accounts"],
            json!([])
        );
        assert_eq!(
            backend
                .request(
                    "GET",
                    "/native/cache?path=%2Fapi%2Fv1%2Faccounts",
                    Value::Null
                )
                .await
                .unwrap(),
            json!([])
        );
        backend
            .request(
                "PUT",
                "/native/settings",
                json!({"dark":false,"scale":1.25}),
            )
            .await
            .unwrap();
        backend.shutdown().await;
        let restored = Backend::new(dir.path().into()).await.unwrap();
        assert_eq!(
            restored
                .request("GET", "/native/settings", Value::Null)
                .await
                .unwrap()["scale"],
            1.25
        );
        assert_eq!(
            restored
                .request("GET", "/native/capabilities", Value::Null)
                .await
                .unwrap()["calls"]["audio"],
            false
        );
        restored.shutdown().await;
    }
    #[tokio::test]
    async fn local_api_requires_token_and_binds_only_loopback() {
        let dir = tempfile::tempdir().unwrap();
        let backend = Backend::new(dir.path().into()).await.unwrap();
        let status = backend
            .request("POST", "/native/local-api", json!({"port":0}))
            .await
            .unwrap();
        let address = status["address"].as_str().unwrap();
        assert!(address.starts_with("http://127.0.0.1:"));
        let client = reqwest::Client::new();
        assert_eq!(
            client
                .get(format!("{address}/api/v1/accounts"))
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        assert_eq!(
            client
                .get(format!("{address}/api/v1/accounts"))
                .bearer_auth(status["token"].as_str().unwrap())
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        backend.shutdown().await;
    }
    #[tokio::test]
    async fn rejects_unsafe_remote_and_route_paths() {
        let dir = tempfile::tempdir().unwrap();
        let backend = Backend::new(dir.path().into()).await.unwrap();
        assert!(backend
            .set_remote("http://example.com".into(), "token".into())
            .await
            .is_err());
        assert!(backend
            .set_remote("https://user:secret@example.com".into(), "token".into())
            .await
            .is_err());
        for path in [
            "https://example.com/",
            "/api/v1/../secret",
            "/api/v1/accounts\r\nsecret",
        ] {
            assert!(backend.request("GET", path, Value::Null).await.is_err());
        }
        assert_ne!(
            backend.cache_scope(&None),
            backend.cache_scope(&Some(Remote {
                base: "https://example.com".into(),
                token: "a".into()
            }))
        );
        backend.shutdown().await;
    }
    #[tokio::test]
    async fn remote_rest_sse_and_cache_are_isolated_from_embedded() {
        use axum::{response::IntoResponse, routing::get};
        let app = Router::new()
            .route(
                "/api/v1/accounts",
                get(|| async { axum::Json(json!([{"accountId":"remote"}])) }),
            )
            .route(
                "/api/v1/events",
                get(|| async {
                    (
                        [("content-type", "text/event-stream")],
                        "event: telegram:new-message\r\ndata: {\"accountId\":\"remote\"}\r\n\r\n",
                    )
                        .into_response()
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let backend = Backend::new(dir.path().into()).await.unwrap();
        backend
            .request("GET", "/api/v1/accounts", Value::Null)
            .await
            .unwrap();
        let mut events = backend.subscribe();
        backend
            .set_remote(format!("http://{address}"), "test-token".into())
            .await
            .unwrap();
        assert_eq!(
            backend
                .request(
                    "GET",
                    "/native/cache?path=%2Fapi%2Fv1%2Faccounts",
                    Value::Null
                )
                .await
                .unwrap(),
            Value::Null
        );
        assert_eq!(
            backend
                .request("GET", "/api/v1/accounts", Value::Null)
                .await
                .unwrap()[0]["accountId"],
            "remote"
        );
        let remote_event = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let event = events.recv().await.unwrap();
                if event.name == "telegram:new-message" {
                    break event;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(remote_event.payload["accountId"], "remote");
        backend.set_embedded().await.unwrap();
        assert_eq!(
            backend
                .request(
                    "GET",
                    "/native/cache?path=%2Fapi%2Fv1%2Faccounts",
                    Value::Null
                )
                .await
                .unwrap(),
            json!([])
        );
        backend.shutdown().await;
        server.abort();
    }

    #[tokio::test]
    async fn remote_connection_is_encrypted_restored_and_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let backend = Backend::new(dir.path().into()).await.unwrap();
        backend
            .set_remote("http://127.0.0.1:1".into(), "super-secret-token".into())
            .await
            .unwrap();
        let stored = tokio::fs::read_to_string(dir.path().join("connection.enc"))
            .await
            .unwrap();
        assert!(!stored.contains("super-secret-token"));
        backend.shutdown().await;
        let restored = Backend::new(dir.path().into()).await.unwrap();
        let connection = restored
            .request("GET", "/native/connection", Value::Null)
            .await
            .unwrap();
        assert_eq!(
            connection,
            json!({"remote":true,"baseUrl":"http://127.0.0.1:1"})
        );
        restored.set_embedded().await.unwrap();
        restored.shutdown().await;
        let embedded = Backend::new(dir.path().into()).await.unwrap();
        assert_eq!(
            embedded
                .request("GET", "/native/connection", Value::Null)
                .await
                .unwrap()["remote"],
            false
        );
        embedded.shutdown().await;
    }

    #[tokio::test]
    async fn downloaded_files_get_real_extensions_and_remain_cached() {
        use axum::{response::IntoResponse, routing::get};
        let app = Router::new().route(
            "/api/v1/test-media",
            get(|| async {
                (
                    [("content-type", "application/octet-stream")],
                    b"%PDF-1.7 test fixture".as_slice(),
                )
                    .into_response()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let backend = Backend::new(dir.path().into()).await.unwrap();
        backend
            .set_remote(format!("http://{address}"), "test-token".into())
            .await
            .unwrap();
        let path = backend.download("/api/v1/test-media").await.unwrap();
        assert_eq!(path.extension().unwrap(), "pdf");
        server.abort();
        let _ = server.await;
        assert_eq!(backend.download("/api/v1/test-media").await.unwrap(), path);
        backend.shutdown().await;
    }
    #[test]
    fn media_signatures_preserve_playback_extensions() {
        assert_eq!(
            download_extension("application/octet-stream", b"\x89PNG\r\n\x1a\n"),
            "png"
        );
        assert_eq!(
            download_extension("application/octet-stream", b"OggS"),
            "ogg"
        );
        assert_eq!(
            download_extension("audio/mp4", b"\0\0\0\x18ftypM4A "),
            "m4a"
        );
        assert_eq!(
            download_extension("video/mp4", b"\0\0\0\x18ftypisom"),
            "mp4"
        );
        assert_eq!(download_extension("unknown", b""), "bin");
    }

    #[test]
    fn sse_preserves_account_payload_and_ignores_comments() {
        let event = parse_sse(
            b"event: telegram:new-message\ndata: {\"accountId\":\"a1\",\"messageId\":7}\n\n",
        )
        .unwrap();
        assert_eq!(event.name, "telegram:new-message");
        assert_eq!(event.payload["accountId"], "a1");
        assert!(parse_sse(b":keepalive\n\n").is_none());
    }
}
