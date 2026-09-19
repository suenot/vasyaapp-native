//! Legacy desktop metadata-sync adapter. Independent of remote Telegram hosting.
use super::*;
impl Backend {
    pub(super) fn storage_status(&self) -> Value {
        match self.0.storage.read().unwrap().as_ref() {
            Some(remote) => {
                json!({"mode":"remote","url":remote.base,"apiKeySet":!remote.token.is_empty()})
            }
            None => json!({"mode":"local","url":null,"apiKeySet":false}),
        }
    }
    pub(super) async fn configure_storage(&self, body: Value, persist: bool) -> Result<Value> {
        let mode = body["mode"].as_str().unwrap_or("local");
        let config = match mode {
            "local" => None,
            "remote" => {
                let url =
                    reqwest::Url::parse(body["url"].as_str().context("Storage URL required")?)?;
                anyhow::ensure!(
                    url.scheme() == "https"
                        || (url.scheme() == "http"
                            && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))),
                    "Storage requires HTTPS except on loopback"
                );
                anyhow::ensure!(
                    url.username().is_empty()
                        && url.password().is_none()
                        && url.query().is_none()
                        && url.fragment().is_none(),
                    "Storage URL must not contain credentials, query, or fragment"
                );
                let previous = self.0.storage.read().unwrap().clone();
                let base = url.as_str().trim_end_matches('/').to_string();
                let token = body["apiKey"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        previous
                            .filter(|p| p.base == base)
                            .map(|p| p.token)
                            .unwrap_or_default()
                    });
                Some(Remote { base, token })
            }
            _ => bail!("Storage mode must be local or remote"),
        };
        if persist {
            let stored = match config.as_ref() {
                Some(r) => json!({"mode":"remote","url":r.base,"apiKey":r.token}),
                None => json!({"mode":"local"}),
            };
            connection::save_secret(
                self.0.key_provider.clone(),
                &self.0.dir.join("storage.enc"),
                &stored,
            )
            .await?;
        }
        *self.0.storage.write().unwrap() = config;
        Ok(self.storage_status())
    }
    pub(super) async fn storage_request(
        &self,
        method: &str,
        path: &str,
        body: &Value,
    ) -> Result<Option<Value>> {
        let config = self.0.storage.read().unwrap().clone();
        let Some(config) = config else {
            return Ok(None);
        };
        let segments: Vec<_> = path
            .split('?')
            .next()
            .unwrap_or(path)
            .split('/')
            .filter(|v| !v.is_empty())
            .collect();
        if segments.len() < 5 || segments[0..3] != ["api", "v1", "accounts"] {
            return Ok(None);
        }
        let account = percent_encoding::percent_decode_str(segments[3]).decode_utf8()?;
        let route = segments[4];
        if !matches!(route, "folders" | "tabs") {
            return Ok(None);
        }
        let mut url = reqwest::Url::parse(&format!("{}/api/{route}", config.base))?;
        if segments.len() == 6 && route == "folders" && method == "DELETE" {
            url.path_segments_mut()
                .map_err(|_| anyhow::anyhow!("Invalid storage base URL"))?
                .push(segments[5]);
        }
        url.query_pairs_mut().append_pair("account_id", &account);
        let remote_method = if route == "tabs" && method == "PUT" {
            "POST"
        } else {
            method
        };
        let mut payload = body.clone();
        if route == "folders" && method == "POST" {
            payload["account_id"] = json!(account);
        }
        if route == "tabs" && remote_method == "POST" {
            if let Some(items) = payload.as_array_mut() {
                for item in items {
                    item["account_id"] = json!(account);
                }
            }
        }
        Ok(Some(
            sync_request(&self.0.client, &config, remote_method, url, payload).await?,
        ))
    }
    pub(super) async fn storage_cached_chats(&self, path: &str) -> Result<Option<Value>> {
        let config = self.0.storage.read().unwrap().clone();
        let Some(config) = config else {
            return Ok(None);
        };
        let Some(account) = chat_account(path) else {
            return Ok(None);
        };
        let mut url = reqwest::Url::parse(&format!("{}/api/chats", config.base))?;
        url.query_pairs_mut().append_pair("account_id", &account);
        let value = sync_request(&self.0.client, &config, "GET", url, Value::Null).await?;
        let chats = value
            .as_array()
            .context("Storage chat response must be an array")?;
        Ok(Some(Value::Array(chats.iter().map(|c|json!({"id":c["id"],"title":c["title"],"username":c["username"],"chatType":c["chat_type"],"unreadCount":c["unread_count"],"lastMessage":c["last_message"],"avatarPath":null,"isForum":c["is_forum"],"isMuted":false})).collect())))
    }
    pub(super) fn mirror_chats(&self, path: &str, value: &Value) {
        let Some(account) = chat_account(path) else {
            return;
        };
        let Some(config) = self.0.storage.read().unwrap().clone() else {
            return;
        };
        let Ok(permit) = self.0.storage_sync.clone().try_acquire_owned() else {
            return;
        };
        let Some(chats) = value.as_array() else {
            return;
        };
        let chats = chats.clone();
        let client = self.0.client.clone();
        let events = self.0.events.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let url = match reqwest::Url::parse(&format!("{}/api/chats", config.base)) {
                Ok(url) => url,
                Err(_) => return,
            };
            let jobs=futures_util::stream::iter(chats.into_iter().map(|c|{
                let client=client.clone();let config=config.clone();let url=url.clone();let account=account.clone();
                async move{sync_request(&client,&config,"POST",url,json!({"id":c["id"],"account_id":account,"chat_type":c["chatType"],"title":c["title"],"username":c["username"],"avatar_path":null,"last_message":c["lastMessage"],"unread_count":c["unreadCount"],"is_forum":c["isForum"]})).await}
            })).buffer_unordered(4);
            tokio::pin!(jobs);
            while let Some(result) = jobs.next().await {
                if result.is_err() {
                    events.emit("native:error",json!({"message":"Metadata synchronization failed; check storage service settings"}));
                    break;
                }
            }
        });
    }
}
fn chat_account(path: &str) -> Option<String> {
    let route = path.split('?').next()?;
    let segments: Vec<_> = route.split('/').filter(|v| !v.is_empty()).collect();
    if segments.len() == 5 && segments[0..3] == ["api", "v1", "accounts"] && segments[4] == "chats"
    {
        Some(
            percent_encoding::percent_decode_str(segments[3])
                .decode_utf8()
                .ok()?
                .into_owned(),
        )
    } else {
        None
    }
}
async fn sync_request(
    client: &reqwest::Client,
    config: &Remote,
    method: &str,
    url: reqwest::Url,
    body: Value,
) -> Result<Value> {
    let mut request = client
        .request(method.parse()?, url)
        .timeout(Duration::from_secs(30));
    if !config.token.is_empty() {
        request = request.bearer_auth(&config.token);
    }
    if !body.is_null() {
        request = request.json(&body);
    }
    let response = request.send().await?.error_for_status()?;
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        anyhow::ensure!(
            bytes.len() + chunk.len() <= 16 * 1024 * 1024,
            "Storage response exceeds 16 MiB"
        );
        bytes.extend_from_slice(&chunk);
    }
    if method != "GET" || bytes.is_empty() {
        Ok(Value::Null)
    } else {
        Ok(serde_json::from_slice(&bytes)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn legacy_storage_contract_and_encrypted_settings_restore() {
        use axum::{
            extract::Query,
            routing::{get, post},
            Json,
        };
        async fn folders(
            Query(query): Query<std::collections::HashMap<String, String>>,
        ) -> Json<Value> {
            assert_eq!(query["account_id"], "a1");
            Json(json!([{"id":"work","account_id":"a1","name":"Work"}]))
        }
        async fn save_folder(Json(body): Json<Value>) -> axum::http::StatusCode {
            assert_eq!(body["account_id"], "a1");
            axum::http::StatusCode::NO_CONTENT
        }
        let app=Router::new().route("/api/folders",get(folders).post(save_folder)).route("/api/tabs",post(||async{axum::http::StatusCode::NO_CONTENT})).route("/api/chats",get(||async{Json(json!([{"id":7,"title":"Synced","chat_type":"group","last_message":"Hi","unread_count":2,"is_forum":true}]))}));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let backend = Backend::new(dir.path().into()).await.unwrap();
        let config = backend
            .request(
                "PUT",
                "/native/storage",
                json!({"mode":"remote","url":format!("http://{address}"),"apiKey":"sync-secret"}),
            )
            .await
            .unwrap();
        assert_eq!(config["apiKeySet"], true);
        assert!(config.get("apiKey").is_none());
        assert_eq!(
            backend
                .request("GET", "/api/v1/accounts/a1/folders", Value::Null)
                .await
                .unwrap()[0]["id"],
            "work"
        );
        backend
            .request(
                "POST",
                "/api/v1/accounts/a1/folders",
                json!({"id":"work","account_id":"wrong","name":"Work"}),
            )
            .await
            .unwrap();
        backend
            .request("PUT", "/api/v1/accounts/a1/tabs", json!([]))
            .await
            .unwrap();
        let cache = backend
            .request(
                "GET",
                "/native/cache?path=%2Fapi%2Fv1%2Faccounts%2Fa1%2Fchats%3Fsource%3Dlive",
                Value::Null,
            )
            .await
            .unwrap();
        assert_eq!(cache[0]["chatType"], "group");
        assert_eq!(cache[0]["unreadCount"], 2);
        let stored = tokio::fs::read_to_string(dir.path().join("storage.enc"))
            .await
            .unwrap();
        assert!(!stored.contains("sync-secret"));
        backend.shutdown().await;
        let restored = Backend::new(dir.path().into()).await.unwrap();
        assert_eq!(restored.storage_status()["mode"], "remote");
        restored
            .request("PUT", "/native/storage", json!({"mode":"local"}))
            .await
            .unwrap();
        assert_eq!(restored.storage_status()["mode"], "local");
        assert_eq!(
            restored
                .request("GET", "/api/v1/accounts/a1/folders", Value::Null)
                .await
                .unwrap(),
            json!([])
        );
        restored.shutdown().await;
        server.abort();
    }
    #[tokio::test]
    async fn local_api_restores_enabled_port_and_stable_encrypted_token() {
        let dir = tempfile::tempdir().unwrap();
        let backend = Backend::new(dir.path().into()).await.unwrap();
        let first = backend
            .request("POST", "/native/local-api", json!({"port":0}))
            .await
            .unwrap();
        let stored = tokio::fs::read_to_string(dir.path().join("local-api.enc"))
            .await
            .unwrap();
        assert!(!stored.contains(first["token"].as_str().unwrap()));
        backend.shutdown().await;
        let restored = Backend::new(dir.path().into()).await.unwrap();
        let status = restored
            .request("GET", "/native/local-api", Value::Null)
            .await
            .unwrap();
        assert_eq!(status["running"], true);
        assert_eq!(status["token"], first["token"]);
        assert_eq!(status["address"], first["address"]);
        restored
            .request("DELETE", "/native/local-api", Value::Null)
            .await
            .unwrap();
        restored.shutdown().await;
        let stopped = Backend::new(dir.path().into()).await.unwrap();
        assert_eq!(
            stopped
                .request("GET", "/native/local-api", Value::Null)
                .await
                .unwrap()["running"],
            false
        );
        stopped.shutdown().await;
    }
}
