# Vasya native workspace

## Scope and ownership
- Root Cargo workspace: `crates/vasya-core` and `crates/vasya-server` contain the engine/API; `crates/vasya-backend` provides adapters; `crates/vasya-native` owns shared state; `vasyaapp-gpui` and `vasyaapp-iced` render it.
- `vasyaapp/` is a separate, ignored upstream checkout. Edit it only when the task explicitly targets the original application; commit its changes in its own repository.
- For parallel work, assign distinct backend/shared-state, GPUI and Iced ownership. Agree on shared types before dependent edits. One agent owns Cargo integration checks, packaging and the shared desktop UI session; avoid duplicate builds waiting on the same target directory.

## Validation and delivery
- Start with checks for affected packages. Use `cargo test --workspace` for integrated/shared behavior and `cargo fmt --all -- --check` for Rust changes. Repeat only checks affected by later changes or failures.
- Use `docs/TRANSLATION.md` for translation behavior, `docs/FEATURES.md` for capability boundaries and `docs/VERIFICATION.md` for validation limits. Read them when relevant, not before every edit.
- For packaged desktop changes, use `scripts/build-macos.sh`, then `python3 scripts/verify-macos.py`; use `--stress` when responsiveness or performance is affected. Full logs belong in local files; summarize results and relevant errors.
- Root version comes from `[workspace.package].version` in `Cargo.toml`. Update workspace lockfile versions with product releases. Documentation/instruction-only changes go under `Unreleased` without a binary release.
- Embedded desktop applications need no server deployment. Server/remote changes require their documented infrastructure procedure. Never claim real Telegram, paid-provider or device acceptance from isolated mock tests.

## Context economy
- Search source paths directly; exclude `target/`, `dist/` and the upstream checkout unless needed. Do not load whole logs or source files when a relevant range suffices.
- Use `docs/CODEX_AUDIT_PROMPT.md` only when asked to audit Codex usage or instructions; it is not a mandatory startup checklist.
