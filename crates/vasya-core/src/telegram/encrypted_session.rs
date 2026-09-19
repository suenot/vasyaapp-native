//! Encrypted at-rest Telegram session storage.
//!
//! grammers' `SqliteSession` keeps the MTProto auth keys in a plaintext
//! SQLite file — anything that can read the app's data dir can hijack the
//! Telegram account. This module replaces it with an in-memory `Session`
//! implementation that persists an encrypted snapshot
//! (ChaCha20-Poly1305) to `<account>.session.enc`. The master key comes
//! from a pluggable [`super::master_key::MasterKeyProvider`] (OS keychain
//! on desktop, env-injected on servers).
//!
//! Design notes:
//! * The `Session` trait is synchronous and called on hot paths
//!   (`set_update_state` fires for every received update), so writes are
//!   throttled: critical changes (auth keys / home DC) flush immediately,
//!   everything else marks the state dirty and is flushed at most every
//!   couple of seconds — plus explicitly via [`EncryptedSession::flush`].
//!   Losing the last seconds of `pts/qts` on a crash is safe (grammers
//!   catches up); losing an auth key is not, hence the split.
//! * `peer(PeerId::self_user())` must resolve to the cached `is_self` user —
//!   grammers uses it to detect "already logged in" (SqliteSession does the
//!   same via a subtype bit; `MemorySession` notably does NOT).
//! * Migration from a legacy plaintext `SqliteSession` goes through
//!   `SessionData::from`, which keeps the auth keys, the self peer and the
//!   updates state. The plaintext file is deleted only after the encrypted
//!   snapshot has been written successfully.

use std::collections::HashMap;
use std::net::{SocketAddrV4, SocketAddrV6};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use grammers_session::defs::{
    ChannelKind, ChannelState, DcOption, PeerAuth, PeerId, PeerInfo, UpdateState, UpdatesState,
};
use grammers_session::{Session, SessionData};
use rand::RngCore;
use serde::{Deserialize, Serialize};

const SAVE_THROTTLE: Duration = Duration::from_secs(2);
const NONCE_LEN: usize = 12;
const FORMAT_VERSION: u32 = 1;

// --- Stored (serde) representation -----------------------------------------

#[derive(Serialize, Deserialize)]
struct StoredSession {
    version: u32,
    home_dc: i32,
    dcs: Vec<StoredDc>,
    peers: Vec<StoredPeer>,
    updates: StoredUpdates,
}

#[derive(Serialize, Deserialize)]
struct StoredDc {
    id: i32,
    ipv4: String,
    ipv6: String,
    auth_key: Option<Vec<u8>>,
}

#[derive(Serialize, Deserialize)]
enum StoredPeer {
    User {
        id: i64,
        auth: Option<i64>,
        bot: Option<bool>,
        is_self: Option<bool>,
    },
    Chat {
        id: i64,
    },
    Channel {
        id: i64,
        auth: Option<i64>,
        kind: Option<u8>,
    },
}

#[derive(Serialize, Deserialize, Default)]
struct StoredUpdates {
    pts: i32,
    qts: i32,
    date: i32,
    seq: i32,
    channels: Vec<(i64, i32)>,
}

fn peer_to_stored(peer: &PeerInfo) -> StoredPeer {
    match peer {
        PeerInfo::User {
            id,
            auth,
            bot,
            is_self,
        } => StoredPeer::User {
            id: *id,
            auth: auth.map(|a| a.hash()),
            bot: *bot,
            is_self: *is_self,
        },
        PeerInfo::Chat { id } => StoredPeer::Chat { id: *id },
        PeerInfo::Channel { id, auth, kind } => StoredPeer::Channel {
            id: *id,
            auth: auth.map(|a| a.hash()),
            kind: kind.map(|k| match k {
                ChannelKind::Megagroup => 0,
                ChannelKind::Broadcast => 1,
                ChannelKind::Gigagroup => 2,
            }),
        },
    }
}

fn stored_to_peer(peer: &StoredPeer) -> PeerInfo {
    match peer {
        StoredPeer::User {
            id,
            auth,
            bot,
            is_self,
        } => PeerInfo::User {
            id: *id,
            auth: auth.map(PeerAuth::from_hash),
            bot: *bot,
            is_self: *is_self,
        },
        StoredPeer::Chat { id } => PeerInfo::Chat { id: *id },
        StoredPeer::Channel { id, auth, kind } => PeerInfo::Channel {
            id: *id,
            auth: auth.map(PeerAuth::from_hash),
            kind: kind.and_then(|k| match k {
                0 => Some(ChannelKind::Megagroup),
                1 => Some(ChannelKind::Broadcast),
                2 => Some(ChannelKind::Gigagroup),
                _ => None,
            }),
        },
    }
}

// --- In-memory state ---------------------------------------------------------

struct State {
    home_dc: i32,
    dc_options: HashMap<i32, DcOption>,
    peers: HashMap<PeerId, PeerInfo>,
    updates: UpdatesState,
    dirty: bool,
    persistence_disabled: bool,
    last_save: Instant,
}

pub struct EncryptedSession {
    state: Mutex<State>,
    path: PathBuf,
    cipher: ChaCha20Poly1305,
}

impl EncryptedSession {
    /// Creates a session backed by `path`, seeding it from `data`
    /// and writing the first encrypted snapshot right away.
    pub fn create(path: &Path, key: &[u8; 32], data: SessionData) -> Result<Self> {
        let this = Self::from_data(path, key, data);
        this.save_now()
            .context("Failed to write initial encrypted session")?;
        Ok(this)
    }

    /// Loads an existing encrypted session file.
    pub fn load(path: &Path, key: &[u8; 32]) -> Result<Self> {
        let blob = std::fs::read(path).context("Failed to read encrypted session")?;
        if blob.len() <= NONCE_LEN {
            return Err(anyhow!("Encrypted session file is truncated"));
        }
        let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
        let (nonce, ciphertext) = blob.split_at(NONCE_LEN);
        let plaintext = cipher
            .decrypt(Nonce::from_slice(nonce), ciphertext)
            .map_err(|_| anyhow!("Failed to decrypt session (wrong or missing key?)"))?;
        let stored: StoredSession =
            serde_json::from_slice(&plaintext).context("Failed to parse decrypted session")?;
        if stored.version != FORMAT_VERSION {
            return Err(anyhow!(
                "Unsupported session format version {}",
                stored.version
            ));
        }

        // Start from defaults so statically-known DC options are present,
        // then overlay everything from the snapshot.
        let mut data = SessionData::default();
        data.home_dc = stored.home_dc;
        for dc in &stored.dcs {
            let parsed = (|| -> Option<DcOption> {
                Some(DcOption {
                    id: dc.id,
                    ipv4: dc.ipv4.parse::<SocketAddrV4>().ok()?,
                    ipv6: dc.ipv6.parse::<SocketAddrV6>().ok()?,
                    auth_key: dc
                        .auth_key
                        .as_ref()
                        .and_then(|k| <[u8; 256]>::try_from(k.as_slice()).ok()),
                })
            })();
            if let Some(option) = parsed {
                data.dc_options.insert(option.id, option);
            }
        }
        for peer in &stored.peers {
            let info = stored_to_peer(peer);
            data.peer_infos.insert(info.id(), info);
        }
        data.updates_state = UpdatesState {
            pts: stored.updates.pts,
            qts: stored.updates.qts,
            date: stored.updates.date,
            seq: stored.updates.seq,
            channels: stored
                .updates
                .channels
                .iter()
                .map(|(id, pts)| ChannelState { id: *id, pts: *pts })
                .collect(),
        };

        Ok(Self::from_data(path, key, data))
    }

    fn from_data(path: &Path, key: &[u8; 32], data: SessionData) -> Self {
        Self {
            state: Mutex::new(State {
                home_dc: data.home_dc,
                dc_options: data.dc_options,
                peers: data.peer_infos,
                updates: data.updates_state,
                dirty: false,
                persistence_disabled: false,
                last_save: Instant::now(),
            }),
            path: path.to_path_buf(),
            cipher: ChaCha20Poly1305::new(Key::from_slice(key)),
        }
    }

    /// Serializes + encrypts the current state and writes it atomically.
    fn save_locked(&self, state: &mut State) -> Result<()> {
        if state.persistence_disabled {
            return Ok(());
        }
        let stored = StoredSession {
            version: FORMAT_VERSION,
            home_dc: state.home_dc,
            dcs: state
                .dc_options
                .values()
                .map(|dc| StoredDc {
                    id: dc.id,
                    ipv4: dc.ipv4.to_string(),
                    ipv6: dc.ipv6.to_string(),
                    auth_key: dc.auth_key.map(|k| k.to_vec()),
                })
                .collect(),
            peers: state.peers.values().map(peer_to_stored).collect(),
            updates: StoredUpdates {
                pts: state.updates.pts,
                qts: state.updates.qts,
                date: state.updates.date,
                seq: state.updates.seq,
                channels: state
                    .updates
                    .channels
                    .iter()
                    .map(|c| (c.id, c.pts))
                    .collect(),
            },
        };

        let plaintext = serde_json::to_vec(&stored).context("Failed to serialize session")?;
        let mut nonce = [0u8; NONCE_LEN];
        rand::rngs::OsRng.fill_bytes(&mut nonce);
        let ciphertext = self
            .cipher
            .encrypt(Nonce::from_slice(&nonce), plaintext.as_slice())
            .map_err(|_| anyhow!("Failed to encrypt session"))?;

        let mut blob = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        blob.extend_from_slice(&nonce);
        blob.extend_from_slice(&ciphertext);

        // Atomic replace so a crash mid-write can't corrupt the only copy.
        let tmp = self.path.with_extension("enc.tmp");
        std::fs::write(&tmp, &blob).context("Failed to write session temp file")?;
        std::fs::rename(&tmp, &self.path).context("Failed to move session file in place")?;

        state.dirty = false;
        state.last_save = Instant::now();
        Ok(())
    }

    fn save_now(&self) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        self.save_locked(&mut state)
    }

    /// Mark dirty and save if enough time has passed since the last write.
    fn lazy_save(&self, state: &mut State) {
        state.dirty = true;
        if state.last_save.elapsed() >= SAVE_THROTTLE {
            if let Err(e) = self.save_locked(state) {
                tracing::error!(error = %e, path = ?self.path, "Failed to persist session");
            }
        }
    }

    /// Persist immediately — critical data (auth keys) must never be lost.
    fn eager_save(&self, state: &mut State) {
        state.dirty = true;
        if let Err(e) = self.save_locked(state) {
            tracing::error!(error = %e, path = ?self.path, "Failed to persist session");
        }
    }

    /// Irreversibly stop this instance from writing after logout. SenderPool and
    /// in-flight operations may retain Arc clones after the manager removes it.
    /// The state mutex fences an already-running save before the caller unlinks.
    pub fn disable_persistence(&self) {
        let mut state = self.state.lock().unwrap();
        state.persistence_disabled = true;
        state.dirty = false;
    }

    /// Flush pending changes to disk (call on shutdown/disconnect).
    pub fn flush(&self) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.dirty {
            self.save_locked(&mut state)?;
        }
        Ok(())
    }
}

impl Drop for EncryptedSession {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

impl Session for EncryptedSession {
    fn home_dc_id(&self) -> i32 {
        self.state.lock().unwrap().home_dc
    }

    fn set_home_dc_id(&self, dc_id: i32) {
        let mut state = self.state.lock().unwrap();
        state.home_dc = dc_id;
        self.eager_save(&mut state);
    }

    fn dc_option(&self, dc_id: i32) -> Option<DcOption> {
        self.state.lock().unwrap().dc_options.get(&dc_id).cloned()
    }

    fn set_dc_option(&self, dc_option: &DcOption) {
        let mut state = self.state.lock().unwrap();
        state.dc_options.insert(dc_option.id, dc_option.clone());
        // Auth keys live here — losing one would log the account out.
        self.eager_save(&mut state);
    }

    fn peer(&self, peer: PeerId) -> Option<PeerInfo> {
        let state = self.state.lock().unwrap();
        if peer == PeerId::self_user() {
            // The self sentinel never matches a real key: find the cached
            // user flagged is_self (mirrors SqliteSession's subtype lookup).
            return state
                .peers
                .values()
                .find(|p| {
                    matches!(
                        p,
                        PeerInfo::User {
                            is_self: Some(true),
                            ..
                        }
                    )
                })
                .cloned();
        }
        state.peers.get(&peer).cloned()
    }

    fn cache_peer(&self, peer: &PeerInfo) {
        let mut state = self.state.lock().unwrap();
        let is_self = matches!(
            peer,
            PeerInfo::User {
                is_self: Some(true),
                ..
            }
        );
        state.peers.insert(peer.id(), peer.clone());
        if is_self {
            // Knowing "who am I" is what marks the session as logged-in.
            self.eager_save(&mut state);
        } else {
            self.lazy_save(&mut state);
        }
    }

    fn updates_state(&self) -> UpdatesState {
        self.state.lock().unwrap().updates.clone()
    }

    fn set_update_state(&self, update: UpdateState) {
        let mut state = self.state.lock().unwrap();
        match update {
            UpdateState::All(updates_state) => {
                state.updates = updates_state;
            }
            UpdateState::Primary { pts, date, seq } => {
                state.updates.pts = pts;
                state.updates.date = date;
                state.updates.seq = seq;
            }
            UpdateState::Secondary { qts } => {
                state.updates.qts = qts;
            }
            UpdateState::Channel { id, pts } => {
                state.updates.channels.retain(|c| c.id != id);
                state.updates.channels.push(ChannelState { id, pts });
            }
        }
        self.lazy_save(&mut state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> [u8; 32] {
        [7u8; 32]
    }

    #[test]
    fn roundtrip_state_through_disk() {
        let dir = std::env::temp_dir().join(format!("enc-session-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("acc.session.enc");

        let session = EncryptedSession::create(&path, &test_key(), SessionData::default()).unwrap();
        session.set_home_dc_id(4);
        session.cache_peer(&PeerInfo::User {
            id: 12345,
            auth: Some(PeerAuth::from_hash(777)),
            bot: Some(false),
            is_self: Some(true),
        });
        session.set_update_state(UpdateState::Primary {
            pts: 10,
            date: 20,
            seq: 30,
        });
        session.flush().unwrap();
        drop(session);

        let restored = EncryptedSession::load(&path, &test_key()).unwrap();
        assert_eq!(restored.home_dc_id(), 4);
        let me = restored
            .peer(PeerId::self_user())
            .expect("self peer survives restart");
        assert!(matches!(me, PeerInfo::User { id: 12345, .. }));
        let updates = restored.updates_state();
        assert_eq!((updates.pts, updates.date, updates.seq), (10, 20, 30));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn logged_out_session_cannot_be_recreated_by_late_save_or_drop() {
        let dir = std::env::temp_dir().join(format!("enc-session-logout-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("acc.session.enc");
        let session = std::sync::Arc::new(
            EncryptedSession::create(&path, &test_key(), SessionData::default()).unwrap(),
        );
        let late = session.clone();
        session.set_update_state(UpdateState::Primary {
            pts: 10,
            date: 20,
            seq: 30,
        });
        session.disable_persistence();
        std::fs::remove_file(&path).unwrap();
        drop(session);
        late.set_home_dc_id(4); // eager save after logout must not recreate the file
        late.set_update_state(UpdateState::Primary {
            pts: 20,
            date: 30,
            seq: 40,
        });
        late.flush().unwrap();
        assert!(!path.exists());
        drop(late);
        assert!(
            !path.exists(),
            "last Arc drop recreated logged-out credentials"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn wrong_key_fails_closed() {
        let dir = std::env::temp_dir().join(format!("enc-session-test2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("acc.session.enc");

        EncryptedSession::create(&path, &test_key(), SessionData::default()).unwrap();
        assert!(EncryptedSession::load(&path, &[9u8; 32]).is_err());

        std::fs::remove_dir_all(&dir).ok();
    }
}
