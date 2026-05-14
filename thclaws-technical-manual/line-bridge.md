# LINE Bridge (plan-07)

LINE OA ↔ thClaws desktop relay: a user chats with their thClaws session over LINE, the agent runs on their local machine, and a small server in the middle routes messages between LINE Messaging API webhooks and a per-install WebSocket.

| Layer | Lives at | Role |
|---|---|---|
| Client-side bridge | `crates/core/src/line/` | WS client + reply-sender + `LineApprover` + pairing-token config |
| Frontend modal | `frontend/src/components/LineConnectModal.tsx` | Paste pairing code → POST `/pair` → store JWT → start WS |
| Sidebar pill | `frontend/src/components/Sidebar.tsx` | "Bridge live · `<display_name>`" status with avatar |
| Worker integration | `crates/core/src/shared_session.rs` `ShellInput::LineMessage` arm | Drives `Agent::run_turn` per inbound LINE message |
| Official relay | `crates/line-server/` (workspace-only — not in public mirror) | Axum + Redis + Postgres on k3s at `line.thclaws.ai` |

## Why this doc

The LINE bridge is unusual among thClaws surfaces because anyone can write their own relay — the protocol between thClaws and the relay is intentionally narrow and documented. This page is the contract third-party relay implementers code against. The official relay lives outside the public repo (server-side infrastructure), but its wire shape is open.

## Wire protocol

### Client → relay: `POST /pair`

Body:
```json
{ "code": "ABCD1234", "cwd": "/path/to/project", "machine_label": "jimmy-mac" }
```

Successful response:
```json
{
  "token": "<HS256 JWT>",
  "line_user_id": "Uxxx…",
  "expires_at": 1735689600,
  "display_name": "Jimmy",
  "picture_url": "https://profile.line-scdn.net/…",
  "language": "th"
}
```

`display_name` / `picture_url` / `language` are optional — relays without a profile cache omit them (older relays, or `GET /v2/bot/profile/:userId` failure). thClaws falls back to "bridge live" on the sidebar pill when absent.

### Client → relay: `POST /unpair`

Authenticated by `Authorization: Bearer <jwt>`. Drops the binding row + reverse index. Idempotent — already-deleted bindings return 200 with `{"status": "already_clean"}`. Best-effort from the client side: the worker fires this in a detached task on `LineDisconnect` and proceeds with local cleanup regardless of the result.

### Client ↔ relay: WebSocket `/ws?token=<jwt>`

Relay → client envelopes:
```json
{ "kind": "user_message", "text": "…", "reply_token": "…", "request_id": "…" }
{ "kind": "postback", "data": "tool:allow:<request_id>" }
{ "kind": "notice", "text": "…" }
```

The client must support reconnect with exponential backoff — pod restarts during k8s rolling updates drop WS connections, and the official relay's [presence TTL](../thclaws/crates/line-server/src/store.rs) (60 s) is sized to absorb the gap without surfacing a spurious "thClaws offline" pairing code to the user.

### Client → relay: `POST /reply/:request_id`

Authenticated by `Authorization: Bearer <jwt>`. Body:
```json
{ "text": "agent response", "quick_reply": [
  { "label": "Approve", "data": "tool:allow:abc", "display_text": "Approve" },
  { "label": "Deny",    "data": "tool:deny:abc",  "display_text": "Deny" }
] }
```

`quick_reply` is optional. When present, the relay attaches LINE-native postback chips so the user can tap instead of typing approve/deny.

## Implementer guidance: prefer reply API over push

The LINE Messaging API has two outbound paths for `POST /reply/:request_id` to map to:

- **`POST /v2/bot/message/reply`** — uses the cached `replyToken` from the webhook. Free, unlimited within the channel's per-event quota.
- **`POST /v2/bot/message/push`** — direct push to a user. **Counts against the channel's monthly quota** (200/month on free tier; rapid kill if defaulted).

**Always try reply first.** Reply tokens expire 60 seconds after the webhook event and are single-use. Recommended logic:

> Call `POST /v2/bot/message/reply` if the cached `replyToken` is less than ~55 seconds old. Fall back to `POST /v2/bot/message/push` only when the reply token is expired or when the reply API returns an error.

The official relay implements this (`crates/line-server/src/routes/reply.rs`): reply-first, push fallback on any reply-API error. Third-party relays defaulting to push will exhaust the free quota in days under realistic load — verified empirically.

## Profile cache

The official relay maintains a `line_users` Postgres table:

```sql
CREATE TABLE line_users (
    line_user_id        TEXT PRIMARY KEY,
    display_name        TEXT NOT NULL,
    picture_url         TEXT,
    status_message      TEXT,
    language            TEXT,
    profile_fetched_at  TIMESTAMPTZ NOT NULL,
    first_seen_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_seen_at        TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
```

On every inbound `Message` / `Follow` webhook event, the relay calls `GET /v2/bot/profile/:userId` if the cached row is empty or older than 7 days, UPSERTs, and bumps `last_seen_at`. `/pair` response surfaces the cached profile so thClaws renders it on the sidebar pill.

Third-party relays MAY skip the profile cache — `/pair` response fields are optional. thClaws degrades gracefully.

## Surface-aware tools

A subtle gotcha for any relay's design: when a turn is driven by LINE, the user is **not at the local thClaws GUI**. Tools whose only output surface is the desktop modal (currently: `AskUserQuestion`) would hang the LINE conversation forever — the prompt lands on a screen the user can't see.

thClaws short-circuits `AskUserQuestion` on LINE-driven turns and returns a message instructing the model to fold the question into its LINE reply text. The user's next inbound LINE message becomes the answer naturally. See `crates/core/src/tools/ask.rs` `LINE_DRIVEN_TURN`.

Other surface-coupled tools are evaluated case-by-case as they're added. Relay implementers don't need to do anything — this is enforced on the client side.

## Permission gating

When the LINE bridge is connected, thClaws auto-switches `PermissionMode` to `LineGated` and routes all mutating-tool approval prompts to LINE as Quick Reply chips (`[✅ Approve] [🚫 Deny]`). Postbacks come back over the WS as `{ "kind": "postback", "data": "tool:allow:<id>" | "tool:deny:<id>" }`. On `LineDisconnect`, the previous local mode (Auto / Ask / Plan) is restored.

See [`permissions.md`](permissions.md) for `LineGated` and the broader approval-sink trait.

## Browser-chat surface (plan-10, v0.9.3+)

LINE bubbles are awkward for code blocks and long markdown
responses. plan-10 added a second relay surface — an external
browser SPA at `chat.thclaws.ai` — that connects to the same
desktop session over a parallel WebSocket. Both surfaces share
the same agent session and broker, but the desktop fans events to
each surface's channel independently and routes approvals to
whichever surface is currently open.

### Wire shape

```
LINE OA ────────────► POST /webhook (signed)
                        │ user types `/chat`
                        ▼
                    /reply with magic-link splash page
                      → https://chat.thclaws.ai/launch?token=...
                        │ user opens link in browser
                        ▼
                    GET /launch
                      → HTML splash that auto-POSTs back (dodges
                        LINE URL-preview crawler that would otherwise
                        burn the single-use token first)
                        ▼
                    POST /launch
                      → take_magic(token)              [Redis GETDEL]
                      → put_chat_session(...)          [10-min TTL]
                      → Set-Cookie: chat_sess=... HttpOnly Secure SameSite=Lax
                      → 303 to /chat
                        ▼
                    GET /chat  (SPA static HTML)
                        │
                        ▼ WebSocket upgrade
                    GET /chat-ws (cookie-authenticated)
                      ↔ desktop's WS broker via Channel::Browser
```

Cookie TTL: `SESSION_TTL_SECS = 10 * 60`. Three failed reconnects
without an OPEN trigger the "session expired" splash that points
the user back to `/chat` in LINE for a fresh link.

### Broker Channel enum

`crates/line-server/src/broker.rs`:

```rust
pub enum Channel {
    Desktop,   // the thClaws Rust client
    Browser,   // the chat.html SPA
}
```

The broker multiplexes events keyed by `(line_user_id, channel)`.
Inbound LINE webhooks publish to `Channel::Desktop`; inbound
browser keystrokes publish to `Channel::Browser`. Desktop's
`POST /chat-bridge/event` fans every `ViewEvent` to BOTH channels
so the browser sees the assistant's response too. The desktop's
`GET /chat-bridge/has-browser` returns `{browser_connected: bool}`
so the `LineApprover` can decide between browser modal vs LINE
Quick Reply at approval time.

### History replay

`POST /chat-bridge/event` also `XADD MAXLEN ~50` to a Redis stream
`chat_hist:{user}`. On every fresh browser connect, `/chat-ws`
sends a `session_info` envelope followed by `XRANGE - +` of the
stream, so the browser SPA can re-render the last ~50 events
(assistant deltas, tool calls, approval prompts) even on
mid-session reconnects. Empty history loads (new sessions) skip
the replay block entirely.

### Approval routing

`LineApprover::approve()` (in `crates/core/src/line/approver.rs`)
queries `has_browser_connected()` once per approval:

- `true` → publish `approval_request` envelope to
  `/chat-bridge/event`. The browser SPA's `case
  "approval_request"` shows an inline modal with **[Approve]
  [Deny]** buttons. User's click → `approval_decision` envelope
  back up the WS → desktop resolves the approver's oneshot. The
  desktop's own approval modal stays in sync (approving in either
  surface dismisses both).
- `false` → fall back to LINE `push_with_buttons` and Quick Reply
  postbacks. Identical wire shape to the legacy OA-only path.

### Inbound translation

`view_event_to_chat_envelope` (in `shared_session.rs`) maps
desktop `ViewEvent`s to browser-facing JSON. Notable
transformations:

- `AssistantTextDelta` runs through `crate::line::clean_for_stream`
  (strips ANSI + tool-narration glyphs) before emitting
  `assistant_delta`. Empty results after stripping are dropped so
  the browser doesn't render blank bubbles for tool-call-only
  chunks.
- `ErrorText` runs through `crate::providers::humanize_provider_error`
  before emitting the `error` envelope — same humanizer used by
  the desktop chat. See [`running-modes.md`](running-modes.md)
  for the humanizer's parsing rules.
- `ToolCallStart` / `ToolCallResult` → compact `tool_call_start` /
  `tool_call_result` envelopes (output text intentionally
  suppressed — browser chat mirrors the desktop chat tab's
  "the agent ran X, not what X returned" UX).
- `TurnDone` → `{type: "turn_done"}` ends the streaming bubble.

### Endpoints added by plan-10

| Method | Path | Purpose |
|---|---|---|
| GET | `/launch` | HTML splash that auto-POSTs the magic token |
| POST | `/launch` | Consumes token (Redis GETDEL), mints session cookie, 303 → `/chat` |
| GET | `/chat` | SPA static HTML (vendored `marked.min.js` + `purify.min.js` for markdown rendering) |
| GET | `/chat-ws` | WebSocket upgrade; cookie-authenticated; registers `Channel::Browser` |
| POST | `/chat/logout` | Deletes Redis `chat_sess:` + clears cookie + Postgres revoke |
| POST | `/chat-bridge/event` | Desktop publishes envelopes; XADDs to history stream |
| GET | `/chat-bridge/has-browser` | Desktop queries `is_browser_present` for approval routing |

Traefik `IngressRoute` at `chat.thclaws.ai` (203.150.118.93)
applies a `RateLimit` middleware on `/launch` (10/min/IP) to
defuse abuse, and a CSP of `script-src 'self' 'unsafe-inline'`
relaxed enough to run the splash's auto-submit script. See
`dev-plan/08-line-server-k3s/41-chat-ingress.yaml`.

### Session-expired UX

The browser SPA tracks `everOpened` (did we ever reach an OPEN
state?) and `sessionExpired` (3 reconnect failures without
intervening OPEN). Crossing both triggers a centered splash:

```
Your session expired.
Send /chat in LINE to @thClaws for a new chat link.
```

vs the never-opened case ("This chat link can't open") that
distinguishes "stale forwarded link" from "session timed out
mid-life". Either way the user's next action is the same — type
`/chat` in LINE again.

## Workspace-only

The official relay (`crates/line-server/`) is **server-side infrastructure** and never ships with the public thClaws release. `make sync-public` excludes it via `--exclude='line-server/'` in `Makefile`'s `RSYNC_CRATES_EXCLUDES`. Anyone self-hosting reimplements the protocol; the public surface is only the client-side `crates/core/src/line/` module and this doc.
