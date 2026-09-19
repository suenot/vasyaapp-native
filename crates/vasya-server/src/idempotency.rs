//! Idempotency-Key support for mutating REST routes (plan §4.4: agents
//! must be able to retry safely).
//!
//! Keyed by (user, Idempotency-Key, method, path). First request executes
//! and the response (status < 500) is cached for the TTL; replays return
//! the cached response with an `Idempotency-Replayed: true` header. A
//! concurrent duplicate while the first is in flight gets 409.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Clone)]
pub struct StoredResponse {
    pub status: u16,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

enum Entry {
    InFlight,
    Done {
        response: StoredResponse,
        stored_at: Instant,
    },
}

pub struct IdempotencyStore {
    ttl: Duration,
    entries: Mutex<HashMap<String, Entry>>,
}

pub enum Begin {
    /// Execute the request; call `complete` (or `abandon`) afterwards.
    Execute,
    /// A previous identical request finished — replay this response.
    Replay(StoredResponse),
    /// An identical request is currently in flight.
    InFlight,
}

impl IdempotencyStore {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            entries: Mutex::new(HashMap::new()),
        }
    }

    fn purge_expired(&self, entries: &mut HashMap<String, Entry>) {
        let ttl = self.ttl;
        entries.retain(|_, entry| match entry {
            Entry::InFlight => true,
            Entry::Done { stored_at, .. } => stored_at.elapsed() < ttl,
        });
    }

    pub fn begin(&self, key: &str) -> Begin {
        let mut entries = self.entries.lock().unwrap();
        self.purge_expired(&mut entries);
        match entries.get(key) {
            Some(Entry::InFlight) => Begin::InFlight,
            Some(Entry::Done { response, .. }) => Begin::Replay(response.clone()),
            None => {
                entries.insert(key.to_string(), Entry::InFlight);
                Begin::Execute
            }
        }
    }

    pub fn complete(&self, key: &str, response: StoredResponse) {
        let mut entries = self.entries.lock().unwrap();
        entries.insert(
            key.to_string(),
            Entry::Done {
                response,
                stored_at: Instant::now(),
            },
        );
    }

    /// Drop the in-flight marker without caching (5xx responses or panics
    /// should not pin a failure as "the" result).
    pub fn abandon(&self, key: &str) {
        let mut entries = self.entries.lock().unwrap();
        if matches!(entries.get(key), Some(Entry::InFlight)) {
            entries.remove(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resp(status: u16) -> StoredResponse {
        StoredResponse {
            status,
            content_type: None,
            body: b"ok".to_vec(),
        }
    }

    #[test]
    fn execute_then_replay() {
        let store = IdempotencyStore::new(Duration::from_secs(60));
        assert!(matches!(store.begin("k"), Begin::Execute));
        // duplicate while in flight
        assert!(matches!(store.begin("k"), Begin::InFlight));
        store.complete("k", resp(201));
        match store.begin("k") {
            Begin::Replay(r) => assert_eq!(r.status, 201),
            _ => panic!("expected replay"),
        }
    }

    #[test]
    fn abandon_allows_retry() {
        let store = IdempotencyStore::new(Duration::from_secs(60));
        assert!(matches!(store.begin("k"), Begin::Execute));
        store.abandon("k");
        assert!(matches!(store.begin("k"), Begin::Execute));
    }

    #[test]
    fn entries_expire() {
        let store = IdempotencyStore::new(Duration::from_millis(10));
        assert!(matches!(store.begin("k"), Begin::Execute));
        store.complete("k", resp(200));
        std::thread::sleep(Duration::from_millis(20));
        assert!(matches!(store.begin("k"), Begin::Execute));
    }
}
