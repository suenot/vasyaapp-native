# Verification — 0.10.0

Validation performed on Apple Silicon, macOS 27, Rust 1.98.1. Both `.app` bundles are locally ad-hoc signed. The bundled FFmpeg runtime requires macOS 27; older macOS versions and Intel Macs are not claimed as validated targets.

## Automated checks

- `cargo test --workspace`: **143 passed**, zero failures; core session lifecycle, backend adapters/cache/configuration, application state/races/pagination, server contracts/search peer identities, and GUI image decoding/cache tests. Translation coverage adds real mock-provider HTTP calls, encrypted/write-only credentials, authentication/body limits, outgoing generation/draft races and native translated-row layout checks.
- `cargo fmt --all -- --check`.
- `cargo test --manifest-path sidecars/stt-sidecar/Cargo.toml`: **1 passed** in 0.9.0; the sidecar source is unchanged.
- `scripts/build-macos.sh`: release binaries, Whisper and native capture helpers, relocatable FFmpeg libraries, app metadata and signature verification.
- `python3 scripts/verify-macos.py --stress`: signatures, bundled FFmpeg/Whisper startup, real native windows, isolated offline load test for both packaged applications. Results are recorded in [performance.json](performance.json).

The load scenario uses 10,000 chats, a 100,000-message paginated source, 100 incoming events/second, scripted scrolling, history loading and draft changes over 30 seconds. GUI work and programmatic input acknowledgment are instrumented. These are not GPU frame presentation measurements or physical keyboard latency measurements. RSS and CPU are single process samples at 15 seconds, not peak memory or sustained CPU guarantees. The scenario does not claim that all 100,000 messages are resident at once.

## Manual and integration checks

During 0.9.0 validation, both packaged windows were opened and their credential/settings navigation inspected through the macOS UI. Native form scrolling and secret fields were checked without submitting credentials. GPUI component assets are bundled for checkbox/dropdown icons.

The unchanged packaged local Whisper helper was previously verified in 0.9.0: it successfully transcribed a generated spoken sentence using the packaged FFmpeg runtime and a real tiny GGML model. This exercised actual local inference, not a transcription fixture. The microphone and camera were not activated during validation.

For 0.10.0, the final Iced package was exercised against an isolated local mock server: incoming `你好` appeared as `Привет`, the original-text toggle restored `你好`, and sending `hello` delivered `你好，朋友` and cleared the composer. Native provider and independent chat-direction forms were inspected. No real messages or paid LLM requests were used. GPUI startup and the translation menu were inspected; its pre-seeded integration fixture was blocked by macOS Keychain access to a helper-created key, so a full GPUI translation UI run is not claimed. Shared controller/API tests cover both clients.

The original `vasyaapp/` checkout remained unchanged. The native workspace has no Tauri/WebView/React runtime or Node frontend build.

## Boundaries

No real Telegram account credentials were supplied for this migration. Real login/code/2FA, cross-account network behavior, actual message delivery, remote production SSE, cloud Deepgram, physical microphone/camera capture and OS notification permissions therefore still require an account/device acceptance run. Automated state and API tests cover their local contracts; they do not replace that run.

The packages are local development delivery artifacts, not notarized public macOS releases. No server deployment is required for the embedded desktop applications. Unfinished upstream features are listed in [FEATURES.md](FEATURES.md).
