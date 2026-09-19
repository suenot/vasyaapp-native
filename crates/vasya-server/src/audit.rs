//! Audit log: who (user / agent key), what (method + path), when, and the
//! outcome of every mutating call. Append-only JSONL in the data dir.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditEntry {
    /// Epoch milliseconds.
    pub ts: i64,
    pub user_id: String,
    /// Set when the caller authenticated with an agent key.
    pub agent_key_id: Option<String>,
    pub method: String,
    /// Full request path — carries the target (account/chat) ids.
    pub path: String,
    pub status: u16,
}

pub struct AuditLog {
    path: PathBuf,
    file: Mutex<std::fs::File>,
}

impl AuditLog {
    pub fn open(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .context("failed to open audit log")?;
        Ok(Self {
            path,
            file: Mutex::new(file),
        })
    }

    pub fn record(&self, entry: &AuditEntry) {
        let Ok(line) = serde_json::to_string(entry) else {
            return;
        };
        let mut file = self.file.lock().unwrap();
        if let Err(e) = writeln!(file, "{line}") {
            tracing::error!(error = %e, "Failed to write audit entry");
        }
    }

    /// Read the whole log (reads the whole file — fine for the file-backed
    /// phase; a database store would query instead). Takes the lock so
    /// concurrent writes flush before we read.
    fn read_all(&self) -> Result<Vec<AuditEntry>, crate::error::ApiError> {
        let _guard = self.file.lock().unwrap();
        match std::fs::read_to_string(&self.path) {
            Ok(raw) => Ok(raw
                .lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(crate::error::ApiError::internal(e)),
        }
    }

    /// The most recent `limit` entries across all users. Internal — never serve
    /// this to a request without an admin gate (no admin role exists yet, so
    /// HTTP callers must use `recent_for`).
    pub fn recent(&self, limit: usize) -> Result<Vec<AuditEntry>, crate::error::ApiError> {
        let mut entries = self.read_all()?;
        if entries.len() > limit {
            entries.drain(..entries.len() - limit);
        }
        Ok(entries)
    }

    /// The most recent `limit` entries belonging to `user_id`. Per-user
    /// isolation: a caller only ever sees their own audit trail (an agent
    /// key's owner is its `user_id`, so this also scopes agent activity).
    pub fn recent_for(
        &self,
        user_id: &str,
        limit: usize,
    ) -> Result<Vec<AuditEntry>, crate::error::ApiError> {
        let mut entries: Vec<AuditEntry> = self
            .read_all()?
            .into_iter()
            .filter(|e| e.user_id == user_id)
            .collect();
        if entries.len() > limit {
            entries.drain(..entries.len() - limit);
        }
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let log = AuditLog::open(dir.path().join("audit.log")).unwrap();

        log.record(&AuditEntry {
            ts: 1,
            user_id: "alice".into(),
            agent_key_id: Some("ak01".into()),
            method: "POST".into(),
            path: "/api/v1/accounts/acc/chats/5/messages".into(),
            status: 200,
        });
        log.record(&AuditEntry {
            ts: 2,
            user_id: "alice".into(),
            agent_key_id: None,
            method: "DELETE".into(),
            path: "/api/v1/accounts/acc/folders/f1".into(),
            status: 204,
        });

        let entries = log.recent(10).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].agent_key_id.as_deref(), Some("ak01"));
        assert_eq!(entries[1].method, "DELETE");

        // limit keeps the most recent entries
        let entries = log.recent(1).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].ts, 2);
    }

    #[test]
    fn recent_for_isolates_by_user() {
        let dir = tempfile::tempdir().unwrap();
        let log = AuditLog::open(dir.path().join("audit.log")).unwrap();

        let mk = |ts: i64, user: &str| AuditEntry {
            ts,
            user_id: user.into(),
            agent_key_id: None,
            method: "POST".into(),
            path: "/api/v1/accounts/secret/chats/9/messages".into(),
            status: 200,
        };
        log.record(&mk(1, "alice"));
        log.record(&mk(2, "bob"));
        log.record(&mk(3, "alice"));

        // alice sees only her two rows, never bob's (which would leak bob's path/ids)
        let alice = log.recent_for("alice", 100).unwrap();
        assert_eq!(alice.len(), 2);
        assert!(alice.iter().all(|e| e.user_id == "alice"));

        let bob = log.recent_for("bob", 100).unwrap();
        assert_eq!(bob.len(), 1);
        assert_eq!(bob[0].ts, 2);

        // a user with no activity gets an empty trail, not everyone's
        assert!(log.recent_for("carol", 100).unwrap().is_empty());
    }
}
