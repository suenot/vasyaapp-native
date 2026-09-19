# Vasya Native

Two native Rust desktop clients derived from [suenot/vasyaapp](https://github.com/suenot/vasyaapp), sharing one Telegram engine and application state machine:

- **Vasya GPUI** — GPUI Kit 0.6, native GPU-rendered components.
- **Vasya Iced** — Iced 0.14, native widgets and asynchronous subscriptions.

No Tauri, WebView, React, JavaScript runtime, HTML or CSS application frontend is used. The original checkout in `vasyaapp/` is separate, ignored by this repository, and is not needed to build these applications.

## Run

On the development Mac, open `dist/Vasya GPUI.app` or `dist/Vasya Iced.app`. Enter your own Telegram API ID/hash, then sign in with your phone, code and optional two-step password. Alternatively, connect an existing `vasya-server` with its URL and bearer token.

The first packaged release targets **Apple Silicon on macOS 27**: bundled Homebrew FFmpeg libraries require macOS 27. Building on an older supported macOS with compatible native dependencies is a separate validation target. Packages are locally ad-hoc signed, not Developer ID notarized.

Each GUI has an independent profile and Keychain namespace. Existing application's sessions/databases are never opened or migrated automatically. Ordinary operation uses real Telegram data; synthetic data exists only with the explicit `--stress-test` argument.

## Build

Install a current stable Rust toolchain with rustfmt, Xcode Command Line Tools, CMake and FFmpeg. This release was built with Rust 1.98.1.

```sh
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"
brew install cmake ffmpeg
cargo run -p vasyaapp-gpui
cargo run -p vasyaapp-iced
./scripts/build-macos.sh
python3 scripts/verify-macos.py
```

The package script builds both release executables, the Whisper helper and a small AVFoundation capture helper, bundles FFmpeg and its non-system dynamic libraries, writes application metadata from Cargo's workspace version, and signs/verifies both `.app` bundles. The capture bridge is Swift using Apple's native AVFoundation API; the interfaces, Telegram engine and application state are Rust.

`VASYA_BUILD_PROFILE=dev ./scripts/build-macos.sh` creates development packages. `VASYA_NATIVE_DATA_DIR=/path` overrides the profile parent directory and still creates separate `gpui` and `iced` children. No Node tooling or frontend build is required.

## Capabilities

- Multiple accounts, code/2FA login and encrypted session restoration.
- Cached and live chats, unread state, favorites, custom folder filters and tab ordering/visibility.
- Paged history and topics, formatted/selectable text, search with message navigation, sending and single/bulk forwarding.
- Files, images, clipboard images, native file picker, voice recording and camera capture; downloaded audio/video opens with the system application.
- Cloud Deepgram and local Whisper transcription, optional automatic transcription, model download and per-profile settings.
- Native light/dark themes, scale, text size, density, sender grouping, Russian/English labels, folder layout and remappable shortcuts.
- Embedded engine, remote REST/SSE engine, independent legacy metadata synchronization and opt-in authenticated local REST/GraphQL/SSE API.

Unfinished call audio/video, message reply/edit/delete/pin and profile/privacy controls in the upstream application were not promoted into fake working features. Call signaling remains in the engine/API; the UI explains that calls are unavailable. The upstream's unimplemented web-only controls do not create a working-feature guarantee. See [feature and validation notes](docs/FEATURES.md).

## Architecture and responsiveness

`crates/vasya-core` contains the Telegram engine. `vasya-server` supplies the existing API contract. `vasya-backend` invokes the router in-process in embedded mode and uses REST/SSE in remote mode. `vasya-native` owns typed commands, immutable view snapshots, cache merging, scoped state and background tasks. The two GUI crates render that same state.

Network, file operations, capture processes and transcription run off the UI thread. State keys include account/chat/topic; switching transport cancels outstanding work, and late responses cannot replace another dialog. Incoming edits/deletions take precedence over stale cache/history. Queued data for a logged-out account is discarded, and late session handles cannot recreate its deleted session file.

Watch notifications drive both interfaces. Chat/message/search lists are virtualized, older-history loading preserves the scroll anchor, and foreground snapshots are coalesced. Iced text measurement runs on a coalesced background worker. Media concurrency, disk caches and in-memory history are bounded.

## Checks and measurements

```sh
cargo test --workspace
cargo fmt --all -- --check
cargo test --manifest-path sidecars/stt-sidecar/Cargo.toml
python3 scripts/verify-macos.py --stress
```

Stress mode uses an isolated temporary profile, 10,000 chats, a 100,000-message paginated source and 100 synthetic incoming events per second. It scrolls, loads history and types drafts for 30 seconds. Results in [docs/performance.json](docs/performance.json) describe programmatic input acknowledgment and CPU-side toolkit work; **they are not measurements of actual GPU frame presentation or physical keyboard latency**. See [verification](docs/VERIFICATION.md) for the exact tested boundaries.

Version is defined once in root `Cargo.toml` and inherited by workspace packages; changes are tracked in [CHANGELOG.md](CHANGELOG.md). Upstream provenance and retained license: [UPSTREAM.md](UPSTREAM.md), [LICENSE](LICENSE).
