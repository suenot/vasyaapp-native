# Native feature coverage

Both frontends use the same commands and state from `vasya-native`; differences are native toolkit layout and interaction, not separate Telegram implementations. This is functional migration of implemented upstream behavior, not pixel-identical reproduction of the web frontend.

| Area | Native clients |
| --- | --- |
| Accounts | API credentials, phone/code/2FA, multiple accounts, session restoration, logout |
| Navigation | Accounts, chats, unread counts, favorites, folders, tabs, forum topics |
| History | Cached/live pages, older pages, selectable text, Markdown, message search and navigation |
| Messaging | Compose/send, failed-send retry, single and multiple message forwarding |
| Attachments | Native file picker, clipboard images, download/open, bounded image previews |
| Capture | Native AVFoundation microphone and camera helper; OS permission required when invoked |
| Audio/video | Open downloaded media using the system application |
| Translation | Independent incoming/outgoing chat languages, provider settings, originals, retry, translated attachment captions |
| Transcription | Deepgram, local Whisper, model installation, automatic transcription preference |
| Preferences | Light/dark appearance, language, scale, text size, density, grouping, folders, shortcuts, notifications |
| Connections | Embedded Rust engine, remote REST/SSE, legacy metadata storage configuration |
| Local API | Opt-in authenticated localhost REST/GraphQL/SSE listener with persistent configuration |

Call signaling is retained in the engine. Functional voice/video calls were unfinished upstream and are explicitly unavailable in the native UI. Upstream placeholder reply/edit/delete/pin, profile and privacy controls are not advertised as working native features.

Image attachments are decoded and resized off the UI thread with bounded concurrency and memory. Markdown image syntax is not a second uncontrolled attachment loader. History retains at most 5,000 confirmed messages per cached dialog and at most 16 dialogs; pagination can load another portion of the conversation. Outgoing pending messages are preserved while trimming confirmed history.

Each application has an independent profile. Existing upstream profiles are not migrated or modified. Remote transport changes cancel outstanding work and replace event subscriptions. Late account/dialog/topic responses and stale viewport reports are rejected.

For what was actually exercised without account credentials, see [VERIFICATION.md](VERIFICATION.md).
