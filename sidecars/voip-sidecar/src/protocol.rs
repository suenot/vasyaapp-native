//! IPC protocol types for communication between main app and VoIP sidecar

use serde::{Deserialize, Serialize};

/// Commands sent from main app to sidecar via stdin
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    /// Start a VoIP connection
    Start {
        /// Connection endpoints from phone.PhoneCall
        connections: Vec<Connection>,
        /// Shared encryption key (hex-encoded, 256 bytes)
        encryption_key: String,
        /// Whether we initiated the call
        is_outgoing: bool,
        /// Key fingerprint for verification
        key_fingerprint: i64,
    },
    /// Stop the call
    Stop,
    /// Mute/unmute microphone
    Mute {
        muted: bool,
    },
    /// Set playback volume
    SetVolume {
        volume: f32,
    },
    /// Forward signaling data from Telegram
    SignalingData {
        data: Vec<u8>,
    },
}

/// Events sent from sidecar to main app via stdout
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// Sidecar is ready to receive commands
    Ready,
    /// Attempting to connect
    Connecting,
    /// Connection established, audio flowing
    Connected,
    /// Call stopped
    Stopped {
        reason: String,
    },
    /// Audio input level (0.0 - 1.0)
    AudioLevel {
        level: f32,
    },
    /// Network quality indicator (1-5)
    NetworkQuality {
        quality: u8,
    },
    /// Mute state changed
    MuteChanged {
        muted: bool,
    },
    /// Volume changed
    VolumeChanged {
        volume: f32,
    },
    /// Signaling data to send back to Telegram
    SignalingDataOut {
        data: Vec<u8>,
    },
    /// Error occurred
    Error {
        message: String,
    },
    /// Debug: command was received
    CommandReceived {
        cmd: String,
    },
}

/// A connection endpoint from Telegram
#[derive(Debug, Clone, Deserialize)]
pub struct Connection {
    pub id: i64,
    pub ip: String,
    pub ipv6: String,
    pub port: i32,
    pub peer_tag: Vec<u8>,
}
