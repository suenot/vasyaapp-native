# vasya-server

Telegram session host: an axum REST API over the `vasya-core` engine.
Built as a **library** (router builder) plus a thin standalone **binary**, so
the same router can run standalone on a server (JWT auth, Postgres-less
file-backed ownership) or be mounted in-process by the desktop app on
127.0.0.1 (embedded-local token auth) for local AI agents.

## Layout

| Piece | Purpose |
|---|---|
| `vasya_server::build_context(manager, options)` | assemble shared state around any `TelegramClientManager` |
| `vasya_server::build_router(ctx)` | the complete `/api/v1` axum Router |
| `vasya_server::start_existing_sessions(ctx)` | load sessions from disk + start update pumps |
| `ctx.events` (`BroadcastEventSink`) | in-process event bus; SSE at `/api/v1/events`, GraphQL subscriptions consume the same bus (Phase 3) |
| `src/main.rs` | env-configured standalone server |

Auth modes: `AuthMode::Jwt { secret }` (HS256, same `Claims` as `backend/`,
share `JWT_SECRET` to accept its tokens) or `AuthMode::EmbeddedLocal { token }`
(single local user, no database). Account ownership is per-user, claim-on-first-touch,
persisted to `data_dir/accounts.json`.

Anti-flood: token bucket per account on mutating routes (burst 10, 1 token/2s,
`RateLimitConfig`) + FLOOD_WAIT absorb-and-retry; both surface as HTTP 429
with `Retry-After`. Agent keys get a second, stricter per-key bucket
(burst 5, 1 token/5s, `ServerOptions.agent_rate_limit`).

Voice calls (1:1 + group): the **signaling / control / state** surface is
live over REST and GraphQL — both transports call the same `*_op` functions,
which delegate to the shared `vasya_core::telegram::{calls, group_calls}`
engine that the desktop Tauri commands also use.

- 1:1: `POST /accounts/{acc}/calls/{request,accept,confirm,discard}` —
  DH key exchange + `phone.requestCall`/`acceptCall`/`confirmCall`/`discardCall`.
- Group: `POST /accounts/{acc}/group-calls`, `…/join`, `…/leave`, `…/mute`,
  `GET …/group-calls/participants` — full MTProto signaling
  (`phone.createGroupCall`/`joinGroupCall`/`leaveGroupCall`/`editGroupCallParticipant`/`getGroupParticipants`).
- Call state changes (`telegram:incoming-call`, `telegram:call-*`,
  `telegram:group-call-*`) flow through the same event bus to `/events` (SSE)
  and the `callEvent` / `groupCallEvent` GraphQL subscriptions.

**Headless-audio caveat:** a server has no microphone or speaker, so real-time
call *audio* (capture/playback) cannot run here — that is a client-side
concern, handled by the desktop VoIP sidecar. The two audio-only 1:1
endpoints, `POST /accounts/{acc}/calls/{volume,mute}`, therefore return a
documented **501** explaining audio is client-side. Group-call *mute* is a
real MTProto signal (`editGroupCallParticipant`), so it is fully implemented
on the server.

Storage-mode is reported honestly rather than stubbed: the desktop `local`
(on-device SQLite) / `remote` (this server) toggle does not apply to a headless
server, which always persists state server-side. `GET /storage-mode` returns
`{mode:"server", configurable:false, reason}` and `PUT /storage-mode` returns a
`400` (the mode is fixed) — no remaining `501` here.

## Speech-to-text (STT)

Voice-message transcription is implemented on the server (scope `stt:use`):

| Provider | Where | Notes |
| --- | --- | --- |
| **Deepgram** (cloud, Nova-2) | **Server + desktop** | Bring-your-own-key. The per-user key is stored masked and **never** logged or returned in full. This is the server default. |
| **Local Whisper** (`whisper.cpp`) | **Desktop only** | Depends on the `stt-sidecar` binary that is excluded from the server image. Selecting it on the server returns a clear `400` ("local Whisper is desktop-only; use the Deepgram provider"). |

```sh
# Per-user settings (GET masks the key, never echoes it raw)
curl -s -H "$TOK" $BASE/stt/settings
# -> {"provider":"deepgram","deepgramApiKeySet":false,"deepgramApiKeyMasked":null,"whisperModel":"base","language":"en"}

# Set the Deepgram key + provider (key is write-only; "" clears it)
curl -s -X PUT -H "$TOK" -H 'content-type: application/json' \
  -d '{"provider":"deepgram","deepgramApiKey":"<your-deepgram-key>","language":"en"}' \
  $BASE/stt/settings
# GET now shows: "deepgramApiKeySet":true,"deepgramApiKeyMasked":"••••cdef"

# Transcribe raw audio (upload the bytes directly)
curl -s -X POST -H "$TOK" -H 'content-type: application/octet-stream' \
  -H 'x-language: en' --data-binary @voice.ogg $BASE/stt/transcribe
# -> {"text":"hello world","language":"en"}

# …or transcribe a voice message already in a chat (fetched via the engine)
curl -s -X POST -H "$TOK" -H 'content-type: application/json' \
  -d '{"accountId":"a1","chatId":12345,"messageId":678}' \
  $BASE/stt/transcribe
# -> {"text":"...","language":"en"}

# Local-Whisper model catalog is reported as unavailable on the server (200, not 501)
curl -s -H "$TOK" $BASE/stt/models
# -> {"available":false,"reason":"local Whisper is desktop-only ...","models":[...]}
```

## Agent-native layer

AI agents are first-class clients with their own scoped credentials
(plan §4.4) — never borrowed human sessions:

```sh
# create a key (human session required; the secret is shown ONCE)
curl -s -H "$TOK" -H 'content-type: application/json' \
  -d '{"name":"my-bot","scopes":["accounts:read","chats:read","messages:read","messages:send"],"ttlSecs":2592000}' \
  $BASE/agent-keys
# -> {"id":"ak..","secret":"vk_ak.._..", ...}
curl -s -H "$TOK" $BASE/agent-keys            # list (no secrets)
curl -s -H "$TOK" $BASE/agent-keys/scopes     # valid scopes + descriptions
curl -s -X DELETE -H "$TOK" $BASE/agent-keys/<id>   # revoke
```

### Scopes

A key carries one or more scopes; out-of-scope calls get 403 with the
missing scope named. Destructive operations have their own narrow scopes so
they are never granted implicitly:

| Scope | Grants |
| --- | --- |
| `accounts:read` | List accounts and read account/avatar metadata |
| `accounts:delete` | Log out / delete an account (`DELETE /accounts/{acc}`) |
| `telegram:login` | Log in a Telegram account (login endpoints only) |
| `chats:read` | List chats, contacts, topics, search and chat photos |
| `chats:write` | Create groups and channels |
| `chats:delete` | Delete/leave a chat (`DELETE /accounts/{acc}/chats/{chat_id}`) |
| `messages:read` | Read messages, message media and search messages |
| `messages:send` | Send messages and media, mark messages read |
| `messages:forward` | Forward messages (`POST /accounts/{acc}/messages/forward`) |
| `folders:read` | Read folders and tabs |
| `folders:write` | Create, update and delete folders and tabs |
| `events:read` | Subscribe to the server-sent events stream |
| `calls:use` | Use voice/video and group calls |
| `stt:use` | Use speech-to-text |

> **Breaking change (pre-1.0):** `accounts:delete`, `chats:delete` and
> `messages:forward` were split out of `telegram:login`, `chats:write` and
> `messages:send` respectively. Existing keys are **not** auto-granted the new
> destructive scopes — re-issue keys that need them.

### Per-account allowlist

Pass `accountIds` at creation to restrict a key to specific accounts. The
allowlist is checked **after** the scope gate: a `/accounts/{acc}/…` request that
clears its required scope but targets an account outside the list gets
403 `account not in key allowlist`. Omitted/empty = all the owner's accounts.

```sh
curl -s -H "$TOK" -H 'content-type: application/json' \
  -d '{"name":"chat-x-bot","scopes":["messages:send"],"accountIds":["<acc-uuid>"]}' \
  $BASE/agent-keys
```

The `vk_...` secret is a normal bearer token; only a SHA-256 hash is stored.
Agent keys cannot manage keys or read the audit log. They **can** use GraphQL
(queries, mutations and WS subscriptions): each resolver enforces the same scope
the equivalent REST endpoint would require, plus the per-account allowlist (see
[GraphQL → Agent keys](#agent-keys-over-graphql) below).

Audit: every mutating call (user or agent) is appended to
`data_dir/audit.log` — `GET /api/v1/audit?limit=` (human only) reads it.

Idempotency: send an `Idempotency-Key` header on mutating routes; repeats
within 24h replay the original response with `Idempotency-Replayed: true`;
concurrent duplicates get 409.

### vasya-mcp

`../vasya-mcp` is a thin stdio MCP server wrapping this API (13 read/write
tools: list_accounts, list_chats, get_messages, send_message, search, …).

```sh
cd ../vasya-mcp && npm install && npm run build
VASYA_API_URL=http://127.0.0.1:8787 VASYA_AGENT_KEY=vk_... node dist/index.js
```

Claude Desktop / any MCP client config:

```json
{ "mcpServers": { "vasya": {
    "command": "node",
    "args": ["<repo>/app/src-tauri/vasya-mcp/dist/index.js"],
    "env": { "VASYA_API_URL": "http://127.0.0.1:8787", "VASYA_AGENT_KEY": "vk_..." } } } }
```

## Run (standalone)

```sh
cd app/src-tauri
SESSION_MASTER_KEY=$(openssl rand -hex 32)   # in production: injected from KMS/secret manager
export SESSION_MASTER_KEY
export TELEGRAM_API_ID=...                   # https://my.telegram.org
export TELEGRAM_API_HASH=...
export AUTH_MODE=embedded                    # simplest mode for a smoke test
cargo run -p vasya-server
# prints: VASYA_LOCAL_TOKEN=<64 hex> — the bearer token for all requests
```

JWT mode instead: `AUTH_MODE=jwt JWT_SECRET=<same secret as backend/>`; get a
token from the sync backend's login endpoint.

## Manual smoke test

```sh
BASE=http://127.0.0.1:8787/api/v1
TOK="Authorization: Bearer $VASYA_LOCAL_TOKEN"

# 1. Liveness + machine-readable contract (no auth)
curl -s $BASE/health                          # {"status":"ok"}
curl -s $BASE/openapi.json | head -c 300

# 2. Auth gate: 401 without/with wrong token
curl -s -o /dev/null -w '%{http_code}\n' $BASE/accounts            # 401
curl -s -H "$TOK" $BASE/accounts                                   # []

# 3. Telegram login (sends a real code to the phone!)
curl -s -H "$TOK" -H 'content-type: application/json' \
  -d '{"phone":"+1555..."}' $BASE/telegram/login/code
# -> {"accountId":"<uuid>","phone":"+1555..."}
curl -s -H "$TOK" -H 'content-type: application/json' \
  -d '{"accountId":"<uuid>","code":"12345"}' $BASE/telegram/login/verify
# -> {"status":"authorized","user":{...}} or {"status":"password_required"}
# if 2FA: -d '{"accountId":"<uuid>","password":"..."}' $BASE/telegram/login/password

# 4. Realtime: subscribe before loading chats, watch chat-loaded events stream
curl -sN -H "$TOK" "$BASE/events" &
ACC=<uuid>
curl -s -X POST -H "$TOK" $BASE/accounts/$ACC/chats/load           # 202

# 5. Data routes
curl -s -H "$TOK" "$BASE/accounts/$ACC/chats" | head -c 500
curl -s -H "$TOK" "$BASE/accounts/$ACC/chats/<chat_id>/messages?limit=5"
curl -s -X POST -H "$TOK" -H 'content-type: application/json' \
  -d '{"text":"hello from vasya-api"}' $BASE/accounts/$ACC/chats/<chat_id>/messages

# 6. Media upload (raw body + headers)
curl -s -X POST -H "$TOK" \
  -H 'x-file-name: photo.jpg' -H 'x-mime-type: image/jpeg' \
  --data-binary @photo.jpg $BASE/accounts/$ACC/chats/<chat_id>/media

# 7. Rate limit: >10 rapid sends -> 429 with Retry-After
for i in $(seq 1 12); do curl -s -o /dev/null -w '%{http_code} ' -X POST -H "$TOK" \
  -H 'content-type: application/json' -d '{"text":"spam '$i'"}' \
  $BASE/accounts/$ACC/chats/<chat_id>/messages; done; echo

# 8. Voice call signaling (state only; audio stays on the client)
curl -s -X POST -H "$TOK" -H 'content-type: application/json' \
  -d '{"userId":123456789,"isVideo":false}' $BASE/accounts/$ACC/calls/request
# Audio-only endpoints document a 501 (call audio is client-side):
curl -s -o /dev/null -w '%{http_code}\n' -X POST -H "$TOK" \
  -H 'content-type: application/json' -d '{"callId":1,"muted":true}' \
  $BASE/accounts/$ACC/calls/mute   # -> 501
# 8b. Speech-to-text (cloud Deepgram; see the STT section above)
curl -s -X PUT -H "$TOK" -H 'content-type: application/json' \
  -d '{"deepgramApiKey":"<your-deepgram-key>"}' $BASE/stt/settings
curl -s -X POST -H "$TOK" -H 'content-type: application/octet-stream' \
  --data-binary @voice.ogg $BASE/stt/transcribe   # -> {"text":...}
# 8c. Storage-mode: fixed server-side (GET reports it; PUT 400s — desktop-only toggle)
curl -s -H "$TOK" $BASE/storage-mode   # -> {"mode":"server","configurable":false,"reason":...}
curl -s -o /dev/null -w '%{http_code}\n' -X PUT -H "$TOK" \
  -H 'content-type: application/json' -d '{"mode":"local"}' $BASE/storage-mode   # -> 400

# 9. Restart the server: sessions reload from disk (encrypted with
#    SESSION_MASTER_KEY), GET /accounts shows the account again.
```

## GraphQL

Endpoints (same bearer auth as REST for HTTP; WS authenticates on
`connection_init`):

| Route | What |
|---|---|
| `POST /api/v1/graphql` | queries + mutations (REST parity: accounts, chats, messages, search, topics, folders/tabs, telegram login, voice-call signaling) |
| `GET /api/v1/graphql/ws` | subscriptions, `graphql-transport-ws` + legacy `graphql-ws` protocols |
| `GET /api/v1/graphql/sdl` | schema SDL (public, like /openapi.json) |
| `GET /api/v1/graphql/playground` | dev playground — only with `VASYA_GRAPHQL_PLAYGROUND=1` |

Subscriptions: `messageReceived(accountId, chatId?)`, `messageEdited`,
`messageDeleted`, `chatUpdated`, `chatsLoadingProgress`, `connectionStatus`,
`callEvent`, `groupCallEvent`, `sttProgress`. Every item is
`{ event, payload }` where `event` is the original Tauri-compatible event
name and `payload` is byte-identical to the desktop event payload.

### Subscribing to messageReceived over WebSocket

With [wscat](https://github.com/websockets/wscat) (graphql-transport-ws
protocol — note the subprotocol header and the token inside
`connection_init`, not an HTTP header):

```sh
wscat -c ws://127.0.0.1:8787/api/v1/graphql/ws -s graphql-transport-ws
> {"type":"connection_init","payload":{"Authorization":"Bearer <token>"}}
< {"type":"connection_ack"}
> {"id":"1","type":"subscribe","payload":{"query":"subscription { messageReceived(accountId: \"<uuid>\") { event payload } }"}}
# now send the account a Telegram message; each one arrives as:
< {"id":"1","type":"next","payload":{"data":{"messageReceived":{"event":"telegram:new-message","payload":{"id":123,"chatId":456,"text":"hi","accountId":"<uuid>", "...":"..."}}}}}
```

From JS with [`graphql-ws`](https://github.com/enisdenjo/graphql-ws):

```js
import { createClient } from "graphql-ws";

const client = createClient({
  url: "ws://127.0.0.1:8787/api/v1/graphql/ws",
  connectionParams: { Authorization: `Bearer ${token}` },
});

client.subscribe(
  { query: `subscription { messageReceived(accountId: "${acc}") { event payload } }` },
  { next: ({ data }) => console.log(data.messageReceived.payload), error: console.error, complete: () => {} },
);
```

Query/mutation smoke over HTTP:

```sh
curl -s -H "$TOK" -H 'content-type: application/json' \
  -d '{"query":"{ accounts { accountId phone connected } }"}' $BASE/graphql
curl -s -H "$TOK" -H 'content-type: application/json' \
  -d '{"query":"mutation { sendMessage(accountId: \"<uuid>\", chatId: <id>, text: \"via graphql\") { id date } }"}' $BASE/graphql
```

### Agent keys over GraphQL

A `vk_...` agent key is a valid bearer for `POST /graphql` and for the WS
`connection_init` payload, exactly like a human token. Because every operation
shares the single `/graphql` path, scope enforcement happens **inside each
resolver** rather than in the path-based policy middleware: a resolver requires
the same scope its REST twin does (`sendMessage` → `messages:send`, `chats` →
`chats:read`, the `messageReceived`/`messageEdited`/… subscriptions →
`events:read`, etc.), and `/accounts/{acc}/…`-style fields also check the key's
per-account allowlist. The resolver↔REST scope map is cross-checked by the
`graphql_scopes_mirror_rest` test so the two transports can't drift apart.

Missing scope or allowlist violations come back as a normal GraphQL error
(HTTP 200, `errors[]`, `extensions.code = "FORBIDDEN"`); for a subscription the
error is the first — and only — item the stream yields before it closes.

```sh
# scope present → data; scope absent → {"errors":[{"message":"Missing scope: chats:read", ...}]}
curl -s -H "Authorization: Bearer vk_..." -H 'content-type: application/json' \
  -d '{"query":"{ chats(accountId: \"<uuid>\") { id title } }"}' $BASE/graphql
```

## Embedding in the desktop app (task #8 sketch)

```rust
let ctx = vasya_server::build_context(app_manager.clone(), ServerOptions::new(
    AuthMode::embedded_with_random_token(), data_dir))?;
// Update pumps: use vasya_core::events::MultiEventSink to emit to both the
// webview (TauriEventSink) and ctx.events, then serve build_router(ctx) on 127.0.0.1.
```
