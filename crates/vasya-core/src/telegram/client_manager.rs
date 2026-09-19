//! Telegram Client Manager
//!
//! Manages multiple Telegram client sessions with real-time update streams.

use anyhow::{Context, Result};
use grammers_client::{Client, UpdatesConfiguration};
use grammers_mtsender::{ConnectionParams, SenderPool};
use grammers_session::storages::SqliteSession;
use grammers_session::updates::UpdatesLike;
use grammers_session::SessionData;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock as StdRwLock};
use tokio::sync::{mpsc, RwLock};
use tokio::task::JoinHandle;

use super::auth::UserInfo;
use super::encrypted_session::EncryptedSession;
use super::master_key::{KeychainKeyProvider, MasterKeyProvider};
use super::updates;

/// Telegram client wrapper with metadata
pub struct TelegramClientWrapper {
    pub client: Client,
    pub account_id: String,
    pub phone: String,
    pub user_info: Option<UserInfo>,
    pub peers: Arc<RwLock<HashMap<i64, grammers_client::types::Peer>>>,
}

/// Per-account handles for background tasks
struct AccountTasks {
    /// Handle for the updates listener
    updates_handle: Option<JoinHandle<()>>,
    /// Shutdown signal sender
    shutdown_tx: Option<updates::ShutdownTx>,
}

/// Manager for multiple Telegram clients
pub struct TelegramClientManager {
    clients: Arc<RwLock<HashMap<String, Arc<TelegramClientWrapper>>>>,
    tasks: Arc<RwLock<HashMap<String, AccountTasks>>>,
    /// Stored updates receivers, to be consumed when starting updates handler
    updates_receivers: Arc<RwLock<HashMap<String, mpsc::UnboundedReceiver<UpdatesLike>>>>,
    /// Session handles retained so pending changes can be flushed to disk
    /// (the other clone lives inside the SenderPool runner).
    sessions: Arc<RwLock<HashMap<String, Arc<EncryptedSession>>>>,
    pub sessions_dir: PathBuf,
    /// API credentials behind a std RwLock for in-place updates without replacing the manager
    credentials: StdRwLock<(i32, String)>,
    /// Where the session encryption master key comes from (keychain on
    /// desktop, env/file on servers).
    key_provider: Arc<dyn MasterKeyProvider>,
    /// Optional SOCKS5 proxy for all Telegram (MTProto) connections, used when
    /// the host's IP has Telegram blocked. `None` = connect directly (default,
    /// unchanged). Set via the `TELEGRAM_PROXY_URL` env var (the feature flag):
    /// e.g. `socks5://user:pass@127.0.0.1:1080`.
    proxy_url: Option<String>,
}

/// Strip credentials from a proxy URL so it is safe to log.
/// `socks5://user:pass@host:port` -> `socks5://host:port`.
fn proxy_url_for_log(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => {
            let hostpart = rest.rsplit('@').next().unwrap_or(rest);
            format!("{scheme}://{hostpart}")
        }
        None => "***".to_string(),
    }
}

/// Read the Telegram proxy URL from the environment (the feature flag).
/// Absent or empty -> `None` (direct connection, unchanged behavior).
fn proxy_from_env() -> Option<String> {
    std::env::var("TELEGRAM_PROXY_URL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

impl TelegramClientManager {
    /// Desktop constructor: master key in the OS keychain with a key-file
    /// fallback (unchanged historical behavior).
    pub fn new(sessions_dir: PathBuf, api_id: i32, api_hash: String) -> Self {
        let key_provider = Arc::new(KeychainKeyProvider::desktop_default(sessions_dir.clone()));
        Self::with_key_provider(sessions_dir, api_id, api_hash, key_provider)
    }

    /// Server constructor: bring your own key provider
    /// (e.g. `EnvKeyProvider::default_var()` reading `SESSION_MASTER_KEY`).
    pub fn with_key_provider(
        sessions_dir: PathBuf,
        api_id: i32,
        api_hash: String,
        key_provider: Arc<dyn MasterKeyProvider>,
    ) -> Self {
        let proxy_url = proxy_from_env();
        if let Some(url) = &proxy_url {
            tracing::info!(
                proxy = %proxy_url_for_log(url),
                "Telegram connections will be routed through a SOCKS5 proxy (TELEGRAM_PROXY_URL)"
            );
        }
        Self {
            clients: Arc::new(RwLock::new(HashMap::new())),
            tasks: Arc::new(RwLock::new(HashMap::new())),
            updates_receivers: Arc::new(RwLock::new(HashMap::new())),
            sessions: Arc::new(RwLock::new(HashMap::new())),
            sessions_dir,
            credentials: StdRwLock::new((api_id, api_hash)),
            key_provider,
            proxy_url,
        }
    }

    /// Build a `SenderPool` for a session, honoring the optional SOCKS5 proxy.
    /// `proxy_url == None` reproduces the historical `SenderPool::new` path.
    fn build_pool(&self, session: Arc<EncryptedSession>) -> SenderPool {
        self.build_pool_with_api_id(session, self.api_id())
    }

    /// Like [`build_pool`] but with an explicit `api_id` so a login can use
    /// per-user Telegram credentials instead of the manager's global default.
    fn build_pool_with_api_id(&self, session: Arc<EncryptedSession>, api_id: i32) -> SenderPool {
        match &self.proxy_url {
            Some(url) => SenderPool::with_configuration(
                session,
                api_id,
                ConnectionParams {
                    proxy_url: Some(url.clone()),
                    ..Default::default()
                },
            ),
            None => SenderPool::new(session, api_id),
        }
    }

    /// Opens the encrypted session for an account, transparently migrating a
    /// legacy plaintext SQLite session if one is found. The plaintext file is
    /// deleted only after the encrypted snapshot is safely on disk.
    fn open_session(&self, account_id: &str) -> Result<Arc<EncryptedSession>> {
        let key = self
            .key_provider
            .get_or_create()
            .context("Failed to obtain session encryption key")?;
        let enc_path = self
            .sessions_dir
            .join(format!("{}.session.enc", account_id));
        let legacy_path = self.sessions_dir.join(format!("{}.session", account_id));

        if enc_path.exists() {
            return Ok(Arc::new(
                EncryptedSession::load(&enc_path, &key)
                    .context("Failed to load encrypted session")?,
            ));
        }

        if legacy_path.exists() {
            tracing::info!(account_id = %account_id, "Migrating plaintext session to encrypted storage");
            let sqlite = SqliteSession::open(legacy_path.to_str().unwrap())
                .context("Failed to open legacy session for migration")?;
            // Keeps auth keys, the self peer and the updates state; the peer
            // cache is rebuilt from the dialog list on the next sync.
            let data = SessionData::from(sqlite);
            let session = EncryptedSession::create(&enc_path, &key, data)
                .context("Failed to write migrated encrypted session")?;
            std::fs::remove_file(&legacy_path)
                .context("Failed to remove plaintext session after migration")?;
            return Ok(Arc::new(session));
        }

        Ok(Arc::new(
            EncryptedSession::create(&enc_path, &key, SessionData::default())
                .context("Failed to create session file")?,
        ))
    }

    /// Get the current API ID
    pub fn api_id(&self) -> i32 {
        self.credentials.read().unwrap().0
    }

    /// Get the current API Hash
    pub fn api_hash(&self) -> String {
        self.credentials.read().unwrap().1.clone()
    }

    /// Update API credentials in place (no manager replacement needed)
    pub fn update_credentials(&self, api_id: i32, api_hash: String) {
        *self.credentials.write().unwrap() = (api_id, api_hash);
    }

    /// Create a new client and SenderPool, store wrapper, return it.
    /// Does NOT start the updates handler yet (call `start_updates` after auth).
    pub async fn create_client(
        &self,
        account_id: String,
        phone: String,
    ) -> Result<Arc<TelegramClientWrapper>> {
        let api_id = self.api_id();
        self.create_client_with_api_id(account_id, phone, api_id)
            .await
    }

    /// Like [`create_client`] but with an explicit `api_id` for the SenderPool,
    /// so a login can use the caller's per-user Telegram credentials. The
    /// matching `api_hash` is passed separately to `request_login_code`.
    pub async fn create_client_with_api_id(
        &self,
        account_id: String,
        phone: String,
        api_id: i32,
    ) -> Result<Arc<TelegramClientWrapper>> {
        let session = self.open_session(&account_id)?;
        self.sessions
            .write()
            .await
            .insert(account_id.clone(), session.clone());

        let pool = self.build_pool_with_api_id(session, api_id);
        let client = Client::new(&pool);

        // Destructure pool — runner drives the network, save updates receiver
        let SenderPool {
            runner,
            updates,
            handle: _,
        } = pool;

        tokio::spawn(runner.run());
        tracing::info!(account_id = %account_id, "SenderPool runner started");

        // Store updates receiver for later use by start_updates
        self.updates_receivers
            .write()
            .await
            .insert(account_id.clone(), updates);

        let wrapper = Arc::new(TelegramClientWrapper {
            client,
            account_id: account_id.clone(),
            phone,
            user_info: None,
            peers: Arc::new(RwLock::new(HashMap::new())),
        });

        self.clients
            .write()
            .await
            .insert(account_id.clone(), wrapper.clone());
        Ok(wrapper)
    }

    /// Start the real-time updates handler for an account.
    /// Should be called after successful authentication.
    pub async fn start_updates(
        &self,
        account_id: &str,
        ctx: updates::UpdatesContext,
    ) -> Result<()> {
        let wrapper = self
            .get_client(account_id)
            .await
            .context("Client not found")?;

        // Take the updates receiver (can only be consumed once)
        let updates_rx = self
            .updates_receivers
            .write()
            .await
            .remove(account_id)
            .context("Updates receiver not found (already consumed or never created)")?;

        // Create the UpdateStream from client + receiver
        let update_stream = wrapper
            .client
            .stream_updates(updates_rx, UpdatesConfiguration::default());

        // Create shutdown channel
        let (shutdown_tx, shutdown_rx) = updates::shutdown_channel();

        // Spawn updates handler with the UpdateStream
        let handle = updates::spawn_updates_handler(
            update_stream,
            account_id.to_string(),
            wrapper.clone(),
            ctx,
            shutdown_rx,
        );

        // Store task handles
        self.tasks.write().await.insert(
            account_id.to_string(),
            AccountTasks {
                updates_handle: Some(handle),
                shutdown_tx: Some(shutdown_tx),
            },
        );

        tracing::info!(account_id = %account_id, "Updates handler started");
        Ok(())
    }

    /// Stop the updates handler for an account gracefully.
    /// Sends shutdown signal and waits for the task to finish (up to 5s).
    async fn stop_updates(&self, account_id: &str) {
        let task = self.tasks.write().await.remove(account_id);
        if let Some(account_tasks) = task {
            // Send shutdown signal first
            if let Some(tx) = account_tasks.shutdown_tx {
                let _ = tx.send(());
            }
            // Wait for graceful shutdown (avoids panic in UpdateStream::drop)
            if let Some(handle) = account_tasks.updates_handle {
                match tokio::time::timeout(std::time::Duration::from_secs(5), handle).await {
                    Ok(Ok(())) => {
                        tracing::info!(account_id = %account_id, "Updates handler stopped gracefully");
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(account_id = %account_id, error = %e, "Updates handler panicked during shutdown");
                    }
                    Err(_) => {
                        tracing::warn!(account_id = %account_id, "Updates handler did not stop within timeout, detaching");
                    }
                }
            }
        }
    }

    pub async fn get_client(&self, account_id: &str) -> Option<Arc<TelegramClientWrapper>> {
        self.clients.read().await.get(account_id).cloned()
    }

    pub async fn remove_client(&self, account_id: &str) -> Result<()> {
        anyhow::ensure!(
            !account_id.is_empty()
                && account_id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "Invalid account ID"
        );
        // Fence all persistence before shutdown: retained SenderPool/session Arcs
        // can otherwise flush a dirty snapshot after the session file is deleted.
        if let Some(session) = self.sessions.write().await.remove(account_id) {
            session.disable_persistence();
        }
        self.stop_updates(account_id).await;
        self.updates_receivers.write().await.remove(account_id);
        if let Some(wrapper) = self.clients.write().await.remove(account_id) {
            wrapper.client.disconnect();
        }
        for name in [
            format!("{}.session", account_id),
            format!("{}.session.enc", account_id),
        ] {
            match std::fs::remove_file(self.sessions_dir.join(name)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).context("Failed to remove session file"),
            }
        }
        Ok(())
    }

    pub async fn save_session(&self, account_id: &str) -> Result<()> {
        if let Some(session) = self.sessions.read().await.get(account_id) {
            session.flush()?;
        }
        Ok(())
    }

    /// Flush every session's pending changes to disk (call on app shutdown).
    pub async fn flush_all_sessions(&self) {
        for (account_id, session) in self.sessions.read().await.iter() {
            if let Err(e) = session.flush() {
                tracing::error!(account_id = %account_id, error = %e, "Failed to flush session");
            }
        }
    }

    pub async fn list_clients(&self) -> Vec<String> {
        self.clients.read().await.keys().cloned().collect()
    }

    /// Load existing sessions from disk.
    /// Updates handlers are NOT started here — call `start_updates` per account after setup.
    pub async fn load_existing_sessions(&self) -> Result<Vec<String>> {
        let mut loaded = Vec::new();

        if !self.sessions_dir.exists() {
            tracing::warn!(path = ?self.sessions_dir, "Sessions directory does not exist");
            return Ok(loaded);
        }

        let entries =
            std::fs::read_dir(&self.sessions_dir).context("Failed to read sessions directory")?;

        // Collect unique account ids from both storage formats: encrypted
        // (`<id>.session.enc`) and legacy plaintext (`<id>.session`, migrated
        // on open). The key file (`.session.key`) matches neither suffix.
        let mut account_ids: Vec<String> = Vec::new();
        for entry in entries {
            let entry = entry.context("Failed to read directory entry")?;
            let Some(name) = entry.file_name().to_str().map(String::from) else {
                continue;
            };
            let account_id = name
                .strip_suffix(".session.enc")
                .or_else(|| name.strip_suffix(".session"))
                .unwrap_or_default();
            if account_id.is_empty() || account_ids.iter().any(|a| a == account_id) {
                continue;
            }
            account_ids.push(account_id.to_string());
        }

        for account_id in account_ids {
            tracing::info!(account_id = %account_id, "Loading session from disk");

            let session = match self.open_session(&account_id) {
                Ok(session) => session,
                Err(e) => {
                    // Don't take the whole app down over one bad session file;
                    // the user can re-login that account.
                    tracing::error!(account_id = %account_id, error = %e, "Failed to open session, skipping account");
                    continue;
                }
            };
            self.sessions
                .write()
                .await
                .insert(account_id.clone(), session.clone());

            let pool = self.build_pool(session);
            let client = Client::new(&pool);

            let SenderPool {
                runner,
                updates,
                handle: _,
            } = pool;

            tokio::spawn(runner.run());

            // Store updates receiver for later use
            self.updates_receivers
                .write()
                .await
                .insert(account_id.clone(), updates);

            let wrapper = Arc::new(TelegramClientWrapper {
                client,
                account_id: account_id.clone(),
                phone: String::new(),
                user_info: None,
                peers: Arc::new(RwLock::new(HashMap::new())),
            });

            self.clients
                .write()
                .await
                .insert(account_id.clone(), wrapper);
            loaded.push(account_id);
        }

        tracing::info!(count = loaded.len(), "Sessions loaded from disk");
        Ok(loaded)
    }
}

#[cfg(test)]
mod tests {
    use super::proxy_url_for_log;

    #[test]
    fn proxy_url_for_log_strips_credentials() {
        // Password must never reach the logs.
        assert_eq!(
            proxy_url_for_log("socks5://vasya:s3cr3t@127.0.0.1:1080"),
            "socks5://127.0.0.1:1080"
        );
        // No credentials -> unchanged.
        assert_eq!(
            proxy_url_for_log("socks5://127.0.0.1:1080"),
            "socks5://127.0.0.1:1080"
        );
        // Malformed -> fully redacted, never echoed back.
        assert_eq!(proxy_url_for_log("not-a-url"), "***");
    }
}
