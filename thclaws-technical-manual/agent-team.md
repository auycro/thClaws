# Agent Team

Multi-process parallel agents coordinated through filesystem mailboxes. The user (or model, via `TeamCreate`) defines a team — named members with roles, prompts, and optional `isolation: "worktree"`. The lead spawns each teammate as a separate **`thclaws --team-agent <name>` subprocess**; teammates poll their inbox + claim from a shared task queue, run their work, and report back via `SendMessage`. Idle / shutdown coordination via typed protocol messages embedded in inbox text. Worktree-isolated teammates work on dedicated `team/<name>` git branches; the lead's `TeamMerge` aggregates them back into the main branch.

Distinct from **subagent** (in-process recursive `Task` tool — same process, shared tool registry, depth-tracked) and from **TaskCreate** (in-memory progress scratchpad, no LLM involvement). Agent team is the only thClaws primitive where work happens in a separate process; everything else lives in the agent loop's address space.

This doc covers: the three-tier delegation hierarchy (vs subagent vs TaskCreate), on-disk layout under `.thclaws/team/`, the data model (`TeamConfig` / `TeamMember` / `TeamMessage` / `TeamTask` / `AgentStatus` / `ProtocolMessage`), all 10 team tools (`TeamCreate` / `SpawnTeammate` / `SendMessage` / `CheckInbox` / `TeamStatus` / `TeamTaskCreate/List/Claim/Complete` / `TeamMerge`), the lead lifecycle (registration, inbox poller, `handle_team_messages`, EOF cleanup), the teammate lifecycle (subprocess spawn → env vars → system-prompt addendum → inbox loop → idle notification → shutdown protocol), tool-registry differences (lead vs teammate), worktree auto-creation + `TeamMerge`, tmux integration, the lead/teammate hard-blocks in BashTool + the merge-conflict carve-out for Write/Edit, agent-name validation + shell-escape (M6.34 TEAM1+TEAM2), scoped teammate kill (M6.34 TEAM3), `team_grounding_prompt` framing under three provider conditions, the `agent_team.md` / `lead.md` / `worktree.md` prompts, JSON file locking + `with_file_lock_shared` reads, and the testing surface.

**Source modules:**
- `crates/core/src/team.rs` — `TeamConfig` / `TeamMember` / `TeamMessage` / `TeamTask` / `AgentStatus` / `ProtocolMessage`, `Mailbox` (single-file inbox per agent), `TaskQueue` (per-task files + `_hwm`), all 10 team tools, `register_team_tools`, `is_team_lead` / `set_is_team_lead`, `set_lead_team_dir` / `kill_my_teammates` (M6.34 TEAM3), `is_valid_agent_name` (M6.34 TEAM1), `shell_escape` (M6.34 TEAM2), `lead_resolving_merge_conflict` (Write/Edit carve-out gate), `with_file_lock` / `with_file_lock_shared`, `has_tmux` / `is_inside_tmux`, `make_idle_notification` / `parse_protocol_message`
- `crates/core/src/repl.rs::run_repl_with_state` — CLI lead startup (registers team tools, sets `is_team_lead`, captures `set_lead_team_dir`), CLI teammate event loop (`if let Some(ref agent_name) = team_agent_name` block at ~line 2737 — inbox polling, task claiming, agent.run_turn per message, idle notification, shutdown protocol handling), CLI lead inbox poller (background tokio task), `process_team_messages!` macro (XML-framed teammate prompt → agent.run_turn), `kill_my_teammates()` at EOF
- `crates/core/src/shared_session.rs::run_worker` — GUI lead startup (mirrors CLI), `team_grounding_prompt` (provider-aware system prompt addendum), GUI lead inbox poller (`if team_enabled` tokio::spawn), `handle_team_messages` (GUI equivalent of CLI's `process_team_messages!`)
- `crates/core/src/gui.rs` — `team_send_message` IPC (user → teammate inbox), `team_list` IPC (Team-tab status snapshot), `team_enabled_get` / `team_enabled_set` IPC (project config flag flip)
- `crates/core/src/tools/bash.rs` — `lead_forbidden_command` (lead destructive-op block), `teammate_forbidden_command` (cross-branch reset block), `ref_resets_to_different_branch` (heuristic), `is_destructive_command` (general warning class), `is_teammate_process` (env-var probe)
- `crates/core/src/tools/write.rs` + `crates/core/src/tools/edit.rs` — lead-block + `lead_resolving_merge_conflict` carve-out
- `crates/core/src/default_prompts/agent_team.md` — embedded teammate system-prompt addendum (template)
- `crates/core/src/default_prompts/lead.md` — embedded lead coordination rules
- `crates/core/src/default_prompts/worktree.md` — embedded worktree-specific rules (appended to teammate prompt when `THCLAWS_IN_WORKTREE=1`)
- `crates/core/src/agent_defs.rs` — `AgentDefsConfig::load_with_extra` provides per-name agent definitions consumed by both `SpawnTeammate` (instructions injected into spawn prompt + system prompt) and the in-process `Task` subagent

**Cross-references:**
- [`subagent.md`](subagent.md) — explicitly contrasts: same process / recursion ceiling / shared tools vs subprocess / mailbox / per-team git branch
- [`permissions.md`](permissions.md) — teammates spawn with `--accept-all` (auto-approve) + `--permission-mode auto`; `is_team_lead()` + `is_teammate_process()` static flags drive the BashTool / Write / Edit lead/teammate guards
- [`agentic-loop.md`](agentic-loop.md) — every teammate subprocess runs the same `Agent::run_turn` loop; the lead's `handle_team_messages` invokes the same loop with an XML-framed teammate prompt
- [`sessions.md`](sessions.md) — each teammate subprocess has its own session under its own `.thclaws/sessions/` (tmux pane sessions are independent of the lead's session)
- [`built-in-tools.md`](built-in-tools.md) §Team — concise tool surface

---

## 1. Three-tier delegation hierarchy

| | TaskCreate | Task (subagent) | SpawnTeammate (this doc) |
|---|---|---|---|
| Mechanism | In-memory `TaskStore` | Recursive in-process `Agent::run_turn` | `thclaws --team-agent <name>` subprocess |
| LLM involvement | None | Same process, shared registry | New process, fresh registry, fresh session |
| Coordination | None | Tool result text → caller's history | Filesystem mailboxes (`.thclaws/team/inboxes/<name>.json`) |
| State sharing | None (own store) | Inherits parent's tools / system / approver / cancel | Subprocess inherits env + `--team-dir` flag; otherwise independent |
| Crash blast radius | Process-local | Same process — child panic kills parent | Subprocess crash; lead observes via mailbox status (or doesn't — see TEAM-M4 deferred gap) |
| Spawn cost | Microseconds | Microseconds | Hundreds of ms (fork + exec + provider handshake) |
| Visibility | CLI `/tasks` | Single chat surface; tool result text | Team tab pane per teammate, separate session JSONL, output log |
| Use when | Ephemeral progress tracking | Bounded subtask, fresh history needed | Long-lived parallel workers; cost amortized |

The system prompt's `team_grounding_prompt` (§14) explicitly tells the model when each tier is appropriate — and pushes back hard against Claude Code training-data bias toward Anthropic's SDK-side teams primitive (`agent_toolset_20260401`), which is **invisible to thClaws**.

---

## 2. On-disk layout

Everything under `<project>/.thclaws/team/`:

```
.thclaws/team/
├── config.json              # TeamConfig: members + roles + isolation
├── inboxes/
│   ├── lead.json            # JSON array of TeamMessage
│   ├── frontend.json
│   ├── backend.json
│   └── …
├── agents/
│   ├── lead/
│   │   ├── status.json      # AgentStatus: { agent, status, current_task, last_heartbeat }
│   │   └── output.log       # captured stdout/stderr (GUI Team-tab tail)
│   ├── frontend/
│   │   ├── status.json
│   │   └── output.log
│   └── …
└── tasks/
    ├── _hwm                 # high water mark (auto-incrementing task id counter)
    ├── 1.json               # TeamTask: subject + description + status + owner + blocked_by
    ├── 2.json
    └── …
```

Plus per-file `.lock` siblings created by `with_file_lock` / `with_file_lock_shared` (TEAM-M6 deferred — these accumulate forever today).

Plus, when worktree isolation is on:
```
<project>/.worktrees/
├── frontend/                # checked out on branch team/frontend
└── backend/                 # checked out on branch team/backend
```

The `.thclaws/team/` directory is **NOT** under any user-home path — it's per-project. Distinct from Anthropic's SDK convention of `~/.claude/teams/` + `~/.claude/tasks/` (which the system prompt explicitly warns the model NOT to reference).

---

## 3. Data model

### `TeamConfig`

```rust
pub struct TeamConfig {
    pub name: String,
    pub description: Option<String>,
    pub created_at: u64,
    pub lead_agent_id: String,        // hardcoded "lead" today
    pub members: Vec<TeamMember>,
    pub agents: Vec<LegacyAgentDef>,  // legacy compat — read but not written
}
```

`TeamConfig::load` migrates the legacy `agents: [...]` shape into the new `members: [...]` shape on first read (legacy migration test pins this).

### `TeamMember`

```rust
pub struct TeamMember {
    pub name: String,                 // validated by is_valid_agent_name (TEAM1)
    pub prompt: String,
    pub role: String,
    pub color: Option<String>,
    pub cwd: Option<String>,
    pub is_active: bool,              // flipped true by SpawnTeammate
    pub tmux_pane_id: Option<String>, // unused today; reserved
    pub isolation: Option<String>,    // Some("worktree") → auto-create .worktrees/<name>
}
```

### `TeamMessage`

```rust
pub struct TeamMessage {
    pub id: String,                   // uuid v4
    pub from: String,
    pub text: String,
    pub timestamp: u64,
    pub read: bool,
    pub summary: Option<String>,      // first ~8 words for terminal log
    pub _content: Option<String>,     // legacy alias (read, not written)
    pub _to: Option<String>,          // legacy alias
}
```

`content()` accessor returns `text` if present, else falls back to the legacy `_content`.

### `TeamTask`

```rust
pub enum TaskStatus { Pending, InProgress, Completed }

pub struct TeamTask {
    pub id: String,                   // sequential string from _hwm
    pub subject: String,
    pub description: String,
    pub owner: Option<String>,        // pre-assigned (reserved) or claimed
    pub status: TaskStatus,
    pub blocks: Vec<String>,
    pub blocked_by: Vec<String>,
    pub created_at: u64,
    pub updated_at: u64,
}
```

### `AgentStatus`

```rust
pub struct AgentStatus {
    pub agent: String,
    pub status: String,               // "alive" | "idle" | "working" | "stopped" (free-form)
    pub current_task: Option<String>,
    pub last_heartbeat: u64,          // unix epoch; written but not consumed (TEAM-M7 deferred gap)
}
```

### `ProtocolMessage` — embedded JSON in `TeamMessage.text`

```rust
pub enum ProtocolMessage {
    IdleNotification {
        from: String,
        idle_reason: Option<String>,        // "available" | "interrupted" | "failed"
        completed_task_id: Option<String>,
        completed_status: Option<String>,   // "completed" | "blocked" | "failed"
        summary: Option<String>,
    },
    ShutdownRequest { from: String },
    ShutdownApproved { from: String },
    ShutdownRejected { from: String, reason: String },
}
```

`parse_protocol_message(text)` runs `serde_json::from_str` and returns `Some` if it parses — used by both lead and teammate inbox handlers to short-circuit protocol messages from human-readable text.

---

## 4. The 10 team tools

All registered via `register_team_tools(&mut registry, my_name)` at startup. Lead vs teammate get different filtered subsets (§7).

### `TeamCreate` (lead)

```json
{
  "name": "<team name>",
  "description": "<optional>",
  "agents": [
    { "name": "frontend", "role": "Build the UI", "prompt": "...", "isolation": "worktree" },
    { "name": "backend",  "role": "Build the API", "prompt": "..." },
    { "name": "qa",       "role": "Tests" }
  ]
}
```

Writes `config.json`, runs `init_agent` per member (creates inbox + status="alive"), initializes `tasks/` directory. Returns a confirmation string + lead coordination rules + worktree-isolation reminder for any members declared `isolation: "worktree"`.

`requires_approval = true`. M6.34 TEAM1 rejects any agent name that fails `is_valid_agent_name` BEFORE any side effects. The tool also flags prompts containing literal `git worktree add …` (the LLM hallucination that prompted the declarative `isolation` field).

### `SpawnTeammate` (lead — and teammates, see TEAM-L15 deferred gap)

```json
{ "name": "frontend", "prompt": "Initial task instructions", "cwd": "<optional>" }
```

Builds the spawn command:
```
EDITOR=true VISUAL=true GIT_EDITOR=true GIT_SEQUENCE_EDITOR=true \
THCLAWS_IN_WORKTREE=1 \   # only when worktree isolation fires
THCLAWS_PROJECT_ROOT='/abs/path/to/project' \
cd '/abs/path/to/.worktrees/frontend' && \
'/Users/jimmy/.cargo/bin/thclaws' --cli \
  --team-agent 'frontend' \
  --team-dir '/abs/path/to/.thclaws/team' \
  --permission-mode auto --accept-all \
  --model 'claude-sonnet-4-6'   # only when agent_def specifies
```

M6.34 TEAM2 wraps every interpolated value in `shell_escape` (POSIX single-quote with `'\''` for embedded quotes). Pre-fix `name = "foo; rm -rf $HOME ;"` would inject a shell statement past the approval modal.

Spawn dispatch:
- Inside tmux → `tmux split-window -h -d` in current session
- Outside tmux + `tmux` available → `new-session -d -s thclaws-team` (or split if exists)
- No tmux → background process via `shell_command_sync.spawn()` with stdout/stderr captured to `agents/<name>/output.log`

Initial prompt is delivered to the teammate as the first inbox message — the teammate's poll loop picks it up and processes it as a "user message" in the agent loop.

`requires_approval = true`.

### `SendMessage`

```json
{ "to": "<name>" | "*" | "lead", "text": "...", "summary": "<optional>" }
```

Writes a `TeamMessage` to the recipient's inbox under exclusive lock. `to: "*"` broadcasts to every team member EXCEPT the sender, AND additionally to `lead` if the sender isn't the lead (one message per recipient).

Rejects sending to agents whose status is `"stopped"` (with "Use SpawnTeammate to respawn it first" hint).

M6.34 TEAM1 validates `to` (allowing the `*` sentinel) before any path construction.

`requires_approval = false` — teammates run with `--accept-all` so this is moot for them; for the lead in interactive mode, every SendMessage skips the approval modal (TEAM-M11 deferred UX call).

### `CheckInbox`

```json
{}
```

Reads all unread messages, marks them as read on disk, returns them as a single text block joined by `---`. Format:
```
From: backend
<message text>

---

From: qa
<message text>
```

### `TeamStatus`

```json
{}
```

Returns:
```
## Agents
  frontend — working (task: 3)
  backend — idle (task: -)
  qa — alive (task: -)

## Tasks (5 total: 1 pending, 1 in progress, 3 completed)
  [1] Completed — Scaffold project (owner: backend)
  [2] Completed — Install deps (owner: backend)
  [3] InProgress — Implement /healthz (owner: frontend)
  [4] Pending — Write integration test (owner: qa)
  [5] Completed — Lint pass (owner: qa)
```

`AgentStatus.last_heartbeat` is written but not surfaced in the rendered output today (TEAM-M7 deferred gap — could mark "stale" agents).

### `TeamTaskCreate` (lead)

```json
{
  "subject": "Implement /healthz",
  "description": "Add GET /healthz returning {ok: true}; verify with curl",
  "owner": "backend",                    // optional; reserves task
  "blocked_by": ["1", "2"]               // optional; dependency ids
}
```

Auto-incrementing id from `_hwm` (string-encoded u64). Validates `owner` is a real team member (typo guard — pre-fix a typo would silently leave the task unclaimable forever).

### `TeamTaskList`

```json
{ "status": "pending" | "in_progress" | "completed" }   // optional filter
```

### `TeamTaskClaim` (teammates only — removed from lead's registry)

```json
{ "task_id": "3" }
```

Validates:
- Task is `Pending`
- Task is unowned OR pre-assigned to claiming agent
- Every `blocked_by` dep is `Completed`
- Claimer is not currently busy with another `InProgress` task (busy-check is OUTSIDE the file lock — TEAM-L1 deferred edge case for same-agent-id-in-multiple-processes)

`claim_next` is the auto-pick variant: walks `Pending` list, skips reserved-for-other tasks, calls `claim()` on each candidate; returns the first success. Used by the CLI teammate's poll loop after no inbox messages.

### `TeamTaskComplete` (teammates only)

```json
{ "task_id": "3", "summary": "added handler + test" }
```

Marks the task `Completed`, then sends an `IdleNotification(completed_task_id, "completed", summary)` to `lead`'s inbox so the lead knows to coordinate next steps.

### `TeamMerge` (lead only)

```json
{
  "into": "main",                   // default: current branch
  "only": ["frontend", "backend"],  // optional allow-list
  "cleanup": false,                 // remove worktree + branch on success
  "dry_run": false
}
```

For each `team/*` branch:
1. `git rev-list --count <into>..<branch>` — commits ahead
2. If 0 ahead and `cleanup`: remove worktree + delete branch, skip merge
3. If `dry_run`: report only
4. `git merge --no-ff --no-edit <branch>`
5. On failure: collect conflict files via `git diff --name-only --diff-filter=U`, `git merge --abort`, suggest delegating fix to the responsible teammate, **stop on first failure** (don't continue to next branch — lead deals with the conflict before more merges)

`requires_approval = true`.

---

## 5. Mailbox (single-file inbox per agent)

```rust
pub struct Mailbox {
    pub team_dir: PathBuf,
}
```

Public methods:
- `init_agent(name)` — create dirs + empty inbox + status="alive"
- `read_mailbox(name)` / `read_unread(name)` — shared lock read
- `write_to_mailbox(name, msg)` — exclusive lock RMW
- `mark_as_read(name, ids)` — exclusive lock RMW
- `write_status(name, status, task)` / `read_status(name)` — M6.34 TEAM6 locks
- `all_status()` — reads every `agents/<*>/status.json`
- `output_log_path(name)` — for teammate stdout/stderr capture
- `task_queue()` — returns a `TaskQueue` rooted at `team_dir/tasks/`

Inbox storage is a **single JSON array per agent** (not append-log). Every write is read-modify-write under exclusive lock — `O(n)` per message in inbox size. Compaction is not implemented (TEAM-M5 deferred gap).

The legacy `_content` / `_to` fields on `TeamMessage` are read-only compat aliases (`#[serde(skip_serializing, alias = "content"|"to")]`) — old inbox files keep working, new writes use the canonical names.

---

## 6. TaskQueue (per-task files + `_hwm` counter)

```rust
pub struct TaskQueue {
    tasks_dir: PathBuf,    // team_dir/tasks
}
```

- `_hwm` — exclusive-lock counter file; `next_id()` increments + writes back atomically
- `<id>.json` — one file per task; M6.34 TEAM6 wraps `get` + `list` in shared locks
- `claim(id, agent)` — exclusive lock, validates pending + unblocked + not-reserved-for-other + claimer-not-busy
- `complete(id, agent)` — exclusive lock, validates owner matches
- `release(id)` — exclusive lock, owner=None + status=Pending (currently unused by any tool)
- `claim_next(agent)` — iterates pending list, tries `claim` on each; swallows errors as "race" (TEAM-L7 deferred)

The busy-check inside `claim` lives OUTSIDE the file lock — only matters when the same `agent_id` somehow runs in two processes. Single-process is fine.

---

## 7. Tool registry differences (lead vs teammate)

`register_team_tools` registers all 9 mailbox tools + `TeamMerge` (lead only — `if name == "lead"`). Then `repl.rs::run_repl_with_state` filters:

| Tool | Lead | Teammate |
|---|---|---|
| `TeamCreate` | ✓ | ✓ (TEAM-L15 deferred — probably should be lead-only) |
| `SpawnTeammate` | ✓ | ✓ (TEAM-L15 deferred — same) |
| `SendMessage` | ✓ | ✓ |
| `CheckInbox` | ✓ | ✓ |
| `TeamStatus` | ✓ | ✓ |
| `TeamTaskCreate` | ✓ | ✓ |
| `TeamTaskList` | ✓ | ✓ |
| `TeamTaskClaim` | ✗ | ✓ |
| `TeamTaskComplete` | ✗ | ✓ |
| `TeamMerge` | ✓ | ✗ |
| `AskUserQuestion` | ✓ | ✗ (no human watching) |
| `EnterPlanMode` / `ExitPlanMode` | ✓ | ✗ (plan mode is interactive) |

The `team_essential_tools` set used for `--allowed-tools` filtering:
```rust
["SendMessage", "CheckInbox", "TeamStatus",
 "TeamCreate", "SpawnTeammate",
 "TeamTaskCreate", "TeamTaskList",
 "TeamTaskClaim", "TeamTaskComplete"]
```

These are kept regardless of the `--allowed-tools` user filter (same for `--disallowed-tools`). M6.34 TEAM4 fixed an asymmetry — pre-fix the `--allowed-tools` keep applied only to teammates, so a lead with `--allowed-tools Read` silently lost SendMessage/etc.

---

## 8. Lead lifecycle

### Startup (CLI: `repl.rs::run_repl_with_state`; GUI: `shared_session.rs::run_worker`)

1. Read `team_enabled` from project config (or `THCLAWS_TEAM_AGENT` env — teammate processes always have team enabled).
2. `team_role = team_agent_name.as_deref().unwrap_or("lead")` — process is "lead" or `<teammate-name>`.
3. `register_team_tools(&mut tool_registry, team_role)` — registers all 9 mailbox tools + (if lead) `TeamMerge`.
4. `set_is_team_lead(team_enabled && team_agent_name.is_none())` — process-global static consumed by BashTool / Write / Edit guards. Static (not env var) so spawned teammate children don't inherit.
5. **M6.34 TEAM3**: `set_lead_team_dir(&team_dir_abs)` — stores absolute team_dir for `kill_my_teammates()` to scope the EOF cleanup hammer.
6. Filter tool registry per role (§7).
7. Apply `--allowed-tools` / `--disallowed-tools` while protecting `team_essential_tools`.
8. Inject team_grounding_prompt + lead.md addendum into agent's system prompt.
9. Start lead inbox poller (background tokio task, 1s interval).
10. Spawn lead's own `output.log` (Team-tab visibility) + status="active".

### Lead inbox poller

```rust
loop {
    let unread = mailbox.read_unread("lead").unwrap_or_default();
    if !unread.is_empty() {
        let ids = unread.iter().map(|m| m.id.clone()).collect();
        // M6.34 TEAM5: send BEFORE mark-as-read so a closed channel
        // doesn't lose messages permanently.
        if poller_tx.send(ShellInput::TeamMessages(unread)).is_ok() {
            let _ = mailbox.mark_as_read("lead", &ids);
        }
    }
    tokio::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;  // 1000
}
```

The CLI variant pushes onto an `mpsc::UnboundedSender<Vec<TeamMessage>>` consumed by the readline `select!`; the GUI variant pushes `ShellInput::TeamMessages` into the worker's input channel.

### `handle_team_messages` (GUI) / `process_team_messages!` (CLI)

When messages arrive:

1. UI header (chat surface): `[teammate messages from: backend, qa]`
2. Per-message preview written to lead's `output.log` (300-char trim)
3. Detect `ProtocolMessage` per message:
   - **`IdleNotification`** — print `[backend is idle (task #3)] <summary>` + feed message to agent so it can coordinate
   - **`ShutdownApproved`** — print `[backend shutdown approved — stopped]` + feed to agent
   - **`ShutdownRejected`** — print `[backend shutdown rejected: <reason>]` + feed to agent
   - Plain text → preview + feed to agent
4. Build XML-framed prompt:
   ```
   <teammate_message from="backend" summary="finished /healthz">
   <message text>
   </teammate_message>
   ```
   Multiple messages joined by `\n\n`.
5. `agent.run_turn(prompt)` — same loop as a user message, but with the framed teammate context. `tokio::select!` against `cancel.cancelled()` (M6.17 BUG H1) so ctrl-C aborts mid-stream.

### EOF cleanup

CLI readline EOF + bottom of `run_repl_with_state` both call `kill_my_teammates()` (M6.34 TEAM3): runs `pkill -f "--team-dir <abs path>"` matching only teammates of THIS lead session. Pre-fix the broad `pkill -f team-agent` killed teammates of other thClaws sessions in other projects.

GUI worker doesn't currently call `kill_my_teammates()` at shutdown — the OS reclaims child processes when the GUI quits. `set_lead_team_dir` is still called for parity + future "Stop all teammates" UI.

---

## 9. Teammate lifecycle

### Spawn — by SpawnTeammate (§4)

The lead's `SpawnTeammate::call`:
1. `init_agent(name)` — create inbox + status="alive"
2. Look up `agent_def` from `~/.claude/agents/<name>.md` etc. (load order documented in [`subagent.md`](subagent.md))
3. Compose initial prompt: `[Agent role: <description>]\n[Instructions: <agent_def.instructions>]\n\n<spawn prompt>` (when agent_def has instructions; otherwise just the spawn prompt)
4. Write initial prompt to teammate's inbox as the first message
5. Build the spawn command (§4 — env vars, shell-escaped values, model alias resolution)
6. Worktree auto-creation if `isolation: worktree` on member or agent_def (§10)
7. Update `config.json` member `is_active = true`
8. Spawn (tmux split / new-session / background)

### Boot — `repl.rs::run_repl_with_state` with `THCLAWS_TEAM_AGENT=<name>`

The teammate is the same `thclaws` binary, just invoked with `--cli --team-agent <name> --team-dir <abs> --permission-mode auto --accept-all`. The `--team-agent` flag sets `THCLAWS_TEAM_AGENT=<name>` env var → `is_teammate_process()` returns true.

1. Same `register_team_tools` + filter (§7) — teammate keeps `TeamTaskClaim`/`Complete`, drops `AskUserQuestion`/`Plan*`.
2. Load agent_def for self → `agent.append_system("# Agent Role: <description>\n<instructions>")`.
3. Build team-members list from `config.json` for the system prompt.
4. Render `agent_team.md` template (substitutes `{agent_name}`, `{team_members_info}`, `{cwd}`, `{project_root}`, `{worktree_rules}`).
5. If `THCLAWS_IN_WORKTREE=1`, render `worktree.md` template (substitutes `{agent_name}`, `{project_root}`) and inject as `worktree_rules`.
6. `agent.append_system(&team_rules)` — merged team + worktree addendum.
7. Open output log under `agents/<name>/output.log` (truncate-create).
8. Set initial status = "idle".
9. Enter the inbox + task poll loop.

### Inbox + task poll loop

```rust
loop {
    // 1. Read unread inbox.
    let unread = mailbox.read_unread(name).unwrap_or_default();
    for msg in unread {
        if let Some(proto) = parse_protocol_message(msg.content()) {
            match proto {
                ShutdownRequest { from } => {
                    let busy = !pending_queue.is_empty()
                        || tq.list(InProgress).any(|t| t.owner == name);
                    if busy {
                        send ShutdownRejected to <from>
                    } else {
                        send ShutdownApproved to <from>
                        write_status(name, "stopped", None)
                        return Ok(());   // teammate process exits
                    }
                }
                _ => { /* other protocol — ignore for now */ }
            }
        } else {
            pending_queue.push_back(msg);   // plain text → process
        }
    }

    // 2. No inbox? Try claiming a task.
    if pending_queue.is_empty() {
        if let Ok(Some(task)) = tq.claim_next(name) {
            // Synthesize a "task assignment" message
            pending_queue.push_back(TeamMessage::new("task-queue",
                "[Task #<id> — <subject>]\n\n<description>\n\n
                 When done, use TeamTaskComplete with task_id=\"<id>\"."));
        }
    }

    // 3. Process one message.
    if let Some(msg) = pending_queue.pop_front() {
        write_status(name, "working", Some(&msg.id));
        let mut stream = agent.run_turn(<XML-framed prompt>);
        loop {
            tokio::select! {
                ev = stream.next() => match ev { ... },
                _ = tokio::signal::ctrl_c() => break,
            }
        }
        // Always-send idle notification after every turn end.
        send IdleNotification("finished current turn") to lead
        write_status(name, "idle", None);
    } else {
        // Nothing to do — heartbeat + sleep.
        write_status(name, "idle", None);
        sleep(POLL_INTERVAL_MS).await;
    }
}
```

Heartbeat throttling: status writes happen on every output (text delta or tool call result), but only at most every 30s during a working turn (otherwise every text delta would burn write IO).

The XML prompt frame:
```
<teammate_message from="<from>" summary="<summary>">
<message text>
</teammate_message>
```
Same shape both sides — lead and teammate see identical framing.

### Shutdown protocol

A teammate that should stop receives `ShutdownRequest {from: "lead"}`. The teammate checks for unfinished work:
- Any messages in `pending_queue`?
- Any tasks the teammate currently owns in InProgress?

If yes → `ShutdownRejected {from, reason: "still have unfinished tasks"}`. If no → `ShutdownApproved {from}` + `write_status("stopped")` + process exit (teammate falls out of the poll loop and `return Ok(())`).

The lead is responsible for sending the request via `SendMessage("backend", json_string)`. Today no automated path exists — the model orchestrates manually.

---

## 10. Worktree isolation

When `isolation: "worktree"` is set on a `TeamMember` (via TeamCreate) or on an `AgentDef` (via `.thclaws/agents/<name>.md` frontmatter), `SpawnTeammate` creates a dedicated git worktree:

```
.worktrees/<name>/      # checked out on branch team/<name>
```

### Auto-creation flow

```rust
let project_root = std::env::current_dir();
let wt_dir = project_root.join(format!(".worktrees/{name}"));   // M6.34 TEAM1: name validated
let branch = format!("team/{name}");

// Step 1: ensure project_root is a git repo (run `git init` if not)
// Step 2: ensure HEAD exists (create empty initial commit if unborn)
//         — pre-M6 the worktree creation would fail with "invalid reference"
//           in repos with no commits
// Step 3: git branch <branch>     (no-op if branch exists)
// Step 4: git worktree add <wt_dir> <branch>
```

Both the `git init` and the empty-commit fallback fire with stderr warnings so the user sees what happened.

If the worktree creation fails, the teammate falls back to `effective_cwd = project_root` — they run on the lead's branch. Lossy but doesn't deadlock.

### Worktree-only teammate prompt

`worktree.md` template (rendered + appended to `agent_team.md`):
```
## Git worktree
Your working directory is a git worktree on branch team/<name> ...
- Edit source/tests/code in your worktree freely — it's your branch.
- Shared docs, API specs, schemas, ... belong at the project root, never only in your worktree.
- When you produce a shared artifact at the project root, SendMessage the dependent teammates with its absolute path.
```

Combined with the `THCLAWS_PROJECT_ROOT` env var the teammate inherits, the model knows where to write shared artifacts (project root, visible to all teammates) vs branch-isolated code (its own worktree).

### `TeamMerge` (the inverse)

The lead's `TeamMerge` (§4) walks `git for-each-ref refs/heads/team/`, counts commits ahead of the target branch, runs `git merge --no-ff --no-edit <branch>` per teammate. On conflict: collects conflicted file list via `git diff --name-only --diff-filter=U`, runs `git merge --abort`, **stops on first failure** so the lead deals with one conflict at a time.

`cleanup: true` removes the worktree + deletes the merged branch (using `branch -d` — safe; if the branch is checked out elsewhere, deletion fails and the report says "branch kept").

---

## 11. tmux integration

`SpawnTeammate` checks `has_tmux()` (probes `tmux -V`) + `is_inside_tmux()` (checks `TMUX` env var):

| Condition | Spawn dispatch |
|---|---|
| Outside tmux + tmux available | `tmux new-session -d -s thclaws-team -n team <cmd>` (creates session) OR split if exists |
| Inside tmux | `tmux split-window -h -d <cmd>` in current session, then `select-layout tiled` |
| No tmux | Background process; stdout/stderr → `agents/<name>/output.log` |

When tmux is used, the user can attach to the `thclaws-team` session to watch teammates live. When background, the GUI Team tab tails the output log.

`tmux_pane_id` field on `TeamMember` is reserved for future use — today no path captures the pane id back from the spawn.

---

## 12. Lead/teammate hard-blocks (BashTool / Write / Edit)

### `lead_forbidden_command` (lead-only — `is_team_lead()` gate)

Returns `Some(reason)` for any of:
- `git reset --hard` (any args)
- `git clean -f` / `git clean -d`
- `git push --force` / `git push -f `
- `git rebase`
- `git worktree remove` / `git worktree prune`
- `git checkout -- ` / `git checkout .`
- `git restore --worktree` / `git restore .`
- `git merge --abort`
- `rm -rf` / `rm -fr` / `rm -r `

BashTool surfaces:
```
team lead is not allowed to run this command: it would <reason>.
Lead is a COORDINATOR — destructive workspace ops belong to teammates inside their own worktrees, never the lead. ...
```

The rule exists because real-world test runs had LLM leads run `git reset --hard main` to "clean up", wiping a teammate's worktree. The prompt rule alone is honor-system in `--accept-all` mode; this is the seatbelt.

### `teammate_forbidden_command` (teammate-only — `THCLAWS_TEAM_AGENT` env var gate)

Catches the cross-branch reset pattern: `git reset --hard <ref>` where `ref` is anything other than `HEAD`, `HEAD~N`, `HEAD^`, `HEAD@{N}`, `tags/...`, or a hex SHA (≥7 chars). Bare branch names (`main`, `dev`), remote refs (`origin/main`), sibling team branches (`team/backend`) all blocked.

Same rationale: real-world run had `frontend` accidentally `git reset --hard main`, wiping its own work. Same-branch recovery (HEAD~N, sha) stays allowed.

### Lead Write/Edit block + merge-conflict carve-out

`tools/write.rs` + `tools/edit.rs` reject any write when `is_team_lead()` is true, UNLESS `lead_resolving_merge_conflict(&path)` returns true. The carve-out requires BOTH:
1. `.git/MERGE_HEAD` exists (merge in progress — found via `find_git_dir(start)` walking parents, or the `gitdir:` worktree pointer)
2. The target file currently contains `<<<<<<<` AND `=======` AND `>>>>>>>` markers

The test pins both signals — file-with-markers but no MERGE_HEAD → blocked; MERGE_HEAD without markers (e.g. trying to write a fresh file mid-merge) → blocked; both → allowed.

This is the only legitimate lead-author activity. Once the merge commit is made, MERGE_HEAD disappears and the exception closes automatically.

---

## 13. M6.34 audit fixes (recap)

Six bugs shipped together (`dev-log/149-team-m6-34-audit-fixes.md`):

| Bug | Severity | Fix |
|---|---|---|
| TEAM1 | HIGH | `is_valid_agent_name` (1–64 chars, alphanumeric+_+-) applied at every tool entry point + Mailbox storage boundary. Rejects path-traversal (`..`, `/`), shell metachars, control chars. |
| TEAM2 | HIGH | `shell_escape` helper applied to bin / name / team_dir / effective_cwd / model in `SpawnTeammate`'s `agent_cmd`. Replaces inline `replace('\'', "'\\''")` for consistency. |
| TEAM3 | HIGH | `kill_my_teammates()` scoped to `--team-dir <abs path>` match (captured at startup via `set_lead_team_dir`). Replaces broad `pkill -f team-agent` that killed teammates of other thClaws sessions. |
| TEAM4 | MED | Lead's `--allowed-tools` keeps `team_essential_tools` (was teammate-only). Symmetric with disallowed_tools handling. |
| TEAM5 | MED | Lead inbox pollers (CLI + GUI) send to channel BEFORE mark-as-read. Closed channel no longer loses messages permanently. |
| TEAM6 | MED | `write_status` (exclusive), `read_status` / `TaskQueue::list` / `TaskQueue::get` (shared) wrapped in file locks. Eliminates partial-write race that surfaced as "agent missing". |

---

## 14. `team_grounding_prompt` — three states

`shared_session.rs::team_grounding_prompt(model, team_enabled)` injects different system-prompt addenda based on `(team_enabled, on_claude_sdk)`:

### `team_enabled=true` + non-`agent/*` provider — full team rules

The model gets the canonical thClaws team-tool reference, lead coordination rules, worktree-isolation framing, and a critical warning against calling Claude Code's SDK team primitives:

> Your training data contains references to an Anthropic Managed Agents SDK server-side toolset (`agent_toolset_20260401`) that ships its own `TeamCreate`, `Agent`, `AskUserQuestion`, `TodoWrite`, `ToolSearch`, `SendMessage` tools backed by `~/.claude/teams/` and `~/.claude/tasks/`. Those are a DIFFERENT SYSTEM, invisible to thClaws — if you call them (or claim to have called them in your text output), the user will see an empty Team tab and think nothing happened.

### `team_enabled=true` + `agent/*` provider — UNREACHABLE warning

The `agent/*` provider shells to the local `claude` CLI subprocess, which uses Claude Code's own toolset and **does not see thClaws's tool registry**. The model is explicitly told it cannot call thClaws's team tools from this provider, and offered the choice to:
- Switch to a non-`agent/*` provider via `/model`
- Proceed sequentially without a team
- NOT pretend a team has been created

### `team_enabled=false` + any provider — DISABLED warning

The model is told the team feature is off, the SDK's built-in team primitives are NOT a substitute (their state is invisible to thClaws), and offered:
- Tell the user to set `teamEnabled: true` in `.thclaws/settings.json`
- Proceed without a team

This three-state design is a direct counter to LLM training-data bias — most models default to "yes, I created a team!" even when no real team primitive ran. The prompts force the honest answer.

---

## 15. The embedded prompts

### `agent_team.md`

Templated; rendered with substitutions for `{agent_name}` / `{team_members_info}` / `{cwd}` / `{project_root}` / `{worktree_rules}`. Establishes:
- "All tools are auto-approved" (teammate runs `--accept-all`)
- Use SendMessage with `to: "<name>"` — text alone isn't visible
- `.thclaws/` is internal team infrastructure — interact via team tools only
- `.worktrees/<name>/` belongs to its owner — don't read other teammates' worktrees
- Task workflow: CheckInbox → TeamTaskList → claim → work → complete → SendMessage lead
- Plan-approval mode opt-in (per-task convention with the lead, NOT user)

### `lead.md`

Static content. Establishes:
- Lead is a COORDINATOR, not a worker
- DO NOT use Bash, Write, Edit to build code — delegate
- DO NOT use TeamTaskClaim — only teammates claim
- Always set `owner` on TeamTaskCreate (typo guard reinforced)
- After delegating, WAIT for inbox; don't poll
- TeamMerge for `team/<name>` branches; on conflict either resolve self (merge-conflict carve-out applies) or delegate
- Plan-approval mode (when user prompt mentions it) — lead is the approver, never escalate to user

### `worktree.md`

Appended to `agent_team.md` only when `THCLAWS_IN_WORKTREE=1`. Tells the worktree teammate:
- Edit code freely in your worktree (your branch)
- Shared docs / API specs go to project root (visible to all)
- SendMessage the dependent teammate with the absolute path when you produce a shared artifact

---

## 16. Approval / permission flow

| | Lead | Teammate |
|---|---|---|
| `permission_mode` | From config (typically `Ask`) | Forced `Auto` via `--permission-mode auto` |
| Approver | Interactive (CLI prompt or GUI modal) | `--accept-all` (every tool auto-approved) |
| `requires_approval=true` tools | Modal popups (TeamCreate, SpawnTeammate, TeamMerge, Bash, Write, Edit, etc.) | Auto-approved silently |
| BashTool destructive-command warning | stderr ⚠️ | stderr ⚠️ (no human watching) |
| Lead-block on Bash | YES (from `is_team_lead()`) | N/A (not lead) |
| Teammate-block on Bash | N/A | YES (from `is_teammate_process()`) |
| Lead-block on Write/Edit | YES (carve-out for merge conflict) | N/A |

The auto-approval for teammates is intentional — there's no human to click modals — but it means any rogue model can do anything within the BashTool/Write/Edit hard-blocks. The hard-blocks are the seatbelt.

---

## 17. Provider-aware model alias resolution

`SpawnTeammate` resolves `agent_def.model`:

- Full model id (contains `-`, e.g. `claude-sonnet-4-6`, `gpt-4o`) → pass through as `--model <id>`
- Short alias (e.g. `sonnet`, `opus`, `flash`) → resolve via `ProviderKind::resolve_alias_for_provider(alias, current_provider)` — keeps the team on the user's chosen provider
- Alias that doesn't fit current provider (e.g. `sonnet` when provider=ollama) → warn + skip; teammate falls back to its config default

Pre-fix the global `resolve_alias` would surprise-switch a teammate to native Anthropic even if the project was on OpenRouter. Now provider-locked.

---

## 18. Cancellation

Teammate's poll loop has `tokio::select!` with `tokio::signal::ctrl_c()` — a Ctrl-C in the teammate's own pane / shell stops the current `agent.run_turn` stream + breaks the inner loop, but **the outer poll loop continues**. Teammates aren't easily killable from inside themselves — they need to receive a `ShutdownRequest` from lead OR be killed by the lead's `kill_my_teammates()` (M6.34 TEAM3).

Lead's `handle_team_messages` (GUI) and `process_team_messages!` (CLI) both have cancel-aware streaming via `tokio::select! { ev = stream.next() => ..., _ = cancel.cancelled() => return }`. Cancel reaches the lead's mid-turn handling of teammate messages.

---

## 19. Test coverage

`team::tests` (19 tests, all passing):

| Category | Tests |
|---|---|
| Lead Write/Edit carve-out | `lead_resolving_merge_conflict_requires_both_signals` |
| Mailbox happy paths | `mailbox_write_and_read`, `read_unread_and_mark`, `status_write_and_read`, `all_status_lists_agents` |
| Task queue | `task_queue_create_and_claim`, `task_queue_claim_next`, `task_queue_preassigned_owner` |
| Protocol | `protocol_message_roundtrip` |
| Legacy migration | `team_config_legacy_migration` |
| **M6.34 TEAM1 name validation** | `is_valid_agent_name_accepts_normal_names`, `…_rejects_path_traversal`, `…_rejects_shell_metacharacters`, `…_rejects_edges`, `mailbox_init_agent_rejects_invalid_name`, `mailbox_write_to_mailbox_rejects_invalid_recipient` |
| **M6.34 TEAM2 shell-escape** | `shell_escape_wraps_in_single_quotes`, `…_handles_embedded_single_quote`, `…_neutralizes_injection_metacharacters` |

TEAM3 (kill scope), TEAM4 (allow-list), TEAM5 (send-before-mark), TEAM6 (locking) verified by code inspection — they're integration-level (full process / channel / disk plumbing).

Plus BashTool tests in `tools::bash::tests` covering `lead_forbidden_command`, `teammate_forbidden_command_blocks_cross_branch_reset`, `is_destructive_command`.

---

## 20. Known gaps (deferred)

From the M6.34 audit:

**Deferred MED:**
- **TEAM-M4 / M7** — Stale-teammate detector. `last_heartbeat` is written by every teammate but never consumed; teammate process crash leaves task forever in InProgress. Needs design (auto-reclaim vs `TeamReclaim` tool).
- **TEAM-M5** — Mailbox + task files grow unboundedly. Inbox is read-modify-write of the entire JSON array; performance degrades with message count.
- **TEAM-M6** — `.lock` files accumulate forever. Cleanup risks cross-process race during deletion.
- **TEAM-M8 / M9** — TeamCreate eagerly writes status="alive" before SpawnTeammate runs → ghost agents in TeamStatus; SendMessage doesn't reject "defined-but-not-spawned" agents.
- **TEAM-M10** — `IS_TEAM_LEAD` static doesn't re-read on `team_enabled` runtime toggle.
- **TEAM-M11** — `SendMessage` / `TeamTaskCreate` / `TeamTaskComplete` have `requires_approval=false`; lead can spam without modal visibility.
- **TEAM-M12** — `agent_def.instructions` injected twice (system prompt + initial inbox message body). Cosmetic.
- **TEAM-M13** — `fs2::lock_exclusive` doesn't give in-process mutual exclusion on Linux (flock per-OFD). Cross-process exclusion works.
- **TEAM-M14** — No automatic worktree cleanup at session end. Orphan worktrees + branches accumulate without manual `TeamMerge --cleanup`.
- **TEAM-M15** — Lead's `Mailbox` uses relative path `.thclaws/team`; lead's `cd` via BashTool drifts the resolved path.

**LOW (TEAM-L1 through TEAM-L15)**: claim() busy check race (only with same-agent-id-in-multiple-processes), POLL_INTERVAL_MS=1000 IO load, `read_unread` reads-then-filters, `to_string_pretty` verbosity, TeamMerge no clean-tree preflight, `claim_next` swallows all errors as race, protocol-message false positives, hardcoded lead name, teammate ctrl_c could affect lead pane (depends on tmux process-group), `init_agent` "alive" string semantics, unauthenticated `from` field (single-user threat model), idle-notification spam, sandbox doesn't enforce `agent_team.md`'s "don't touch `.thclaws/`" rule for teammates, teammate has SpawnTeammate registered (probably should be lead-only).

---

## 21. What lives where (source-line index)

| Concern | File | Symbol |
|---|---|---|
| Module entry + data structs | `team.rs` | `TeamConfig`, `TeamMember`, `TeamMessage`, `TeamTask`, `AgentStatus`, `ProtocolMessage` |
| Lead/teammate static flag | `team.rs` | `IS_TEAM_LEAD` static, `set_is_team_lead`, `is_team_lead` |
| Merge-conflict carve-out gate | `team.rs` | `lead_resolving_merge_conflict`, `find_git_dir` |
| M6.34 TEAM1 name validator | `team.rs` | `is_valid_agent_name` |
| M6.34 TEAM2 shell escape | `team.rs` | `shell_escape` |
| M6.34 TEAM3 kill scope | `team.rs` | `LEAD_TEAM_DIR` static, `set_lead_team_dir`, `kill_my_teammates` |
| File locking | `team.rs` | `with_file_lock`, `with_file_lock_shared` |
| Mailbox + Tools | `team.rs` | `Mailbox`, `init_agent`, `write_to_mailbox`, `mark_as_read`, `write_status`, `read_status`, `all_status`, `output_log_path` |
| Task queue | `team.rs` | `TaskQueue`, `next_id`, `create`, `get`, `claim`, `complete`, `release`, `list`, `claim_next` |
| Tool registration | `team.rs` | `register_team_tools` (one site per tool struct) |
| tmux helpers | `team.rs` | `has_tmux`, `is_inside_tmux` |
| Protocol message helpers | `team.rs` | `parse_protocol_message`, `make_idle_notification` |
| Lead startup (CLI) | `repl.rs::run_repl_with_state` | `team_enabled` resolution, `register_team_tools`, `set_is_team_lead`, `set_lead_team_dir` |
| Lead startup (GUI) | `shared_session.rs::run_worker` | Same primitives + `team_grounding_prompt` |
| CLI teammate event loop | `repl.rs` | `if let Some(ref agent_name) = team_agent_name` block |
| CLI lead inbox poller | `repl.rs` | `if team_enabled` tokio::spawn → `inbox_tx` |
| GUI lead inbox poller | `shared_session.rs::run_worker` | `if team_enabled` tokio::spawn → `ShellInput::TeamMessages` |
| GUI teammate-message handler | `shared_session.rs` | `handle_team_messages` |
| CLI teammate-message handler | `repl.rs` | `process_team_messages!` macro |
| EOF cleanup | `repl.rs` | `kill_my_teammates()` (two sites) |
| GUI Team-tab IPCs | `gui.rs` | `team_send_message`, `team_list`, `team_enabled_get`, `team_enabled_set` |
| BashTool guards | `tools/bash.rs` | `lead_forbidden_command`, `teammate_forbidden_command`, `ref_resets_to_different_branch`, `is_destructive_command`, `is_teammate_process` |
| Write/Edit lead-block | `tools/write.rs`, `tools/edit.rs` | the `is_team_lead() && !lead_resolving_merge_conflict(...)` early-return |
| System-prompt addenda | `default_prompts/agent_team.md`, `lead.md`, `worktree.md` | (whole files) |
| Provider-aware alias | `team.rs::SpawnTeammate::call` | the `if model.contains('-')` branch |
