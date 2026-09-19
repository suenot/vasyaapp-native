# Native translation — 0.10.0

Both GPUI and Iced use the same Rust translation service and state machine. No web frontend is involved.

1. Open **Translation settings** in the application menu. Set the OpenAI-compatible API base URL, model and token. Include `/v1` when required by your provider; the service appends `/chat/completions`. HTTPS is required for remote providers; loopback HTTP supports local models.
2. Open a chat and choose **Chat translation**. Incoming and outgoing switches are independent and off by default. Use language codes such as `ru`, `zh`, `en`, or a language name.
3. For a Chinese-speaking contact, set outgoing to `zh` and incoming to `ru`. Compose in Russian; the translation is delivered to Telegram. Incoming text translates in the background, with an original-text toggle.

Text and attachment captions follow outgoing settings. Forwarding existing messages and voice transcription remain separate operations. On translation failure no untranslated message is sent; the text draft stays available. A completed send clears only its own unchanged draft, including across chat navigation. Changing provider settings invalidates outstanding translations before delivery.

Incoming work is restricted to the native viewport (GPUI includes its existing bounded list overdraw margin), with at most two active requests, cancellation when their view or configuration becomes obsolete, and a 128-entry / 1 MiB result cache. Original message text is never replaced in the stored history. Errors remain visible until an explicit retry or relevant setting/text change.

Preferences are scoped by connection, account and chat; forum topics inherit their parent chat's languages. Each GUI keeps its own profile. The original application's settings and tokens are not automatically migrated.

The token is encrypted using the existing master-key provider. Settings responses expose only whether a key exists. A blank token field keeps the saved key; **Remove saved token** clears it on Save. Message text is sent to the configured model provider only for enabled directions. Translations are not stored in the general disk cache.

Embedded mode runs entirely through the local Rust engine. Remote mode needs a Vasya server implementing `GET/PUT /api/v1/translation/settings` and `POST /api/v1/translation/translate`. Those routes are present in this repository and the original project's translation change; an older deployed server must be updated before remote translation works.
