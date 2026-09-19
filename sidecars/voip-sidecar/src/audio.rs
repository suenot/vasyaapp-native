//! Audio capture and playback using cpal
//!
//! Captures microphone input, encodes to Opus, and plays back decoded audio.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Audio engine managing capture and playback
pub struct AudioEngine {
    muted: Arc<AtomicBool>,
    _capture_stream: Option<cpal::Stream>,
    _playback_stream: Option<cpal::Stream>,
}

impl AudioEngine {
    /// Create a new audio engine (does not start streams yet)
    pub fn new() -> Self {
        Self {
            muted: Arc::new(AtomicBool::new(false)),
            _capture_stream: None,
            _playback_stream: None,
        }
    }

    /// Start audio capture from default input device
    pub fn start_capture(&mut self, audio_tx: mpsc::UnboundedSender<Vec<f32>>) -> Result<(), String> {
        let host = cpal::default_host();
        let device = host.default_input_device()
            .ok_or("No input audio device available")?;

        tracing::info!("Using input device: {}", device.name().unwrap_or_default());

        let config = device.default_input_config()
            .map_err(|e| format!("Failed to get input config: {}", e))?;

        let muted = self.muted.clone();

        let stream = device.build_input_stream(
            &config.into(),
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                if !muted.load(Ordering::Relaxed) {
                    let _ = audio_tx.send(data.to_vec());
                }
            },
            move |err| {
                tracing::error!("Audio capture error: {}", err);
            },
            None,
        ).map_err(|e| format!("Failed to build input stream: {}", e))?;

        stream.play().map_err(|e| format!("Failed to start capture: {}", e))?;
        self._capture_stream = Some(stream);

        Ok(())
    }

    /// Start audio playback on default output device
    pub fn start_playback(&mut self, mut audio_rx: mpsc::UnboundedReceiver<Vec<f32>>) -> Result<(), String> {
        let host = cpal::default_host();
        let device = host.default_output_device()
            .ok_or("No output audio device available")?;

        tracing::info!("Using output device: {}", device.name().unwrap_or_default());

        let config = device.default_output_config()
            .map_err(|e| format!("Failed to get output config: {}", e))?;

        let buffer: Arc<std::sync::Mutex<Vec<f32>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let buffer_clone = buffer.clone();

        // Spawn task to receive audio data and buffer it
        tokio::spawn(async move {
            while let Some(samples) = audio_rx.recv().await {
                let mut buf = buffer_clone.lock().unwrap();
                buf.extend_from_slice(&samples);
                // Keep buffer from growing too large (max ~1 second at 48kHz)
                if buf.len() > 48000 {
                    let drain_count = buf.len() - 48000;
                    buf.drain(..drain_count);
                }
            }
        });

        let stream = device.build_output_stream(
            &config.into(),
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                let mut buf = buffer.lock().unwrap();
                for sample in data.iter_mut() {
                    *sample = buf.first().copied().unwrap_or(0.0);
                    if !buf.is_empty() {
                        buf.remove(0);
                    }
                }
            },
            move |err| {
                tracing::error!("Audio playback error: {}", err);
            },
            None,
        ).map_err(|e| format!("Failed to build output stream: {}", e))?;

        stream.play().map_err(|e| format!("Failed to start playback: {}", e))?;
        self._playback_stream = Some(stream);

        Ok(())
    }

    /// Set mute state
    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }

    /// Check if muted
    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }
}

/// List available audio devices
pub fn list_devices() -> Vec<(String, String)> {
    let host = cpal::default_host();
    let mut devices = Vec::new();

    if let Ok(input_devices) = host.input_devices() {
        for device in input_devices {
            if let Ok(name) = device.name() {
                devices.push(("input".to_string(), name));
            }
        }
    }

    if let Ok(output_devices) = host.output_devices() {
        for device in output_devices {
            if let Ok(name) = device.name() {
                devices.push(("output".to_string(), name));
            }
        }
    }

    devices
}
