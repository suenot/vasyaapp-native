//! STT Sidecar: Local Whisper.cpp transcription
//!
//! CLI tool that loads a whisper model and transcribes audio files.
//! Outputs JSON to stdout for easy parsing by the main app.
//! Outputs progress events to stderr as JSON lines for real-time UI updates.

use clap::Parser;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

#[derive(Parser, Debug)]
#[command(name = "stt-sidecar")]
#[command(about = "Local Whisper.cpp STT sidecar")]
struct Args {
    /// Path to the GGML model file (e.g., ggml-small.bin)
    #[arg(long)]
    model: PathBuf,

    /// Path to the audio file to transcribe (WAV/MP3/OGG/etc.)
    #[arg(long)]
    input: PathBuf,

    /// Language code (ru, en, uk, etc.) or "auto"
    #[arg(long, default_value = "auto")]
    language: String,
}

#[derive(Serialize, Deserialize)]
struct Output {
    text: String,
    language: Option<String>,
}

/// Progress event emitted to stderr as a JSON line
#[derive(Serialize)]
struct ProgressEvent {
    event: &'static str,
    detail: Option<String>,
}

fn emit_progress(event: &'static str, detail: Option<String>) {
    let ev = ProgressEvent { event, detail };
    if let Ok(json) = serde_json::to_string(&ev) {
        eprintln!("{}", json);
    }
}

fn main() {
    let args = Args::parse();

    match run_transcription(&args) {
        Ok(output) => {
            println!("{}", serde_json::to_string(&output).unwrap());
            std::process::exit(0);
        }
        Err(err) => {
            eprintln!("Error: {}", err);
            std::process::exit(1);
        }
    }
}

fn run_transcription(args: &Args) -> Result<Output, String> {
    // Load whisper model
    emit_progress(
        "loading_model",
        Some(args.model.to_string_lossy().to_string()),
    );

    let ctx = WhisperContext::new_with_params(
        &args.model.to_string_lossy(),
        WhisperContextParameters::default(),
    )
    .map_err(|e| format!("Failed to load whisper model: {}", e))?;

    emit_progress("model_loaded", None);

    // Convert audio to 16kHz mono PCM f32 (whisper requirement)
    emit_progress(
        "converting_audio",
        Some(args.input.to_string_lossy().to_string()),
    );

    let audio_data = convert_audio_to_pcm(&args.input)?;

    emit_progress("audio_ready", Some(format!("{} samples", audio_data.len())));

    // Set up transcription parameters
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });

    // Set language (if not auto)
    if args.language != "auto" && args.language != "multi" {
        params.set_language(Some(&args.language));
    }

    params.set_translate(false);
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);

    // Create state
    let mut state = ctx
        .create_state()
        .map_err(|e| format!("Failed to create whisper state: {}", e))?;

    // Run transcription
    emit_progress("transcribing", None);

    state
        .full(params, &audio_data)
        .map_err(|e| format!("Transcription failed: {}", e))?;

    emit_progress("extracting_text", None);

    // Extract text
    let num_segments = state
        .full_n_segments()
        .map_err(|e| format!("Failed to get segments: {}", e))?;

    let mut full_text = String::new();
    for i in 0..num_segments {
        let segment = state
            .full_get_segment_text(i)
            .map_err(|e| format!("Failed to get segment {}: {}", i, e))?;
        full_text.push_str(&segment);
        full_text.push(' ');
    }

    let text = full_text.trim().to_string();

    // Detect language (whisper auto-detects)
    let detected_lang = if args.language == "auto" || args.language == "multi" {
        None
    } else {
        Some(args.language.clone())
    };

    emit_progress("done", None);

    Ok(Output {
        text,
        language: detected_lang,
    })
}

/// Resolve shipped conversion helper first, then PATH and common package-manager paths.
fn find_ffmpeg() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok();
    let mut candidates = executable
        .as_ref()
        .and_then(|p| p.parent())
        .map(|p| vec![p.join("ffmpeg"), p.join("../Resources/ffmpeg")])
        .unwrap_or_default();
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|p| p.join("ffmpeg")));
    }
    candidates.extend(
        [
            "/opt/homebrew/bin/ffmpeg",
            "/usr/local/bin/ffmpeg",
            "/usr/bin/ffmpeg",
        ]
        .map(PathBuf::from),
    );
    candidates.into_iter().find(|p| executable_file(p))
}
fn executable_file(path: &std::path::Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Convert audio file to 16kHz mono PCM f32 samples (required by whisper)
fn convert_audio_to_pcm(input_path: &PathBuf) -> Result<Vec<f32>, String> {
    use std::fs::File;
    use std::io::Read;

    let mut file =
        File::open(input_path).map_err(|e| format!("Failed to open audio file: {}", e))?;

    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|e| format!("Failed to read audio file: {}", e))?;

    if bytes.len() < 12 {
        return Err("Audio file too small".to_string());
    }

    // Check if already a valid WAV file with correct format
    let is_wav = &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE";

    if is_wav {
        // Try to parse WAV directly first
        match parse_wav_pcm(&bytes) {
            Ok(samples) => return Ok(samples),
            Err(_) => {
                // WAV but not in a compatible format — convert via ffmpeg
            }
        }
    }

    // Not WAV or WAV in incompatible format — convert via ffmpeg
    convert_via_ffmpeg(input_path)
}

/// Parse a simple 16-bit PCM WAV file
fn parse_wav_pcm(bytes: &[u8]) -> Result<Vec<f32>, String> {
    if bytes.len() < 44 {
        return Err("WAV file too small".to_string());
    }

    if &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("Not a WAV file".to_string());
    }

    // Find the "fmt " chunk
    let mut pos = 12;
    let mut sample_rate: u32 = 0;
    let mut channels: u16 = 0;
    let mut bits_per_sample: u16 = 0;
    let mut audio_format: u16 = 0;
    let mut data_start = 0usize;
    let mut data_size = 0usize;

    while pos + 8 <= bytes.len() {
        let chunk_id = &bytes[pos..pos + 4];
        let chunk_size = u32::from_le_bytes([
            bytes[pos + 4],
            bytes[pos + 5],
            bytes[pos + 6],
            bytes[pos + 7],
        ]) as usize;

        if chunk_id == b"fmt " && chunk_size >= 16 {
            audio_format = u16::from_le_bytes([bytes[pos + 8], bytes[pos + 9]]);
            channels = u16::from_le_bytes([bytes[pos + 10], bytes[pos + 11]]);
            sample_rate = u32::from_le_bytes([
                bytes[pos + 12],
                bytes[pos + 13],
                bytes[pos + 14],
                bytes[pos + 15],
            ]);
            bits_per_sample = u16::from_le_bytes([bytes[pos + 22], bytes[pos + 23]]);
        } else if chunk_id == b"data" {
            data_start = pos + 8;
            data_size = chunk_size.min(bytes.len() - data_start);
        }

        pos += 8 + chunk_size;
        // Align to 2-byte boundary
        if chunk_size % 2 != 0 {
            pos += 1;
        }
    }

    if data_start == 0 || data_size == 0 {
        return Err("No data chunk found".to_string());
    }

    // Only handle PCM format (1) with 16-bit samples
    if audio_format != 1 || bits_per_sample != 16 {
        return Err(format!(
            "Unsupported WAV format: audio_format={}, bits_per_sample={}",
            audio_format, bits_per_sample
        ));
    }

    let pcm_bytes = &bytes[data_start..data_start + data_size];
    let samples_i16: Vec<i16> = pcm_bytes
        .chunks_exact(2)
        .map(|chunk| i16::from_le_bytes([chunk[0], chunk[1]]))
        .collect();

    // Convert to mono if stereo
    let mono_samples: Vec<f32> = if channels == 2 {
        samples_i16
            .chunks_exact(2)
            .map(|pair| (pair[0] as f32 + pair[1] as f32) / (2.0 * i16::MAX as f32))
            .collect()
    } else {
        samples_i16
            .iter()
            .map(|&s| s as f32 / i16::MAX as f32)
            .collect()
    };

    // Resample to 16kHz if needed
    if sample_rate != 16000 {
        Ok(resample(&mono_samples, sample_rate, 16000))
    } else {
        Ok(mono_samples)
    }
}

/// Convert any audio file to 16kHz mono PCM via ffmpeg
fn convert_via_ffmpeg(input_path: &PathBuf) -> Result<Vec<f32>, String> {
    let ffmpeg = find_ffmpeg().ok_or_else(|| {
        "ffmpeg not found. Install it to transcribe non-WAV audio files.\n\
         macOS: brew install ffmpeg\n\
         Linux: sudo apt install ffmpeg"
            .to_string()
    })?;

    emit_progress(
        "ffmpeg_converting",
        Some(input_path.to_string_lossy().to_string()),
    );

    // Convert to 16kHz mono 16-bit PCM WAV and write to stdout
    let output = std::process::Command::new(&ffmpeg)
        .args([
            "-i",
            input_path.to_str().unwrap_or(""),
            "-ar",
            "16000",
            "-ac",
            "1",
            "-f",
            "s16le", // raw PCM, no WAV header
            "-acodec",
            "pcm_s16le",
            "-v",
            "quiet",
            "pipe:1", // output to stdout
        ])
        .output()
        .map_err(|e| format!("Failed to run ffmpeg: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "ffmpeg conversion failed (exit {}): {}",
            output.status.code().unwrap_or(-1),
            stderr
        ));
    }

    let pcm_bytes = output.stdout;
    if pcm_bytes.is_empty() {
        return Err("ffmpeg produced no audio output".to_string());
    }

    // Convert raw i16 PCM to f32
    let samples: Vec<f32> = pcm_bytes
        .chunks_exact(2)
        .map(|chunk| {
            let sample = i16::from_le_bytes([chunk[0], chunk[1]]);
            sample as f32 / i16::MAX as f32
        })
        .collect();

    Ok(samples)
}

/// Simple linear interpolation resampling
fn resample(samples: &[f32], from_rate: u32, to_rate: u32) -> Vec<f32> {
    if from_rate == to_rate || samples.is_empty() {
        return samples.to_vec();
    }

    let ratio = from_rate as f64 / to_rate as f64;
    let output_len = (samples.len() as f64 / ratio) as usize;
    let mut output = Vec::with_capacity(output_len);

    for i in 0..output_len {
        let src_pos = i as f64 * ratio;
        let src_idx = src_pos as usize;
        let frac = src_pos - src_idx as f64;

        let sample = if src_idx + 1 < samples.len() {
            samples[src_idx] as f64 * (1.0 - frac) + samples[src_idx + 1] as f64 * frac
        } else if src_idx < samples.len() {
            samples[src_idx] as f64
        } else {
            0.0
        };

        output.push(sample as f32);
    }

    output
}

#[cfg(test)]
mod helper_tests {
    use super::*;
    #[test]
    fn helper_must_be_executable_regular_file() {
        let directory =
            std::env::temp_dir().join(format!("vasya-helper-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("ffmpeg");
        std::fs::write(&path, b"#!/bin/sh\nexit 0\n").unwrap();
        assert!(!executable_file(&directory));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            assert!(!executable_file(&path));
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            assert!(executable_file(&path));
        }
        std::fs::remove_dir_all(directory).unwrap();
    }
}
