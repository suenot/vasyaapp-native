//! Group call state management

use serde::Serialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum GroupCallState {
    Idle,
    Creating,
    Joining,
    Active,
    Leaving,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupCallParticipant {
    pub user_id: i64,
    pub name: Option<String>,
    pub is_muted: bool,
    pub is_self: bool,
    pub is_speaking: bool,
    pub volume: Option<i32>,
    pub can_self_unmute: bool,
    pub video_joined: bool,
    pub about: Option<String>,
    pub raise_hand_rating: Option<i64>,
    pub source: i32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupCallInfoResponse {
    pub call_id: i64,
    pub access_hash: i64,
    pub chat_id: i64,
    pub state: GroupCallState,
    pub title: Option<String>,
    pub participants_count: i32,
    pub can_start_video: bool,
}

/// Internal group call info
#[derive(Debug)]
pub struct GroupCallInfo {
    pub call_id: i64,
    pub access_hash: i64,
    pub chat_id: i64,
    pub state: GroupCallState,
    pub title: Option<String>,
    pub participants: HashMap<i64, GroupCallParticipant>,
    pub source: Option<i32>, // Our SSRC
    pub account_id: String,
}

impl GroupCallInfo {
    pub fn to_response(&self) -> GroupCallInfoResponse {
        GroupCallInfoResponse {
            call_id: self.call_id,
            access_hash: self.access_hash,
            chat_id: self.chat_id,
            state: self.state.clone(),
            title: self.title.clone(),
            participants_count: self.participants.len() as i32,
            can_start_video: true,
        }
    }
}

/// Active group calls manager
#[derive(Debug, Default)]
pub struct ActiveGroupCalls {
    pub calls: HashMap<i64, GroupCallInfo>,
}
