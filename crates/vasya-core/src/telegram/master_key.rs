//! Pluggable master-key providers for encrypted session storage.
//!
//! The session files are ChaCha20-Poly1305 encrypted; where the 32-byte
//! master key comes from depends on the deployment:
//! * [`KeychainKeyProvider`] — desktop/mobile default: OS keychain with a
//!   0600 key-file fallback (the behavior the app has always had).
//! * [`EnvKeyProvider`] — servers: hex key injected via an environment
//!   variable (`SESSION_MASTER_KEY`) from a secret manager / KMS. Fails
//!   fast when missing — a server must never invent an ephemeral key, or
//!   existing sessions become undecryptable on restart.
//! * [`FileKeyProvider`] — a standalone key file, for setups without a
//!   keychain or env injection.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use rand::RngCore;

/// Default keychain coordinates used by the desktop app.
pub const KEYRING_SERVICE: &str = "com.suenot.vasyapp";
pub const KEYRING_USER: &str = "session-encryption-key";
/// Default key-file name inside the sessions dir (keychain fallback).
pub const KEY_FILE_NAME: &str = ".session.key";
/// Default env var read by [`EnvKeyProvider::default_var`].
pub const DEFAULT_ENV_VAR: &str = "SESSION_MASTER_KEY";

/// Source of the 32-byte session master key.
pub trait MasterKeyProvider: Send + Sync + 'static {
    /// Returns the master key, creating and persisting it on first use if
    /// the provider supports generation (env-based providers do not).
    fn get_or_create(&self) -> Result<[u8; 32]>;
}

fn encode_key(key: &[u8; 32]) -> String {
    key.iter().map(|b| format!("{b:02x}")).collect()
}

fn decode_key(hex: &str) -> Option<[u8; 32]> {
    let hex = hex.trim();
    if hex.len() != 64 {
        return None;
    }
    let mut key = [0u8; 32];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        key[i] = u8::from_str_radix(std::str::from_utf8(chunk).ok()?, 16).ok()?;
    }
    Some(key)
}

fn generate_key() -> [u8; 32] {
    let mut key = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut key);
    key
}

fn write_key_file(path: &Path, key: &[u8; 32]) -> Result<()> {
    std::fs::write(path, encode_key(key)).context("Failed to write session key file")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

// --- Keychain (desktop default) ---------------------------------------------

/// OS keychain with key-file fallback.
///
/// Resolution ladder (stable across runs):
/// 1. OS keychain (macOS/iOS Keychain, Windows Credential Manager, …)
/// 2. Key file in `fallback_dir` (platforms/setups without a usable
///    keychain — still better than plaintext sessions, and the 0600 file
///    at least keeps other users out)
/// 3. Generate fresh: prefer storing in the keychain, else the key file.
pub struct KeychainKeyProvider {
    service: String,
    user: String,
    fallback_dir: PathBuf,
}

impl KeychainKeyProvider {
    pub fn new(
        service: impl Into<String>,
        user: impl Into<String>,
        fallback_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            service: service.into(),
            user: user.into(),
            fallback_dir: fallback_dir.into(),
        }
    }

    /// The exact configuration the desktop app has always used.
    pub fn desktop_default(sessions_dir: impl Into<PathBuf>) -> Self {
        Self::new(KEYRING_SERVICE, KEYRING_USER, sessions_dir)
    }

    fn keyring_get(&self) -> Option<[u8; 32]> {
        let entry = keyring::Entry::new(&self.service, &self.user).ok()?;
        decode_key(&entry.get_password().ok()?)
    }

    fn keyring_set(&self, key: &[u8; 32]) -> bool {
        match keyring::Entry::new(&self.service, &self.user) {
            Ok(entry) => entry.set_password(&encode_key(key)).is_ok(),
            Err(_) => false,
        }
    }

    fn key_file_path(&self) -> PathBuf {
        self.fallback_dir.join(KEY_FILE_NAME)
    }
}

impl MasterKeyProvider for KeychainKeyProvider {
    fn get_or_create(&self) -> Result<[u8; 32]> {
        if let Some(key) = self.keyring_get() {
            return Ok(key);
        }
        if let Some(key) = std::fs::read_to_string(self.key_file_path())
            .ok()
            .and_then(|s| decode_key(&s))
        {
            return Ok(key);
        }

        let key = generate_key();
        if self.keyring_set(&key) {
            tracing::info!("Session encryption key created in the OS keychain");
        } else {
            write_key_file(&self.key_file_path(), &key)?;
            tracing::warn!("OS keychain unavailable — session key stored in a 0600 key file");
        }
        Ok(key)
    }
}

// --- Environment variable (server) -------------------------------------------

/// Reads the key as 64 hex chars from an environment variable.
pub struct EnvKeyProvider {
    var: String,
}

impl EnvKeyProvider {
    pub fn new(var: impl Into<String>) -> Self {
        Self { var: var.into() }
    }

    /// Reads [`DEFAULT_ENV_VAR`] (`SESSION_MASTER_KEY`).
    pub fn default_var() -> Self {
        Self::new(DEFAULT_ENV_VAR)
    }
}

impl MasterKeyProvider for EnvKeyProvider {
    fn get_or_create(&self) -> Result<[u8; 32]> {
        let raw = std::env::var(&self.var)
            .map_err(|_| anyhow!("{} is not set — inject the session master key", self.var))?;
        decode_key(&raw).ok_or_else(|| anyhow!("{} must be 64 hex chars (32 bytes)", self.var))
    }
}

// --- Key file -----------------------------------------------------------------

/// A standalone key file. Generates the key on first use; an existing but
/// malformed file is an error (regenerating would orphan existing sessions).
pub struct FileKeyProvider {
    path: PathBuf,
}

impl FileKeyProvider {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl MasterKeyProvider for FileKeyProvider {
    fn get_or_create(&self) -> Result<[u8; 32]> {
        if self.path.exists() {
            let content =
                std::fs::read_to_string(&self.path).context("Failed to read session key file")?;
            return decode_key(&content)
                .ok_or_else(|| anyhow!("Key file {:?} is malformed", self.path));
        }
        let key = generate_key();
        write_key_file(&self.path, &key)?;
        Ok(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_codec_roundtrip() {
        let key = generate_key();
        assert_eq!(decode_key(&encode_key(&key)), Some(key));
    }

    #[test]
    fn env_provider_reads_hex_key_and_fails_closed() {
        let var = format!("VASYA_TEST_KEY_{}", std::process::id());
        assert!(EnvKeyProvider::new(&var).get_or_create().is_err());

        let key = generate_key();
        std::env::set_var(&var, encode_key(&key));
        assert_eq!(EnvKeyProvider::new(&var).get_or_create().unwrap(), key);

        std::env::set_var(&var, "not-hex");
        assert!(EnvKeyProvider::new(&var).get_or_create().is_err());
        std::env::remove_var(&var);
    }

    #[test]
    fn file_provider_persists_across_calls() {
        let dir = std::env::temp_dir().join(format!("key-provider-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("master.key");

        let provider = FileKeyProvider::new(&path);
        let first = provider.get_or_create().unwrap();
        let second = provider.get_or_create().unwrap();
        assert_eq!(first, second);

        std::fs::write(&path, "garbage").unwrap();
        assert!(provider.get_or_create().is_err());

        std::fs::remove_dir_all(&dir).ok();
    }
}
