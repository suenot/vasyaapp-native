//! Account ownership: which user owns which telegram account.
//!
//! Per-user isolation is mandatory — every /accounts/{acc}/... route checks
//! ownership here. Semantics mirror the sync backend's `ensure_account_access`:
//! an unowned account is claimed by the first user to touch it
//! (self-provisioning), an account owned by someone else is a 403.
//!
//! Persistence is a JSON file in the data dir so embedded-local mode needs
//! no database. A Postgres-backed implementation can replace this for large
//! multi-user deployments (task #7 adds agent keys alongside).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result};

use crate::error::ApiError;

pub struct AccountStore {
    path: PathBuf,
    owners: Mutex<HashMap<String, String>>, // account_id -> user_id
}

impl AccountStore {
    /// Loads the store from `path`, starting empty when the file is absent.
    pub fn open(path: PathBuf) -> Result<Self> {
        let owners = match std::fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str(&raw).context("accounts file is malformed")?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => return Err(e).context("failed to read accounts file"),
        };
        Ok(Self {
            path,
            owners: Mutex::new(owners),
        })
    }

    fn persist(&self, owners: &HashMap<String, String>) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(owners)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Claim-on-first-touch ownership check.
    pub fn ensure_access(&self, user_id: &str, account_id: &str) -> Result<(), ApiError> {
        let mut owners = self.owners.lock().unwrap();
        match owners.get(account_id) {
            Some(owner) if owner == user_id => Ok(()),
            Some(_) => Err(ApiError::Forbidden(
                "This account belongs to another user".into(),
            )),
            None => {
                owners.insert(account_id.to_string(), user_id.to_string());
                self.persist(&owners).map_err(ApiError::internal)?;
                Ok(())
            }
        }
    }

    /// Non-claiming read-only ownership check (event filtering).
    pub fn is_owner(&self, user_id: &str, account_id: &str) -> bool {
        let owners = self.owners.lock().unwrap();
        owners.get(account_id).map(String::as_str) == Some(user_id)
    }

    /// Accounts owned by `user_id`.
    pub fn list_for(&self, user_id: &str) -> Vec<String> {
        let owners = self.owners.lock().unwrap();
        let mut ids: Vec<String> = owners
            .iter()
            .filter(|(_, owner)| owner.as_str() == user_id)
            .map(|(acc, _)| acc.clone())
            .collect();
        ids.sort();
        ids
    }

    /// Drop ownership (after logout/account removal).
    pub fn release(&self, account_id: &str) -> Result<(), ApiError> {
        let mut owners = self.owners.lock().unwrap();
        if owners.remove(account_id).is_some() {
            self.persist(&owners).map_err(ApiError::internal)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (tempfile::TempDir, AccountStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = AccountStore::open(dir.path().join("accounts.json")).unwrap();
        (dir, store)
    }

    #[test]
    fn claim_on_first_touch_then_isolate() {
        let (_dir, store) = temp_store();
        store.ensure_access("alice", "acc-1").unwrap();
        // Owner keeps access, others are rejected.
        store.ensure_access("alice", "acc-1").unwrap();
        assert!(matches!(
            store.ensure_access("bob", "acc-1"),
            Err(ApiError::Forbidden(_))
        ));
        assert_eq!(store.list_for("alice"), vec!["acc-1"]);
        assert!(store.list_for("bob").is_empty());
    }

    #[test]
    fn persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("accounts.json");
        {
            let store = AccountStore::open(path.clone()).unwrap();
            store.ensure_access("alice", "acc-1").unwrap();
        }
        let reopened = AccountStore::open(path).unwrap();
        assert!(matches!(
            reopened.ensure_access("bob", "acc-1"),
            Err(ApiError::Forbidden(_))
        ));
    }

    #[test]
    fn release_makes_account_claimable_again() {
        let (_dir, store) = temp_store();
        store.ensure_access("alice", "acc-1").unwrap();
        store.release("acc-1").unwrap();
        store.ensure_access("bob", "acc-1").unwrap();
    }
}
