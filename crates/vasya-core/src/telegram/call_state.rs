//! Call state management

use super::dh::{DhConfig, DhExchange};
use serde::Serialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum CallState {
    Requesting,
    Waiting,
    Ringing,
    Accepted,
    Active,
    Discarded,
    Error,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CallInfoResponse {
    pub call_id: i64,
    pub access_hash: i64,
    pub peer_user_id: i64,
    pub is_outgoing: bool,
    pub is_video: bool,
    pub state: CallState,
}

/// Internal call info (not serialized directly to frontend)
#[derive(Debug)]
pub struct CallInfo {
    pub call_id: i64,
    pub access_hash: i64,
    pub peer_user_id: i64,
    pub is_outgoing: bool,
    pub is_video: bool,
    pub state: CallState,
    pub dh_exchange: Option<DhExchange>,
    pub shared_key: Option<Vec<u8>>,
    pub key_fingerprint: Option<i64>,
    pub account_id: String,
}

impl CallInfo {
    pub fn to_response(&self) -> CallInfoResponse {
        CallInfoResponse {
            call_id: self.call_id,
            access_hash: self.access_hash,
            peer_user_id: self.peer_user_id,
            is_outgoing: self.is_outgoing,
            is_video: self.is_video,
            state: self.state.clone(),
        }
    }
}

/// Manages all active calls
#[derive(Debug, Default)]
pub struct ActiveCalls {
    pub calls: HashMap<i64, CallInfo>,
    pub dh_config: Option<DhConfig>,
}
