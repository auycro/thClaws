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
    Browser,   // the chat.html SPA  (queued for removal — see below)
}
```

The broker multiplexes events keyed by `(line_user_id, channel)`.
Inbound LINE webhooks publish to `Channel::Desktop`; inbound
browser keystrokes publish to `Channel::Browser`. The desktop's
`GET /chat-bridge/has-browser` returns `{browser_connected: bool}`
so the `LineApprover` can decide between browser modal vs LINE
Quick Reply at approval time.

**v0.1.19 (line-server) Channel::Browser fan-out moved to Redis
Stream.** Pre-fix the broker also routed every desktop-emitted
`ViewEvent` to `Channel::Browser` via in-pod `mpsc` (fast) plus a
Redis pubsub fallback (slow), and `/chat-ws` registered with the
broker to receive that fan-out. As of [`1036c5d`](https://github.com/thClaws/thClaws/commit/1036c5d):

- `POST /chat-bridge/event` calls `chat_hist_push` (XADD only) —
  no `broker.route(Channel::Browser, …)` anymore.
- `/chat-ws` does NOT register/deregister with the broker for
  Browser. After the history-replay block (next section), it
  spawns a tail task that `XREAD BLOCK`s on the same stream and
  forwards bytes to the WS via a local `mpsc`.
- `Channel::Desktop` still routes through the broker — only the
  Browser fan-out moved.

Why: cluster runs N replicas, Traefik load-balances POSTs across
them. Pre-fix, a `POST` landing on pod-A while the browser's WS
held on pod-B took the slow Redis-pubsub path and could arrive
out of order with a subsequent `POST` that LB'd back to pod-B.
Redis Stream serialises XADDs server-side with monotonic stream
IDs, so ordering is global across pods regardless of which replica
handled the POST. Pre-fix the cluster had to stay at 1 replica;
post-fix it's HA-safe.

Latency cost: ~1-2ms per event for the Redis round-trip (vs ~10µs
in-pod mpsc). At Anthropic's ~30 tok/s emission cadence (33ms
between tokens), unmeasurable.

Failure-mode shift: a Redis outage now drops Browser-bound chat
broadcast E2E. Pre-fix the local mpsc partially survived a Redis
outage (Desktop channel kept working, Browser kept working
in-pod). Cluster currently runs a single Redis replica
(`thclaws-line-redis` PVC); follow-up if we want 5-nines on the
broadcast path is to layer in Redis Sentinel or a managed Redis.

Plan-10 follow-up (tracked): collapse the `Channel::Browser`
variant out of `broker.rs` once the new Stream path is confirmed
in production. Keeping it in the enum for now to avoid an API
churn while the migration settles.

### History replay + tail subscribe

Single Redis Stream `chat_hist:{line_user_id}` is the source of
truth for everything the browser SPA renders.

**Write path.** `POST /chat-bridge/event` issues `XADD MAXLEN ~50`
on every event (assistant deltas, tool calls, approval prompts,
session-info envelopes). The `MAXLEN ~50` keeps each user's
stream bounded.

**Read path.** When `/chat-ws` upgrades:

1. `chat_hist_load_with_ids` runs `XRANGE - +` and replays the
   ~50 history entries to the WS as a single batch, capturing
   each event's stream ID.
2. The last replayed stream ID becomes the `tail_cursor`.
3. A tail task spawns on a dedicated `redis::aio::Connection`
   (the multiplexed connection used elsewhere can't hold a
   blocking command). The task loops:
   `chat_hist_tail` → `XREAD BLOCK 0 STREAMS chat_hist:{user} <cursor>`
   → forward parsed events to a local `mpsc` → advance `cursor`
   to the last seen ID.
4. The WS's `select!` main loop drains that mpsc as a
   single-producer source — ordering preserved by construction.
5. On WS disconnect, the tail task is aborted and the dedicated
   Redis connection drops.

Empty history loads (new sessions) skip the replay block but
still spawn the tail task — anything XADDed after connect is
delivered live. Mid-session reconnect re-runs the replay (so the
user sees the last ~50 events again) and resumes the tail from
whatever stream ID is most recent at that moment.

Helpers `parse_chat_hist_entries` and
`parse_chat_hist_entries_with_ids` (in `store.rs`) factor the
shared XRANGE / XREAD response shape so the load and tail paths
stay byte-identical.

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

## File uploads (v0.9.6)

LINE attachments (image / video / file) and browser-chat paperclip uploads both land in `<workspace>/uploads/` via the same `crate::uploads` helpers used by `--serve`. The wire path differs by surface:

**LINE path.** When the LINE webhook fires with a message of type `image` / `video` / `file`, the relay POSTs an `upload_ref` envelope down the desktop's WS:

```json
{
  "type": "upload_ref",
  "request_id": "...",
  "line_message_id": "5234...",
  "filename": "IMG_1234.jpg",
  "size": 2841234,
  "mime": "image/jpeg"
}
```

The desktop calls LINE's Messaging API `GET /v2/bot/message/<id>/content` with the channel access token, streams the bytes to `<workspace>/uploads/<unique_path(filename)>`, then dispatches the same `render_upload_message("line", &saved)` synthetic chat message the `--serve` path uses. The agent sees a uniform event — it doesn't know whether the bytes came from a phone, a browser drag-and-drop, or `POST /upload`.

**Browser-chat path.** `chat.thclaws.ai`'s SPA submits via `POST /chat-bridge/upload` (multipart, same shape as `--serve`'s `/upload`). The relay forwards the bytes to the desktop over the WS as a binary frame tagged with the metadata; the desktop saves them locally and emits the synthetic message identically.

**Caps + collision.** Both paths reuse the constants from `crate::uploads` — `UPLOAD_MAX_BYTES = 25 * 1024 * 1024`, `UPLOAD_MAX_FILES = 5`, `_n` suffix on collision. Exceeding the byte cap surfaces in LINE as a relay-sent error bubble; in the browser as a toast.

**Channel access token.** The LINE fetch needs the OA's long-lived token. The relay holds it and stamps the desktop's `upload_ref` envelope with a short-lived signed URL (S3-style presigned shape) — the desktop never sees the raw token. Today's impl is "relay-fetches-then-streams" since presigned LINE URLs don't exist for `/message/<id>/content`; a future tweak could route via S3-cached objects to offload the relay.

**AGENT.md hook.** `<workspace>/uploads/AGENT.md` is consulted on the next agent turn through the normal CLAUDE.md / AGENT.md cascade — no special wiring at the upload layer. The synthetic chat message is what triggers the agent loop; whatever the loop reads from `AGENT.md` decides what happens next.

## Workspace-only

The official relay (`crates/line-server/`) is **server-side infrastructure** and never ships with the public thClaws release. `make sync-public` excludes it via `--exclude='line-server/'` in `Makefile`'s `RSYNC_CRATES_EXCLUDES`. Anyone self-hosting reimplements the protocol; the public surface is only the client-side `crates/core/src/line/` module and this doc.
