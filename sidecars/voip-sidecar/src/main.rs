//! VoIP sidecar for Telegram voice/video calls
//!
//! Communicates with the main Tauri process via stdin/stdout JSON-line protocol.

mod audio;
mod transport;
mod encryption;
mod protocol;

use std::io::{BufRead, BufReader};
use tokio::sync::mpsc;
use protocol::{Command, Event};

#[tokio::main]
async fn main() {
    // Initialize logging to stderr (stdout is for IPC)
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter("voip_sidecar=debug")
        .init();

    tracing::info!("VoIP sidecar started");
    emit_event(&Event::Ready);

    // Channel for sending events to stdout
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<Event>();

    // Spawn event writer (stdout)
    let writer_handle = tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
            let json = serde_json::to_string(&event).unwrap_or_default();
            println!("{}", json);
        }
    });

    // Read commands from stdin (blocking, in a spawn_blocking)
    let event_tx_clone = event_tx.clone();
    let stdin_handle = tokio::task::spawn_blocking(move || {
        let stdin = std::io::stdin();
        let reader = BufReader::new(stdin.lock());

        for line in reader.lines() {
            match line {
                Ok(line) => {
                    let line = line.trim().to_string();
                    if line.is_empty() {
                        continue;
                    }

                    match serde_json::from_str::<Command>(&line) {
                        Ok(cmd) => {
                            tracing::debug!("Received command: {:?}", cmd);
                            if event_tx_clone.send(Event::CommandReceived {
                                cmd: format!("{:?}", cmd)
                            }).is_err() {
                                break;
                            }

                            match cmd {
                                Command::Start { .. } => {
                                    // TODO: Phase 2 - Start WebRTC connection
                                    let _ = event_tx_clone.send(Event::Connecting);
                                    // For now, simulate connection
                                    let _ = event_tx_clone.send(Event::Connected);
                                }
                                Command::Stop => {
                                    let _ = event_tx_clone.send(Event::Stopped { reason: "user_request".to_string() });
                                    break;
                                }
                                Command::Mute { muted } => {
                                    let _ = event_tx_clone.send(Event::MuteChanged { muted });
                                }
                                Command::SetVolume { volume } => {
                                    let _ = event_tx_clone.send(Event::VolumeChanged { volume });
                                }
                                Command::SignalingData { data } => {
                                    tracing::debug!("Received signaling data: {} bytes", data.len());
                                    // TODO: Forward to WebRTC transport
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!("Failed to parse command: {}", e);
                            let _ = event_tx_clone.send(Event::Error {
                                message: format!("Invalid command: {}", e)
                            });
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("stdin read error: {}", e);
                    break;
                }
            }
        }

        tracing::info!("stdin closed, shutting down");
    });

    // Wait for stdin to close (main process terminated or sent Stop)
    let _ = stdin_handle.await;
    drop(event_tx);
    let _ = writer_handle.await;

    tracing::info!("VoIP sidecar exiting");
}

fn emit_event(event: &Event) {
    let json = serde_json::to_string(event).unwrap_or_default();
    println!("{}", json);
}
