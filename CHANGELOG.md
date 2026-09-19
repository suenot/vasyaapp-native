# Changelog

All notable changes are documented here using Keep a Changelog and Semantic Versioning. Product version comes from `[workspace.package].version` in `Cargo.toml`.

## [Unreleased]

## [0.9.0] - 2026-09-19

### Added
- Native GPUI Kit and Iced desktop clients sharing one Rust application engine.
- Account login, live/cache-first chats, paginated topics/history, search navigation, messaging, bulk forwarding and native media actions.
- Appearance/language/shortcut preferences, folders, favorites, tab visibility/order and download inspection.
- Encrypted remote-engine and metadata-sync configuration; persistent opt-in local API with stable authentication token.
- Native microphone/camera capture adapter, packaged Whisper helper and relocatable FFmpeg runtime.
- macOS package builder, startup checks and explicit offline responsiveness benchmark.

### Changed
- Run embedded API requests in-process, and consume remote REST/SSE through an asynchronous Rust adapter.
- Use watch-driven view snapshots, virtual lists, bounded caches and background text/media work.
- Give each native application independent data and Keychain namespaces.

### Fixed
- Isolate message/history/media state by account, chat and topic; reject stale replies and stale cache resurrection after edits/deletions.
- Preserve a newly composed draft when the previous send completes and deduplicate its echoed message.
- Coalesce chat reloads, cancel superseded searches, bound transcription and preserve media MIME/extensions.
- Disable encrypted session persistence before logout so late handles cannot recreate deleted session files.

### Security
- Cancel outstanding operations before changing transport and discard queued data for logged-out accounts.
- Keep persisted remote credentials encrypted and prevent secrets from being printed in server startup logs.

### Removed
- Tauri, React, browser rendering and Node frontend build from the new desktop applications.
