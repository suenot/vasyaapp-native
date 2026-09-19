use super::*;
use chacha20poly1305::{
    aead::{Aead, KeyInit},
    ChaCha20Poly1305, Nonce,
};
use rand::RngCore;
type Provider = Arc<dyn vasya_core::telegram::master_key::MasterKeyProvider>;
async fn key(provider: Provider) -> Result<[u8; 32]> {
    tokio::task::spawn_blocking(move || provider.get_or_create()).await?
}
pub(super) async fn save_secret(provider: Provider, path: &Path, value: &Value) -> Result<()> {
    let cipher = ChaCha20Poly1305::new_from_slice(&key(provider).await?).expect("32-byte key");
    let mut nonce = [0u8; 12];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let plaintext = serde_json::to_vec(value)?;
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), plaintext.as_slice())
        .map_err(|_| anyhow::anyhow!("Failed to encrypt profile settings"))?;
    private_json(
        path,
        &json!({"version":1,"nonce":nonce,"ciphertext":ciphertext}),
    )
    .await
}
pub(super) async fn load_secret(provider: Provider, path: &Path) -> Result<Option<Value>> {
    let Some(stored) = read_json(path).await? else {
        return Ok(None);
    };
    anyhow::ensure!(stored["version"] == 1, "Unknown encrypted settings format");
    let nonce: Vec<u8> = serde_json::from_value(stored["nonce"].clone())?;
    anyhow::ensure!(nonce.len() == 12, "Invalid encrypted settings nonce");
    let ciphertext: Vec<u8> = serde_json::from_value(stored["ciphertext"].clone())?;
    let cipher = ChaCha20Poly1305::new_from_slice(&key(provider).await?).expect("32-byte key");
    let plaintext = cipher
        .decrypt(Nonce::from_slice(&nonce), ciphertext.as_slice())
        .map_err(|_| {
            anyhow::anyhow!("Profile settings could not be decrypted with this profile's key")
        })?;
    Ok(Some(serde_json::from_slice(&plaintext)?))
}
impl Backend {
    pub(super) async fn save_connection(&self, remote: &Remote) -> Result<()> {
        save_secret(
            self.0.key_provider.clone(),
            &self.0.dir.join("connection.enc"),
            &json!({"base":remote.base,"token":remote.token}),
        )
        .await
    }
    pub(super) async fn load_connection(&self) -> Result<Option<Remote>> {
        let Some(value) = load_secret(
            self.0.key_provider.clone(),
            &self.0.dir.join("connection.enc"),
        )
        .await?
        else {
            return Ok(None);
        };
        Ok(Some(Remote {
            base: value["base"].as_str().context("Missing remote URL")?.into(),
            token: value["token"]
                .as_str()
                .context("Missing remote token")?
                .into(),
        }))
    }
    pub(super) async fn save_local_api(&self, enabled: bool, port: u16) -> Result<()> {
        save_secret(
            self.0.key_provider.clone(),
            &self.0.dir.join("local-api.enc"),
            &json!({"enabled":enabled,"port":port,"token":self.0.token}),
        )
        .await
    }
}
