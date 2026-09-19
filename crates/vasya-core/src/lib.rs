//! vasya-core — Tauri-free Telegram engine.
//!
//! Hosts the grammers client manager, real-time update handling, encrypted
//! at-rest session storage and call state. Consumed by two front doors:
//! the Tauri desktop/mobile app (events forwarded to the webview) and the
//! server session host (events fanned out to WebSocket/GraphQL subscribers).
//! Nothing in this crate may depend on `tauri`.

pub mod events;
pub mod media;
pub mod stt;
pub mod telegram;

pub use telegram::TelegramClientManager;
