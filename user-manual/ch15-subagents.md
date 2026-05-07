# Chapter 15 — Subagents

The `Task` tool lets the main agent **delegate** to a sub-agent: a
fresh, isolated copy of the agent with its own tool scope and its own
goal. Useful for branching work (explore multiple approaches in
parallel), protecting the main context (run a noisy exploration in a
child), or specialisation (hand off to a "reviewer" agent with
read-only tools).

Subagents are part of the same process — they run in-memory, not as
separate OS processes. For true parallelism across processes, see
Agent Teams in Chapter 17.

## How it looks

```
❯ are the REST endpoints in this repo consistent with our naming
  convention in AGENTS.md?

[tool: Task: (agent=reviewer, prompt=Check every route under src/api …)] …
  [child:reviewer] Using Glob to find route files…
  [child:reviewer] Found 14 routes; 3 don't match the convention
[tool: Task] ✓

Looking at the sub-agent's findings:
- `src/api/v1/getUsers.ts` should be `get_users.ts` per convention.
- `src/api/v1/FetchOrders.ts` should be `fetch_orders.ts`.
- `src/api/v2/createPost.ts` should be `create_post.ts`.
```

The parent sees only the sub-agent's final text response, keeping the
intermediate tool chatter out of the main context.

## Agent definitions

Specific sub-agent behaviours are configured in
`.thclaws/agents/*.md` (project) or `~/.config/thclaws/agents/*.md`
(user):

```markdown
---
name: reviewer
description: Read-only code review with focus on conventions
model: claude-haiku-4-5
tools: Read, Glob, Grep, Ls
permissionMode: auto
maxTurns: 20
color: cyan
---

You are a code reviewer. Look at the code the parent points you at.
Flag:
- Naming inconsistencies with the project's `AGENTS.md` conventions.
- Missing tests alongside new code.
- Security-sensitive patterns (raw SQL, unsanitised input).

Return a concise bullet list. Don't propose fixes unless asked.
```

Frontmatter fields:

| Field | Purpose |
|---|---|
| `name` | Unique id (defaults to filename stem) |
| `description` | When-to-use text the parent sees |
| `model` | Model override for this agent |
| `tools` | Comma-separated tool allowlist |
| `disallowedTools` | Tool denylist |
| `permissionMode` | `auto` or `ask` (useful for "read-only" agents) |
| `maxTurns` | Max iterations (default 200) |
| `color` | Terminal colour for child output |
| `isolation` | `worktree` — give this agent its own git worktree (teams only) |

## Invoking

There are **two surfaces** for spawning a subagent:

### Model-driven — the `Task` tool

The parent agent invokes via `Task`:

```
Task(agent: "reviewer", prompt: "Check src/api for naming violations")
```

Typically you don't call this directly — you ask the parent a question in English and it decides. The model sees the list of available agents in its system prompt (rendered from the agent defs).

The `Task` tool **blocks the parent's turn** until the child finishes — the parent then sees the result as a tool result and continues. The child's intermediate reasoning isn't echoed into the parent's context (that's the whole point — keep main context clean), but the parent does pay for one full child run before its next move.

### User-driven — the `/agent` slash command (GUI)

In the desktop GUI, you can spawn a subagent **yourself** without going through the main agent's reasoning:

```
/agent translator แปลไฟล์ src/foo.md เป็นภาษาไทย
```

The chat surface confirms: `✓ spawned background agent 'translator' (id: side-abc123)`. While the translator runs:

- **Main agent keeps accepting input** — you can keep working with it. Side-channel agents run on their own tokio task, concurrent with main.
- **Main's history is unaffected.** The prompt + result never enter the main conversation. The side-channel result lands as a separate bordered card in the chat surface (amber border = running, green = done, red = error).
- **Cancel is independent.** Pressing the main agent's stop button does NOT kill side-channels. To cancel a side-channel use `/agent cancel <id>`.
- **Permission requests stay distinguishable.** If the side-channel asks for `Bash` approval while main is also doing tool calls, the approval modal labels each request with its source ("translator (background) wants to run Bash" vs "Main wants to run Bash") so you don't accidentally approve the wrong one.

```
/agents                    # list active background agents
/agent cancel side-abc123  # signal cancel to a specific channel
```

Side-channel agents share the same AgentDef registry — the named agents available via `Task` are the same ones available via `/agent`. Permissions, sandbox, MCP servers, and KMS access all behave identically.

`/agent` is the right surface when you know specifically what you want done, the work is well-scoped, and you want to keep doing other things in the main conversation while it runs. The model-driven `Task` tool stays the right choice when the parent agent's reasoning should decide whether/when to delegate.

## Recursion

A sub-agent can spawn further sub-agents up to `max_depth = 3` by
default. Each level is more scoped:

```
parent (depth 0)
 ├─ reviewer (depth 1) — "look at auth routes"
 │   └─ specialist (depth 2) — "audit JWT signing"
 └─ tester (depth 1) — "write integration tests"
```

The Task tool at depth 3 disables recursion to prevent runaway chains.

## Load order

`~/.config/thclaws/agents.json` → `~/.claude/agents/*.md` →
`~/.config/thclaws/agents/*.md` → `.thclaws/agents/*.md`. Later wins
by name.

### Plugin-contributed agents

Plugins (Chapter 16) can ship agent defs via an `agents` entry in
their manifest. Those dirs are walked **after** the standard ones and
merged **additively** — a plugin agent cannot override a user's or
project's existing agent with the same name. That means:

- You can install a plugin that ships a `reviewer` + `tester` +
  `architect` and all three become available via `Task(agent: "…")`
  and team spawns.
- If you later add your own `.thclaws/agents/reviewer.md`, it wins —
  the plugin's is ignored until you remove yours.
- `/plugin show <name>` lists the `agent dirs` the plugin contributes.

## Subagents vs Side-channel agents vs Teams

| | Task subagent | `/agent` side-channel | Teams |
|---|---|---|---|
| **Trigger** | Model decides via `Task` tool | User types `/agent` | Model uses `SpawnTeammate` |
| **Process model** | In-process, blocks parent's turn | In-process tokio task, concurrent with parent | Multiple `thclaws --team-agent` processes, tmux-orchestrated |
| **Parallelism** | Serial (recursion depth, not concurrency) | Concurrent with main, but each side-channel sequential | Truly concurrent |
| **Main's history** | Tool result lands in parent's context | Untouched — result is a separate side bubble | Untouched — teammates have their own sessions |
| **Isolation** | Shared sandbox | Shared sandbox | Optional git worktree per teammate |
| **Cancel** | Inherits parent's cancel | Independent — own cancel via `/agent cancel <id>` | Independent — `kill` the teammate process |
| **Messaging** | None — child returns a string | None — final result delivered as event | Filesystem mailbox + task queue |
| **Overhead** | Negligible | Negligible | High — spins up 1+ extra processes |
| **Use for** | Model-driven sub-problem reduction | User-driven side errands while main works | Parallel streams of long-running work |

Rule of thumb:

- **Default to model-driven `Task`** — let the parent agent decide when to delegate. Lowest ceremony.
- **Reach for `/agent`** when *you* (the user) know specifically what you want a specialist to do AND want to keep working in main while it runs. "Translate this file to Thai while I keep coding."
- **Reach for teams** when the work genuinely fans out across long-running parallel streams (build backend + frontend + ops in three parallel processes).
