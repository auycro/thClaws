# Chapter 21 — LINE chat & web browser bridge

Drive thClaws from your phone — either as a LINE conversation
(via the `@thClaws` OA bot) or as a chat surface in any web
browser. Both routes share the same Rust agent loop on your
desktop; only the surface changes. Added in v0.9.0+ across the
plan-07 / plan-08 / plan-10 series.

## Why bother

- Approve `Bash` commands from your phone while the desktop runs
  unattended.
- Continue a chat away from your laptop — type on your phone, the
  desktop's full tool registry (Bash, Edit, KMS, MCP, skills)
  executes locally.
- Drive long-running tasks without leaving your machine docked at
  the desk.

The desktop never goes away — your code, secrets, and tools stay
local. The phone / browser surfaces are read+input bridges only.

## How it works (one paragraph)

A small Axum service at `line.thclaws.ai` (and `chat.thclaws.ai`
for the browser variant) holds a WebSocket connection from your
desktop and routes LINE inbound messages / browser keystrokes to
it. The desktop runs the agent unchanged and fans every assistant
delta, tool call, and approval prompt back through the same WS so
the phone or browser sees the conversation as it streams. Sessions
in LINE are pinned to your LINE user id; sessions in the browser
are authenticated with a one-time magic link the LINE bot mints.

## Pairing your phone (LINE)

One-time setup:

1. **Add the LINE OA** — scan the QR code at
   [`thclaws.ai/line`](https://thclaws.ai/line) (or search for
   `@thClaws` in LINE).
2. In thClaws, open Settings → **LINE** → **Pair phone**. The
   modal shows a 6-character code (e.g. `KJ4-9P2`).
3. Send that code to the LINE OA. It replies "Paired ✓ as
   *<your-line-display-name>*".
4. The sidebar's LINE chip lights up green. You're connected.

After pairing, every message you send to `@thClaws` flows into
thClaws's chat session on the desktop. The agent runs there,
streams responses back, and the LINE bot relays them as bubbles.
Tool calls that need approval (Bash, Edit, Write) trigger LINE
Quick Reply chips — tap **[Approve]** or **[Deny]** from the
phone.

## LINE OA commands

Once paired, the LINE bot recognizes a small set of text
commands. Anything else is treated as a chat message.

| You type | What happens |
|---|---|
| `/chat` | Mints a magic link to the browser chat (see below) and replies with it. The link is single-use, 10-min TTL |
| `/pair` | Re-issues a pairing code — useful if you disconnect thClaws then want a new session |
| `/unpair` | Forgets this LINE user id. Next message gets a fresh pairing code, not a chat |
| `/status` | Prints whether thClaws is reachable from the relay right now |
| anything else | Routed to the desktop's chat session as a normal user message |

If thClaws is paired but the desktop is offline (laptop closed,
network dropped), the bot replies "thClaws is offline" rather
than swallowing your message silently.

## Browser chat (the `/chat` path)

LINE bubbles are great for short approvals and quick prompts but
get awkward for code blocks, long responses, and markdown
rendering. Send `/chat` to the OA and you get back a magic link:

```
https://chat.thclaws.ai/launch?token=...
```

Open it in any browser — the link auto-redirects through a splash
page (which exists to dodge LINE's URL-preview crawler that would
otherwise burn the token before you tapped). After the redirect
you land on a full-fidelity chat surface:

- Sidebar shows your session id, sign-out button, and a live
  "browser connected" indicator on the desktop side.
- Assistant responses render as markdown with syntax-highlighted
  code blocks (via vendored marked.js + DOMPurify — all
  rendering stays in the browser; no remote loaders, no eval).
- History replays automatically on connect — even mid-session
  reconnects pick up where you left off (last ~50 messages,
  served from a Redis stream on the relay).
- Tool approvals open an inline modal with **[Approve] [Deny]**
  buttons instead of routing to LINE Quick Replies.
- Sessions expire after 10 minutes of idle — three reconnect
  failures in a row trigger a "session expired" splash that
  points you back to `/chat` in LINE for a fresh link.

The browser link is **per-session, single-use, HTTPS only,
HttpOnly cookie**. Sharing it is identical to handing someone
your desktop session — don't.

## Rich-menu shortcut (v0.9.3+)

If your phone shows the LINE OA's rich menu (the bottom toolbar
with custom buttons), it has two pinned buttons:

- **Chat** — equivalent to typing `/chat`. One tap to get a
  magic link to the browser chat.
- **Pair** — equivalent to typing `/pair`. Quick re-issue of a
  pairing code if you disconnected.

Operators who deploy their own LINE OA can install the rich menu
with the `dev-plan/08-line-server-k3s/rich-menu-setup.sh` script —
see [`docs/line-rich-menu-setup.md`](../../docs/line-rich-menu-setup.md)
for the full setup walk-through.

## Approvals from the phone or browser

When the agent calls a tool that needs approval (Bash, Edit,
Write — see [Chapter 5](ch05-permissions.md)) and a phone/browser
session is active, the approval prompt routes through whichever
surface is currently open:

- **Browser chat open:** modal pops up with the tool name, full
  argument preview, and **[Approve] [Deny]** buttons. The desktop
  Approval modal stays in sync — approving in either surface
  dismisses both.
- **Browser chat NOT open:** falls back to LINE OA Quick Reply.
  The bot pushes a bubble like:

  ```
  thClaws wants to run:
    bash -c "ls -la ~/Downloads"

  [Approve]  [Deny]
  ```

  Tap a chip; the answer flows back to the desktop within ~1 s.
- **Neither surface open:** the desktop's own approval modal
  pops up as usual. Phone/browser routing is additive, not a
  replacement.

## Privacy and trust boundary

- **Desktop never proxies upstream LLM calls through the relay.**
  Your prompts go from the desktop straight to Anthropic / OpenAI
  / etc. The relay only carries the user-facing messages between
  the surfaces and the desktop.
- **The relay can see message content** in transit (it has to
  route it). Host it yourself if you don't want a third party
  reading your prompts — the relay binary is `crates/line-server/`
  in the workspace fork; the public OSS distribution doesn't ship
  it. See plan-08 in the workspace `dev-plan/` for the k3s
  deployment shape.
- **Tokens / API keys never leave the desktop.** The relay holds
  one LINE channel secret (for signature verification) and a
  Postgres-stored user profile cache (name + LINE user id) per
  paired user — nothing more.
- **LINE pairing tokens are single-use, 10-min TTL, hashed
  server-side.** A stolen pairing code is useless once the OA
  has emitted the "Paired ✓" reply.

## Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| `/chat` link shows "expired" on first tap | LINE's URL preview crawler consumed the token | Open the link from the LINE chat directly, not by tapping a forwarded copy |
| LINE bot replies "thClaws is offline" | Desktop's WS disconnected (sleep, network) | Bring the desktop online; pairing persists |
| Browser chat freezes "Opening thClaws Chat…" | Browser blocked the inline auto-submit script | Confirm the CSP allows `script-src 'self' 'unsafe-inline'` on `/launch` |
| LINE Quick Reply buttons don't appear on approval | Browser chat is also open — approval went there instead | Either approve in the browser or close the browser tab and the next approval falls back to LINE |
| Pairing code stays "(none)" after typing it | Code was older than 10 min, or already used | Open the Pair modal again to mint a fresh code |
| "browser connected" pill doesn't appear on the desktop | Magic link token TTL elapsed before you opened it | Send `/chat` again from LINE for a fresh link |

## Status command on the desktop

`make line-status` (from the workspace root) prints a per-user
status table joining Postgres profiles with Redis presence
flags — useful for operators running their own LINE relay:

```
$ make line-status
user_id            paired  present  browser  last_seen
U1a2b3...          ✓       ✓        -        2 min ago
U9z8y7...          ✓       -        -        3 days ago
```

`paired` = ever-paired, `present` = WS connected right now,
`browser` = `/chat` browser session active, `last_seen` =
most recent webhook activity.

## What's NOT in this chapter

- Internal architecture (broker channel multiplex, WS protocol,
  Redis stream layout) — see the technical manual's
  [`line-bridge.md`](../../thclaws-technical-manual/line-bridge.md).
- LINE OA setup from scratch (channel secret, webhook URL, rich
  menu install) — operator-side work documented in
  [`docs/line-rich-menu-setup.md`](../../docs/line-rich-menu-setup.md)
  and the plan-08 workspace docs.
- Cloud gateway (paid SaaS proxy) — that's plan-09, still in
  planning; see the workspace `dev-plan/09-cloud-gateway.md`.
