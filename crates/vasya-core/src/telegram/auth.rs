//! Telegram authentication types
//!
//! Types used across authentication commands and client management.

use serde::{Deserialize, Serialize};

/// User information after successful authentication
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserInfo {
    pub id: i64,
    pub first_name: String,
    pub last_name: Option<String>,
    pub username: Option<String>,
    pub phone: String,
}

/// Authentication token for multi-step auth process
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthToken {
    pub token_data: String,
    pub phone: String,
}
