//! Telegram module for handling Telegram API interactions

pub mod auth;
pub mod call_state;
pub mod calls;
pub mod client_manager;
pub mod dh;
pub mod encrypted_session;
pub mod group_call_state;
pub mod group_calls;
pub mod master_key;
pub mod peer;
pub mod updates;

pub use client_manager::TelegramClientManager;
