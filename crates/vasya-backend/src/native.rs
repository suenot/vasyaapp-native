use super::*;
const MODELS: &[&str] = &[
    "tiny", "base", "small", "medium", "large-v3", "tiny.en", "base.en", "small.en",
];
impl Backend {
    pub(super) async fn native(&self, method: &str, path: &str, body: Value) -> Result<Value> {
        match (method, path.split('?').next().unwrap_or(path)) {
            ("GET", "/native/storage") => Ok(self.storage_status()),
            ("PUT", "/native/storage") => self.configure_storage(body, true).await,
            ("GET", "/native/connection") => {
                let remote = self.0.remote.read().await;
                Ok(match remote.as_ref() {
                    Some(remote) => json!({"remote":true,"baseUrl":remote.base}),
                    None => json!({"remote":false,"baseUrl":null}),
                })
            }
            ("GET", "/native/settings") => Ok(read_json(&self.0.dir.join("settings.json"))
                .await?
                .unwrap_or(
                    json!({"dark":true,"scale":1.0,"notifications":true,"auto_transcribe":false}),
                )),
            ("PUT", "/native/settings") => {
                anyhow::ensure!(body.is_object(), "Settings must be an object");
                private_json(&self.0.dir.join("settings.json"), &body).await?;
                Ok(body)
            }
            ("GET", "/native/capabilities") => Ok(
                json!({"version":env!("CARGO_PKG_VERSION"),"embedded":true,"remote":true,"localApi":true,"deepgram":true,"localWhisper":sidecar().is_some(),"calls":{"signaling":true,"audio":false,"video":false}}),
            ),
            ("GET", "/native/cache") => {
                let query = path.split_once('?').map(|(_, v)| v).unwrap_or("");
                let encoded = query.strip_prefix("path=").context("Missing cache path")?;
                let api_path = percent_encoding::percent_decode_str(encoded).decode_utf8()?;
                validate_path(&api_path)?;
                anyhow::ensure!(cacheable(&api_path), "This route is not cached");
                if self.0.remote.read().await.is_none() {
                    if let Some(value) = self.storage_cached_chats(&api_path).await? {
                        return Ok(value);
                    }
                }
                let scope = self.cache_scope(&*self.0.remote.read().await);
                Ok(read_json(&self.cache_path(&scope, &api_path))
                    .await?
                    .unwrap_or(Value::Null))
            }
            ("POST", "/native/local-api") => {
                let mut listener = self.0.listener.lock().await;
                let port = body["port"].as_u64().unwrap_or(8787);
                anyhow::ensure!(port <= u16::MAX as u64, "Invalid port");
                if listener
                    .as_ref()
                    .is_none_or(|(address, _)| address.port() != port as u16)
                {
                    let socket =
                        tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port as u16))
                            .await?;
                    let address = socket.local_addr()?;
                    let router = self.0.router.clone();
                    let task = tokio::spawn(async move {
                        if let Err(error) = axum::serve(socket, router).await {
                            tracing::warn!(%error, "Local API stopped");
                        }
                    });
                    if let Err(error) = self.save_local_api(true, address.port()).await {
                        task.abort();
                        return Err(error);
                    }
                    *self.0.local_api_error.write().await = None;
                    if let Some((_, previous)) = listener.take() {
                        previous.abort();
                        let _ = previous.await;
                    }
                    *listener = Some((address, task));
                }
                Ok(
                    json!({"running":true,"address":format!("http://{}", listener.as_ref().unwrap().0),"token":self.0.token}),
                )
            }
            ("GET", "/native/local-api") => {
                let listener = self.0.listener.lock().await;
                Ok(match listener.as_ref() {
                    Some((addr, _)) => {
                        json!({"running":true,"address":format!("http://{addr}"),"token":self.0.token})
                    }
                    None => json!({"running":false,"error":*self.0.local_api_error.read().await}),
                })
            }
            ("DELETE", "/native/local-api") => {
                self.save_local_api(false, 8787).await?;
                if let Some((_, task)) = self.0.listener.lock().await.take() {
                    task.abort();
                    let _ = task.await;
                }
                Ok(json!({"running":false}))
            }
            ("GET", "/native/stt/models") => {
                let models: Vec<_> = MODELS.iter().map(|model| json!({"id":model,"installed":self.0.dir.join("models").join(format!("ggml-{model}.bin")).exists()})).collect();
                Ok(json!({"available":sidecar().is_some(),"models":models}))
            }
            ("POST", "/native/stt/models/install") => {
                let _lock = self.0.stt_lock.lock().await;
                let model = valid_model(&body)?;
                let output = self.0.dir.join("models").join(format!("ggml-{model}.bin"));
                tokio::fs::create_dir_all(output.parent().unwrap()).await?;
                let temp = output.with_extension("partial");
                let result = download_model(&model, &temp).await;
                if let Err(error) = result {
                    let _ = tokio::fs::remove_file(&temp).await;
                    return Err(error);
                }
                tokio::fs::rename(temp, output).await?;
                Ok(json!({"installed":true,"model":model}))
            }
            ("POST", "/native/stt/transcribe") => {
                let _lock = self.0.stt_lock.lock().await;
                let input = PathBuf::from(body["filePath"].as_str().context("filePath required")?);
                anyhow::ensure!(input.is_file(), "Audio file not found");
                let model = valid_model(&body)?;
                let model_path = self.0.dir.join("models").join(format!("ggml-{model}.bin"));
                anyhow::ensure!(
                    model_path.is_file(),
                    "Install the selected Whisper model first"
                );
                let executable =
                    sidecar().context("stt-sidecar is not installed next to this executable")?;
                let output = tokio::time::timeout(
                    Duration::from_secs(600),
                    tokio::process::Command::new(executable)
                        .arg("--model")
                        .arg(model_path)
                        .arg("--input")
                        .arg(input)
                        .arg("--language")
                        .arg(body["language"].as_str().unwrap_or("auto"))
                        .kill_on_drop(true)
                        .output(),
                )
                .await??;
                anyhow::ensure!(
                    output.status.success(),
                    "Whisper transcription failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                Ok(serde_json::from_slice(&output.stdout)?)
            }
            _ => bail!("Unknown native operation {method} {path}"),
        }
    }
}
fn valid_model(body: &Value) -> Result<&str> {
    let model = body["model"].as_str().unwrap_or("small");
    anyhow::ensure!(MODELS.contains(&model), "Unknown Whisper model");
    Ok(model)
}
fn sidecar() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    let parent = executable.parent()?;
    [
        parent.join("stt-sidecar"),
        parent.join("../Resources/stt-sidecar"),
    ]
    .into_iter()
    .find(|p| p.is_file())
}

fn model_bounds(model: &str) -> (u64, u64) {
    match model.trim_end_matches(".en") {
        "tiny" => (50_000_000, 100_000_000),
        "base" => (100_000_000, 200_000_000),
        "small" => (350_000_000, 600_000_000),
        "medium" => (1_300_000_000, 1_700_000_000),
        _ => (2_800_000_000, 3_300_000_000),
    }
}
fn validate_model_header(header: &[u8]) -> Result<()> {
    anyhow::ensure!(
        header.starts_with(b"lmgg"),
        "Downloaded file is not a Whisper GGML model"
    );
    Ok(())
}
async fn download_model(model: &str, temporary: &Path) -> Result<()> {
    // Redirects belong to the model host's CDN; this client has no API credentials.
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(1800))
        .build()?;
    let response = client
        .get(format!(
            "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-{model}.bin"
        ))
        .send()
        .await?
        .error_for_status()?;
    let (minimum, maximum) = model_bounds(model);
    let expected = response.content_length();
    if let Some(length) = expected {
        anyhow::ensure!(
            (minimum..=maximum).contains(&length),
            "Unexpected model size"
        );
    }
    let mut file = tokio::fs::File::create(temporary).await?;
    let mut stream = response.bytes_stream();
    let mut received = 0u64;
    let mut header = Vec::new();
    use tokio::io::AsyncWriteExt;
    while let Some(chunk) = tokio::time::timeout(Duration::from_secs(60), stream.next()).await? {
        let chunk = chunk?;
        received += chunk.len() as u64;
        anyhow::ensure!(received <= maximum, "Model exceeds expected size limit");
        if header.len() < 4 {
            header.extend(chunk.iter().take(4 - header.len()));
            if header.len() == 4 {
                validate_model_header(&header)?;
            }
        }
        file.write_all(&chunk).await?;
    }
    anyhow::ensure!(
        received >= minimum && expected.is_none_or(|length| length == received),
        "Incomplete model download"
    );
    validate_model_header(&header)?;
    file.sync_all().await?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn model_integrity_rejects_html_and_limits_each_model() {
        assert!(validate_model_header(b"<!DO").is_err());
        assert!(validate_model_header(b"lmgg").is_ok());
        assert_eq!(model_bounds("tiny"), model_bounds("tiny.en"));
        assert!(model_bounds("tiny").1 < model_bounds("large-v3").0);
        assert!(valid_model(&json!({"model":"../../escape"})).is_err());
    }
}
