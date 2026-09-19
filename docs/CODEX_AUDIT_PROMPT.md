# Codex efficiency audit prompt

Audit the last completed main conversation for this project and its child sessions. Improve token usage and latency without weakening correctness, verification, security or delivery requirements.

1. Locate project sessions using read-only local Codex metadata and rollout files. Aggregate locally first; inspect only episodes supporting a finding. Do not dump whole conversations, images, credentials or encrypted payloads into context.
2. Sum individual `token_usage_record` usage, deduplicating response IDs. Report input, cached input, uncached input and output separately; reasoning is part of output. Never sum cumulative counters or interpret image base64 size as text tokens. Explain differences between counters; do not infer money or subscription limits from token totals.
3. Look for truncated output, unnecessarily broad reads, duplicate checks, build-lock waits, repeated polling and agent coordination overhead. Compare repetitions with intervening changes before calling them waste.
4. For hook failures, establish the command, exit status and root cause. Distinguish local UI events from model-visible context. Back up configuration and fix confirmed broken registrations without disabling working checks; verify the fix and state whether live reload was tested.
5. Review relevant AGENTS.md and actually used skills for duplicated rules, unavailable tools, broad triggers, unconditional document reads and missing completion criteria. Load supporting skill references only when needed. Preserve authorized workflows and safety boundaries.
6. In this workspace, coordinate backend/shared-state, GPUI and Iced ownership; appoint one integration/build/package owner. Avoid duplicate Cargo builds and conflicting desktop UI sessions. Send agent messages for changed contracts, blockers or completed work.
7. Report at most five prioritized findings with evidence, smallest correction and verification. Apply fixes when authorized. Do not promise percentage savings without a comparable measurement. Finish after verifying the changes; do not expand to unrelated projects.

Adapted from [OpenAI guidance on skills and prompts](https://developers.openai.com/blog/rethinking-skills-and-prompts-for-gpt-6-astra) and [skill documentation](https://learn.chatgpt.com/docs/build-skills). This is a project prompt, not a verbatim official template.
