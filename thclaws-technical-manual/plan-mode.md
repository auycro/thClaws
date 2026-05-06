# Plan mode

Structured, sequentially-gated, audit-reviewable execution. The model enters via `EnterPlanMode` (or the user via `/plan` / sidebar), explores read-only, calls `SubmitPlan` with an ordered step list, the user clicks **Approve** in the right-side sidebar, and the engine drives execution one step at a time with a bounded per-step retry budget. Failures route through Retry / Skip / Abort sidebar buttons rather than the model's own discretion. The cross-step `output` channel surfaces generated ids / hashes / paths / port numbers from completed steps into the next step's prompt — independent of the chat history (which is compacted at every step boundary). Persists alongside the session JSONL as `plan_snapshot` events; survives `/load` and reconnects to its sidebar.

Distinct from **TodoWrite** (casual scratchpad, no enforcement, invisible until the user opens `.thclaws/todos.md`) and from **subagent** / **agent team** (different process / recursion models). Plan mode is the **highest-ceremony** planning tool: sequential gate, audit-required note on Failed transitions, sidebar visibility, four user override buttons.

This doc covers: the four-mode permission model + the pre-plan-mode stash, the four plan tools (`EnterPlanMode` / `ExitPlanMode` / `SubmitPlan` / `UpdatePlanStep`), the Layer-1 transition gate (legal / illegal table + the M6.7 "blocked by upstream" path), the Layer-2 system reminder (full prompt vs narrowed-execution view), the dispatch-time block layers (TodoWrite block + generic mutating-tool block + approval-window gate), the M6.1 Ralph driver loop (per-step retry budget + force-Failed + step-boundary compaction), the M4.4 stalled-turn detector (and the M6.31 PM2 rising-edge fix), the M6.3 cross-step output channel + UTF-8 safe truncation, the sidebar IPC handlers (`plan_approve` / `plan_cancel` / `plan_retry_step` / `plan_skip_step` / `plan_stalled_continue`), the CLI `/plan` slash, JSONL persistence + session-swap hygiene (M6.31 PM1), plan-completion auto-restore, mutex-poison recovery (M6.31 PM3 + PM4), and the testing surface.

**Source modules:**
- `crates/core/src/tools/plan.rs` — `EnterPlanModeTool`, `ExitPlanModeTool`, `SubmitPlanTool`, `UpdatePlanStepTool` (input schemas, gate-error → tool_result echo)
- `crates/core/src/tools/plan_state.rs` — `Plan`, `PlanStep`, `StepStatus`, the `slot()` mutex, `submit` / `update_step` / `force_step_done` / `set_step_output` / `clear` / `restore_from_session`, `stall_counter` + `note_turn_completed_without_progress` + `reset_stall_counter`, `step_attempts` + `note_step_attempt` + `reset_step_attempts_external`, `STALL_TURN_THRESHOLD` + `MAX_RETRIES_PER_STEP` + `MAX_STEP_OUTPUT_LEN`, the broadcaster (`set_broadcaster` + `fire`), UTF-8 safe `truncate_at_char_boundary`
- `crates/core/src/agent.rs` — `build_plan_reminder` (the four-state matrix), `build_execution_reminder` (Layer-2 narrowed view), `build_step_continuation_prompt` (per-attempt prompt), `collect_prior_step_outputs`, dispatch-time block layers at `run_turn` (TodoWrite block at line 1133, generic block at line 1163, approval-window gate at line 1204)
- `crates/core/src/permissions.rs` — `PermissionMode::Plan`, `current_mode` / `set_current_mode_and_broadcast`, `stash_pre_plan_mode` / `take_pre_plan_mode`, the mode broadcaster
- `crates/core/src/shared_session.rs::run_worker` — plan_persist_path Arc, plan-state broadcaster registration, `restore_from_session` after `/load`, session-swap hygiene at NewSession / LoadSession / SessionDeletedExternal / SessionRenamedExternal / ChangeCwd (M6.31 PM1)
- `crates/core/src/shared_session.rs::drive_turn_stream` — the Ralph loop (M6.1 retry budget + M6.2 step-boundary compaction + M4.4 stall detector + M6.7 upstream-failed yield)
- `crates/core/src/gui.rs` — sidebar IPC handlers (`plan_approve`, `plan_cancel`, `plan_retry_step`, `plan_skip_step`, `plan_stalled_continue`) + `ViewEvent::PlanUpdate` / `PlanStalled` translation
- `crates/core/src/repl.rs` — `SlashCommand::Plan` (`/plan [enter|exit|status]`), CLI rendering of plan state
- `crates/core/src/session.rs` — `PlanSnapshotEvent` JSONL serialization, `Session::append_plan_snapshot`
- `crates/core/src/compaction.rs` — `compact_for_step_boundary` / `clear_for_step_boundary` (M6.2 / M6.4 step-boundary history strategies)

**Cross-references:**
- [`permissions.md`](permissions.md) — `PermissionMode::Plan` is the third value alongside Auto/Ask; the dispatch gate's plan-mode layers fire from `agent.rs::run_turn`
- [`agentic-loop.md`](agentic-loop.md) — `drive_turn_stream` is the post-turn driver that pushes per-step continuation prompts
- [`sessions.md`](sessions.md) — `plan_snapshot` events are appended to the active session's JSONL on every plan mutation; `/load` replays them via `restore_from_session`
- [`context-composer.md`](context-composer.md) — `build_plan_reminder` / `build_execution_reminder` / `build_step_continuation_prompt` are appended to the system prompt + the per-step user message
- [`todo.md`](todo.md) — TodoWrite is the lower-ceremony alternative; the M6.20 BUG M1 plan-mode block prevents the model from using TodoWrite as a draft for SubmitPlan
- [`built-in-tools.md`](built-in-tools.md) — concise tool surface for the four plan tools

---

## 1. Concept

Plan mode is the **highest-ceremony planning tool** in thClaws's hierarchy:

```
ceremony     │  user visibility   │  enforcement                │  use when
─────────────┼────────────────────┼─────────────────────────────┼─────────────────────
TodoWrite    │  invisible (file   │  none                       │  informal multi-step
             │  only)             │                             │  work; 2-4 subtasks
─────────────┼────────────────────┼─────────────────────────────┼─────────────────────
SubmitPlan + │  sidebar with      │  sequential gating + audit  │  user wants to
UpdatePlanStep│ checkmarks +      │  + per-step retry budget +  │  review the plan
             │  Approve / Cancel /│  approval-window gate +     │  before executing,
             │  Retry / Skip /    │  audit-required note on     │  with live progress
             │  Abort buttons     │  Todo→Failed transitions    │  visibility
─────────────┼────────────────────┼─────────────────────────────┼─────────────────────
TaskCreate / │  CLI `/tasks` only │  none                       │  ephemeral
Update / Get/│  (in-memory)       │                             │  in-process tracking
List         │                    │                             │
```

Plan mode is the right primitive when:
- The work has 3+ steps with order dependencies
- Each step has a runnable verification (build exits 0, file exists, regex match, HTTP probe, test runner)
- The user wants approval gating before mutations begin
- The model needs to be force-iterated to completion (Ralph loop) rather than nudged generically

The system prompt's `build_plan_reminder` (see §6) explicitly tells the model what makes a good plan: one action per step, one shell-runnable verification per step, no throwaway / overwrite pairs, no human-eye checks, no long-running servers as verifications. The plan tool descriptions themselves carry the framing too — the model's first read of `EnterPlanMode` already establishes expectations.

### Why a separate gate from TodoWrite?

TodoWrite is the model's casual scratchpad. Plan mode is the **user's contract** — the user sees the plan in the sidebar, clicks Approve, and the engine commits to executing it under the gate. TodoWrite's empty schema (`{todos: [...]}`) doesn't carry status semantics; SubmitPlan's structured shape (`{steps: [{id, title, description}]}`) is the contract. The M6.20 BUG M1 plan-mode block (§9.1) explicitly prevents the model from using TodoWrite as a draft for SubmitPlan — they're different primitives.

---

## 2. The four plan tools

### `EnterPlanMode`

```json
{"type": "object", "properties": {}}
```

Empty schema — entering plan mode is an intent, not a configuration. Stashes the current `PermissionMode` via `stash_pre_plan_mode(prior)` (idempotent — re-entering plan mode while already in it leaves the stash alone), flips the global mode to `Plan` via `set_current_mode_and_broadcast`. Returns a tool-result text telling the model to use Read / Grep / Glob / Ls + call SubmitPlan when ready.

The mode flip is process-global (`Mutex<PermissionMode>` in permissions.rs) — every subsequent dispatch checks `current_mode()` so the block fires on the very next tool call, not the next user message.

### `ExitPlanMode`

```json
{"type": "object", "properties": {}}
```

Pops the stashed pre-plan mode via `take_pre_plan_mode()`, defaults to `Ask` if nothing was stashed (defensive — entering plan mode via `/plan` or sidebar without a stash should still leave a sane state). Used by the model directly to abort plan mode without submitting (rare), or under the hood by the **sidebar Approve flow** (which uses Auto and the auto-restore on plan-completion to flip back).

### `SubmitPlan`

```json
{
  "type": "object",
  "properties": {
    "steps": {
      "type": "array", "minItems": 1,
      "items": {
        "type": "object",
        "properties": {
          "id":          { "type": "string", "description": "Stable, unique step id (e.g. \"step-1\")" },
          "title":       { "type": "string", "description": "Short imperative title (≤80 chars)" },
          "description": { "type": "string", "description": "Optional longer detail" }
        },
        "required": ["id", "title"]
      }
    }
  },
  "required": ["steps"]
}
```

Steps don't carry a `status` because every step starts as `Todo` — letting the model spell that out invites "the model said InProgress at submit time" race conditions. The `id` is opaque and stable across `UpdatePlanStep` calls (`step-1`, `s0`, `parse-config` — model picks).

Re-calling SubmitPlan **replaces the prior plan wholesale** (channel for "I changed my mind"). The replacement also resets the per-step attempt counter + the stall counter (`reset_step_attempts` + `reset_stall_counter` are called inside `submit`).

Validation inside `plan_state::submit`:
- `steps.is_empty()` → `Err("plan must have at least one step")`
- duplicate step `id` → `Err("duplicate step id: <id>")`
- empty step `id` (after trim) → `Err("step id cannot be empty")`
- empty step `title` (after trim) → `Err("step <id> has empty title")`

Plan id format: `"plan-<uuid v4>"`. The id changes on every SubmitPlan — useful for the sidebar to distinguish "model replaced the plan" (new id) from "model amended a step" (same id, different step states).

### `UpdatePlanStep`

```json
{
  "type": "object",
  "properties": {
    "step_id": { "type": "string" },
    "status":  { "type": "string", "enum": ["todo", "in_progress", "done", "failed"] },
    "note":    { "type": "string", "description": "Optional context — failure reason or progress detail" },
    "output":  { "type": "string", "description": "Optional ≤1KB cross-step value (M6.3) — only on `done` transitions" }
  },
  "required": ["step_id", "status"]
}
```

Routes through `plan_state::update_step` (the gate, §3) and then optionally `set_step_output` if `status == done` and `output` is present. Returns a confirmation string with a "Next step:" hint when the transition was Done and another step exists.

`output` is only persisted on `done` transitions — outputs on other transitions are noise (intermediate states don't produce stable data for later steps). Capped at `MAX_STEP_OUTPUT_LEN = 1024` bytes via `truncate_at_char_boundary` (UTF-8 safe — the M6.9 fix that prevented panics on Thai-language outputs crossing the cap mid-character).

---

## 3. The Layer-1 gate

`plan_state::update_step` enforces:

| From | To | Allowed? | Notes |
|---|---|---|---|
| `Todo` | `InProgress` | ✓ | Only when previous step is `Done` (or this is step 0). Otherwise: error message naming the previous step's title and current status. |
| `Todo` | `Failed` | ✓ | "Blocked by upstream failure" path (M6.7). **Note REQUIRED** — empty / whitespace note rejected with "marking step N (\"...\") Todo → Failed requires a note explaining why". |
| `InProgress` | `Done` | ✓ | Standard happy-path completion. |
| `InProgress` | `Failed` | ✓ | Note recommended (not enforced — the model often *should* report a one-line reason but the gate doesn't refuse if it doesn't). |
| `Failed` | `InProgress` | ✓ | Retry. |
| `Done` | `*` | ✗ | Done is **terminal** — re-opening would let the model rewrite history. To change approach, submit a new plan. |
| `*` | `Todo` | ✗ | No "un-start" path. |
| Any other | | ✗ | "illegal step transition From → To on step <id>" error. |

The error strings are surfaced as `tool_result` content with `is_error=true`. The model reads them on its next turn and self-corrects (same honor system as Anthropic's tool errors). Combined with the Layer-2 narrowed-view system reminder (§6.2), the gate produces a predictable "one step at a time" execution loop.

### Sequential gating in detail

For `Todo → InProgress` on step `N` (N > 0):

```rust
if plan.steps[N - 1].status != StepStatus::Done {
    return Err(format!(
        "cannot start step {N} (\"{}\") — step {} (\"{}\") is currently {:?}, not Done. \
         Finish or fail the previous step first.",
        plan.steps[N].title,
        N - 1,
        plan.steps[N - 1].title,
        plan.steps[N - 1].status,
    ));
}
```

Error names the prior step + its current status, so the model knows whether it should finish step N-1 (if InProgress / Failed) or attempt it first (if Todo).

### Why Done is terminal

The gate's invariant is "every Done step was actually completed." Re-opening Done would break it (and break the model's mental model — what does "InProgress on a Done step" mean?). The model's "I changed my mind" channel is `SubmitPlan` with a fresh plan; the user's "this Failed step is actually fine" channel is `force_step_done` via the sidebar Skip button (§9.4 — bypasses the gate, records `note: "skipped by user"` for audit).

---

## 4. `Plan` + `PlanStep` data model

```rust
pub struct Plan {
    pub id: String,                // "plan-<uuid v4>"
    pub steps: Vec<PlanStep>,
}

pub struct PlanStep {
    pub id: String,
    pub title: String,
    pub description: String,
    pub status: StepStatus,        // Todo | InProgress | Done | Failed
    pub note: Option<String>,      // Set by UpdatePlanStep; required for Todo→Failed
    pub output: Option<String>,    // M6.3 cross-step data channel; ≤1024 bytes
}
```

Lives in a process-global `Mutex<Option<Plan>>` (the `slot()`). One active plan at a time — there's exactly one active session per worker (Terminal + Chat tabs share `SharedSession`); a `/load` swap restores the loaded session's plan into this slot via `restore_from_session`.

`Plan::current_step_idx()` returns the index of the `InProgress` step (at most one — the gate enforces this). `Plan::step_by_id(id)` is the canonical lookup; `position(id)` returns the index for gating math.

Status ordering: `Todo` → `InProgress` → `Done` (happy path) or `Failed` (recovery). `Failed` is terminal-but-retryable (Failed → InProgress is legal; Failed → Done isn't).

---

## 5. Cross-step output channel (M6.3)

Each completed step can carry an `output` value (one line, ≤ 1024 bytes) that surfaces in later steps' prompts. Use cases:
- A step generates a UUID / hash / API key the next step needs
- A step writes a config file path the next step reads
- A step probes a port number / DB id the next step connects to

The model sets `output` on the `done` transition:
```json
{"step_id": "s2", "status": "done", "output": "user-id: abc-123"}
```

`SubmitPlanTool::call` invokes `update_step` first (status transition), then `set_step_output` (M6.3 stash). Outputs are persisted on the step + surfaced in two places:

- **`build_step_continuation_prompt`** (per-attempt user message) — appends `\n\nOutputs from prior steps:\n  - Step 1 (Generate user): user-id: abc-123\n  - …`
- **`build_execution_reminder`** (system reminder, focus-step block) — same lines under "Outputs from prior steps (use these instead of guessing):"

Both channels are needed because chat history is **compacted at every step boundary** (M6.2 — see §10) — a `Read` tool result from step 1 isn't reliably present in step 5's context. The `output` field is the explicit cross-step data channel; the model is told "don't rely on prior tool outputs surviving in context."

### UTF-8 safe truncation

`MAX_STEP_OUTPUT_LEN = 1024` bytes. Naive `s[..1024]` panics when the cut point falls inside a multi-byte char (Thai = 3 bytes/char, emoji = 4 bytes/char). M6.9 fix: `truncate_at_char_boundary` walks back to the largest `is_char_boundary(cut)` ≤ `max_bytes`, then appends `" … [truncated]"`. The `set_step_output_handles_multibyte_utf8_at_truncation_boundary` test pins this with a deliberately constructed Thai string whose cut byte falls inside a character.

---

## 6. System reminders

Two layered reminders, both built each turn by the agent's `compose_system_prompt` path and appended to the full system prompt.

### 6.1 `build_plan_reminder` (high-level, four states)

The `(mode, plan)` matrix:

| `mode` | `plan` | Reminder |
|---|---|---|
| `Plan` | `None` | Long "what makes a good plan" framing (~140 lines). Three-rule decomposition test (one action / one shell-runnable verification / output preserved across next step), bash-runnable verifications only, no long-running servers, default to canonical scaffolders, name cross-step artifacts in descriptions, ACTUALLY RUN the verification, audit-BEFORE-SubmitPlan checklist (7 items). |
| `Plan` | `Some(plan)` (not all-done) | "Plan submitted, awaiting approval" — tells the model to STOP calling tools (Read, Grep, Edit, Bash, UpdatePlanStep, ExitPlanMode are all blocked while waiting), emit a brief one-line confirmation, and let the sidebar buttons drive the workflow. Explicitly bans the "tell the user to type 'go' to start" pattern. |
| `Plan` | `Some(plan)` (all-done) | M6.9 BUG C1 fix: recurse with effectively-no-plan to get the (Plan, None) reminder. Pre-fix the model saw "awaiting approval" of a finished plan when the user re-entered plan mode for a NEW task without dismissing the old completed sidebar. |
| `Auto` / `Ask` | `Some(plan)` | `build_execution_reminder` (Layer-2, see below). |
| `Auto` / `Ask` | `None` | No reminder. |

### 6.2 `build_execution_reminder` (Layer-2, narrowed view)

When a plan is being executed (mode is no longer Plan, plan slot has an unfinished plan), the reminder narrows the model's view to the focused step:

```
## Executing approved plan — step 3 of 5

  Current step: "Add /healthz endpoint to web server"

  Edit src/routes.rs: add GET /healthz handler returning {ok: true}.
  Verify: `curl localhost:8080/healthz | jq .ok` returns true.

  (note: previous attempt failed because port 8080 was busy)

Already complete: 1. Scaffold project · 2. Install deps

Remaining (titles only, do NOT preview):
  • 4. Wire up middleware
  • 5. Smoke-test deployment

Outputs from prior steps (use these instead of guessing):
  - Step 1 (Scaffold project): app-id: web-svc-2024
  - Step 2 (Install deps): node_modules/ ready
```

Focus step picked by:
1. The `InProgress` step (model already started)
2. Else the `Failed` step (recovery state — sidebar shows Retry/Skip/Abort)
3. Else the first `Todo` step (about to start)
4. Else `None` — all steps Done

Future steps show **titles only, no descriptions** — prevents the model from "let me also do step 5 while I'm here" coordination across step boundaries. Done steps collapse into a single tally line.

### 6.3 `build_step_continuation_prompt` (per-attempt user message)

Pushed by the driver as a `ShellInput::Line` after each turn end (when the plan still has an actionable step). Two variants based on the per-step `attempt` counter:

- `attempt == 1` — terse "focus is step N/M, do action / run verification / call UpdatePlanStep(done|failed)"
- `attempt > 1` — escalation: "attempt N on step M, you have at most MAX_RETRIES_PER_STEP attempts; commit to a transition this turn"

Both variants append the prior-outputs block (§5) so the model sees cross-step data with every nudge.

---

## 7. Permission mode + pre-plan stash

`PermissionMode` has three values: `Auto` (never prompt), `Ask` (prompt on `requires_approval` tools), `Plan` (block mutating tools entirely). Plan mode is one slice of the permissions subsystem — see [`permissions.md`](permissions.md) for the full layered gate.

### Pre-plan stash

`Mutex<Option<PermissionMode>>` in permissions.rs. Holds whichever mode was active before `EnterPlanMode` / `/plan` / sidebar-`/plan` flipped us into Plan. `ExitPlanMode` / sidebar Cancel / plan-completion auto-restore (§11) pops it via `take_pre_plan_mode()` so `Ask → Plan → Ask` instead of `Ask → Plan → Auto`.

Idempotent — the `EnterPlanModeTool` guards re-entry:
```rust
let prior = current_mode();
if !matches!(prior, PermissionMode::Plan) {
    stash_pre_plan_mode(prior);
}
```
Single slot — re-entering Plan from Plan is a no-op for the stash.

### Mode flip points

| Flipper | Where | Mode change |
|---|---|---|
| `EnterPlanMode` tool | `tools/plan.rs` | `* → Plan`; stashes prior |
| `ExitPlanMode` tool | `tools/plan.rs` | `Plan → take_pre_plan_mode().unwrap_or(Ask)` |
| `/plan enter` CLI slash | `repl.rs:4781` | Same as `EnterPlanMode` |
| `/plan exit` CLI slash | `repl.rs:4794` | `take_pre_plan_mode()` + `clear()` plan |
| Sidebar Approve | `gui.rs:1670` | `Plan → Auto` (executes unattended); pre-plan stays stashed |
| Sidebar Cancel | `gui.rs:1689` | `Plan → take_pre_plan_mode().unwrap_or(Ask)` + `clear()` plan |
| Plan-completion auto-restore | `plan_state::update_step` + `force_step_done` | `Auto → take_pre_plan_mode().unwrap_or(prior)` when final step transitions to Done |

The auto-restore (§11) is the reason sidebar Approve flips to Auto rather than Ask: the user has already reviewed the plan, individual approval popups would be friction, and the pre-plan stash ensures we don't *stay* in Auto past the plan's lifetime.

---

## 8. Dispatch-time block layers

Three independent blocks fire at `agent.rs::run_turn` per `tool_use` block, before the approval gate:

### 8.1 TodoWrite plan-mode block (M6.20 BUG M1)

```rust
if matches!(permission_mode, PermissionMode::Plan) && name == "TodoWrite" {
    return tool_result_error("Blocked: TodoWrite is the casual scratchpad outside plan mode. \
                              In plan mode, call SubmitPlan to publish your plan to the \
                              sidebar — UpdatePlanStep tracks progress per step.");
}
```

Fires **before** the generic mutating-tool block (8.2). Pre-fix the generic block ran first (because `TodoWrite::requires_approval == true`), so the model always saw the generic "Use Read/Grep/Glob/Ls" message instead of this specific "Use SubmitPlan" one. The model would then write a TodoWrite draft AND SubmitPlan the same content — confused about which is the structured contract.

### 8.2 Generic mutating-tool block (M2)

```rust
if matches!(permission_mode, PermissionMode::Plan) && tool.requires_approval(input) {
    return tool_result_error("Blocked: <name> is not available in plan mode. \
                              Use Read / Grep / Glob / Ls to explore the codebase. ...");
}
```

Catches everything mutating: Write, Edit, Bash, document editors, etc. Plan tools themselves (`SubmitPlan`, `UpdatePlanStep`, `EnterPlanMode`, `ExitPlanMode`) have `requires_approval=false` and sail through. Read-only tools (Read, Grep, Glob, Ls) have `requires_approval=false` and also work — they're the exploration set.

### 8.3 Approval-window gate

```rust
if matches!(permission_mode, PermissionMode::Plan)
    && (name == "UpdatePlanStep" || name == "ExitPlanMode")
    && plan_state::get().is_some()
{
    return tool_result_error("Blocked: <name> is not available while waiting for the user \
                              to approve the plan. ...");
}
```

While `mode == Plan && plan.is_some()`, the model can't progress steps (`UpdatePlanStep`) or unilaterally exit plan mode (`ExitPlanMode`). The sole legal exit from "plan submitted, awaiting approval" is the user clicking sidebar Approve / Cancel (which fire `plan_approve` / `plan_cancel` IPCs). Re-submitting via `SubmitPlan` stays allowed — the new plan also waits for approval.

Without this gate, the model could interpret a casual "Start" as approval, call `ExitPlanMode` itself, flip mode to Auto, and start writing files before the user has reviewed the plan.

---

## 9. The Ralph driver (`drive_turn_stream`)

After each agent turn ends naturally (not via cancellation), the worker checks `plan_state::get()` and decides whether to push another turn. Three subsystems run in this post-turn block:

### 9.1 Stalled-turn detector (M4.4 + M6.31 PM2)

```rust
if let Some(plan) = plan_state::get() {
    let in_progress = plan.steps.iter().find(|s| s.status == StepStatus::InProgress);
    if let Some(step) = in_progress {
        let turns = plan_state::note_turn_completed_without_progress();
        if turns == STALL_TURN_THRESHOLD {  // M6.31 PM2: == not >=
            events_tx.send(ViewEvent::PlanStalled { step_id: step.id, ... });
        }
    }
}
```

`STALL_TURN_THRESHOLD = 3`. Counts consecutive turns that ended without a plan mutation (`submit` / `update_step` / `force_step_done` reset the counter to 0). On the third unproductive turn, fires `ViewEvent::PlanStalled` so the sidebar shows a "model seems stuck" banner with a Continue button.

**M6.31 PM2** fix: changed `>=` to `==` so the detector fires on the rising edge only. Pre-fix every turn after threshold re-fired (turn 3 → fire, turn 4 → fire again, turn 5 → fire again) — the sidebar banner blinked on every turn until the user clicked Continue.

### 9.2 Per-step retry budget (M6.1)

```rust
if !waiting_for_approval {
    if upstream_failed {
        return;  // M6.7: yield to the user; sidebar already shows Retry/Skip/Abort
    }
    let next = plan.steps.iter().find(|s| matches!(s.status, Todo | InProgress));
    if let Some(step) = next {
        let attempt = plan_state::note_step_attempt(&step.id);
        if attempt > MAX_RETRIES_PER_STEP {  // 3
            // Force-mark Failed; sidebar Retry resets counter and lets model try again.
            plan_state::update_step(&step.id, Failed, Some("max retries per step exceeded ..."));
        } else {
            let prompt = build_step_continuation_prompt(&plan, step, attempt);
            input_tx.send(ShellInput::Line(prompt));
        }
    }
}
```

`MAX_RETRIES_PER_STEP = 3`. `note_step_attempt(step_id)` returns 1 on the first nudge for that step id, 2 on the second, etc. Resets when the driver moves on to a different step (the counter holds `(step_id, count)` — different id → reset to 1).

After 3 attempts on the same step without it transitioning to Done or Failed, the driver force-marks it Failed with note "max retries per step exceeded ...". Sidebar Retry button calls `reset_step_attempts_external()` and re-pushes a turn so the model gets a fresh budget on the same step.

**M6.7 yield** at the top of the budget block: when the earliest non-Done step is `Failed`, return immediately. Without this, the driver would push per-step prompts on a Todo downstream step that the gate refuses to start (because the prior Failed step blocks it), burning the budget on a step that can't possibly start. The user owns recovery via Retry / Skip / Abort.

### 9.3 Step-boundary compaction (M6.2 / M6.4)

```rust
if attempt == 1 && any_done {
    // Crossing a step boundary (different step id from last time)
    // AND there's actual completed work in history worth compacting.
    let mut history = state.agent.history_snapshot();
    let (changed, notice) = match config.plan_context_strategy.as_str() {
        "clear"   => clear_for_step_boundary(&mut history),       // wipes history except first user msg
        _         => compact_for_step_boundary(&mut history),    // structural shrink (default)
    };
    if changed {
        state.agent.set_history(history.clone());
        // Persist as a CompactionEvent in the session JSONL so /load restores trimmed history
        session.append_compaction_to(&path, &history);
    }
}
```

Fires only when:
1. `attempt == 1` (we just crossed a step boundary — different step id from last time)
2. At least one step is `Done` (there's actual completed work in history worth compacting)

Default strategy `"compact"` (M6.2) — structural shrink that preserves plan-tool tool_results (`SubmitPlan` / `UpdatePlanStep` calls — the breadcrumbs the model uses to know what's done) and replaces non-plan tool_results with short placeholders.

Alternative strategy `"clear"` (M6.4) — `clear_for_step_boundary` wipes history outright keeping only the first user message for project-level grounding. Set via `config.plan_context_strategy` in `.thclaws/settings.json`.

The compaction marker is persisted as a `CompactionEvent` in the session JSONL (same path `maybe_auto_compact` uses) so `/load` after the fact restores the trimmed history rather than the full pre-compaction history.

---

## 10. Sidebar IPC handlers

GUI-only; CLI uses `/plan` slash (§12). Five buttons + their IPC handlers:

| Button | IPC | Handler logic |
|---|---|---|
| **Approve** | `plan_approve` | M6.9 BUG C2 guard: only act if `plan.is_some() && any non-Done step`. Flip mode to `Auto`. Push `ShellInput::Line("Begin executing the plan.")`. pre_plan_mode stays stashed — restored by §11 when the final step transitions to Done. |
| **Cancel** | `plan_cancel` | `take_pre_plan_mode().unwrap_or(Ask)` → `set_current_mode_and_broadcast`. `plan_state::clear()` (drops the slot, fires `ViewEvent::PlanUpdate(None)` — sidebar dismisses). |
| **Retry** (on a Failed step) | `plan_retry_step` | M6.7 status guard: only if step is currently `Failed`. `update_step(step_id, InProgress, None)` → `reset_step_attempts_external()` (fresh budget) → push "Retry the failed step (...)." |
| **Skip** (on a Failed step) | `plan_skip_step` | `force_step_done(step_id, "skipped by user")` — bypasses the gate (Failed → Done isn't legal via `update_step`); audit note records the override. Push "Step (...) was skipped by the user. Continue with the next step." |
| **Continue** (on Stalled banner) | `plan_stalled_continue` | `reset_stall_counter_external()` + `reset_step_attempts_external()`. Push "Continue with the plan. If you're stuck, commit to a UpdatePlanStep transition ..." |

All four "push a turn" handlers send `ShellInput::Line` into `shared_for_ipc.input_tx` — same channel the user types into. The auto-nudge wakes the worker loop which kicks off another turn.

### `force_step_done` vs `update_step`

`force_step_done` is the **user override** path. Sidebar Skip uses it; `update_step` would reject Failed → Done as "illegal step transition". The gate's invariant ("every Done step was actually completed") is intentionally broken here — the audit `note: "skipped by user"` is the record.

---

## 11. Plan-completion auto-restore

When the final step transitions to Done (via `update_step` or `force_step_done`), `plan_state` checks `all_done = steps.iter().all(|s| s.status == Done)` and pops `take_pre_plan_mode()` if any. The mode is restored via `set_current_mode_and_broadcast`.

This is why sidebar Approve can flip to Auto without the user staying in Auto for unrelated work afterward: the auto-restore unwinds it as soon as the plan finishes. Without it, "I approved one plan an hour ago" would silently disable Ask mode for everything else.

The plan slot is **not cleared** on auto-restore — the sidebar's "All complete" footer relies on the all-done plan staying visible. The (Plan, Some(p)) reminder branch (§6.1) detects all-done separately and routes to the (Plan, None) recursion.

---

## 12. CLI `/plan` slash

```
/plan              # alias for /plan enter
/plan enter        # stash + flip to Plan
/plan on
/plan start

/plan exit         # take_pre_plan_mode + clear()
/plan off
/plan cancel
/plan stop
/plan abort

/plan status       # show current mode + active plan id + step count
/plan show
```

CLI plan mode does NOT have the per-step Ralph driver (§9.2) — `drive_turn_stream` is GUI-only. CLI users get plan-mode semantics (the gate, the system reminders, the dispatch-time blocks) but step transitions require the model to commit each turn without the driver pushing per-step continuation prompts. This is documented as a known gap (PM5 in the M6.31 deferred list) — building a CLI driver would be a parallel implementation in `repl.rs`.

---

## 13. JSONL persistence + session-swap hygiene

Plan state is persisted alongside the session as `plan_snapshot` events in the session JSONL. The plan-state broadcaster registered at worker boot fires on every mutation:

```rust
// shared_session.rs, in run_worker:
crate::tools::plan_state::set_broadcaster(move |plan_opt| {
    let _ = plan_tx.send(ViewEvent::PlanUpdate(plan_opt.clone()));         // sidebar update
    if let Ok(g) = path_arc.lock() {
        if let Some(p) = g.as_ref() {
            let _ = crate::session::append_plan_snapshot(p, plan_opt.as_ref());  // JSONL
        }
    }
});
```

`plan_persist_path` is an `Arc<Mutex<Option<PathBuf>>>` repointed every time `state.session` changes (NewSession / LoadSession / fork / `/save` / etc). On `/load`, `Session::load_from` replays all `plan_snapshot` events, and the worker calls `restore_from_session(plan)` to seed the slot — the sidebar redraws with the loaded session's plan.

### Session-swap hygiene (M6.20 BUG M3 + M6.31 PM1)

Every session-swap path runs the same hygiene block:
- `clear_history()` on the agent
- Mint fresh `state.session = Session::new(...)` (or restore from disk)
- Repoint `plan_persist_path` at the new session's JSONL
- `plan_state::clear()` + `reset_step_attempts_external()`
- `approver.reset_session_flag()` — clear yolo flag (M6.20 BUG M2)
- `take_pre_plan_mode()` — drop stashed mode (M6.20 BUG M3)
- `set_current_mode_and_broadcast(state.agent.permission_mode)` — broadcast the reset

**M6.31 PM1** extended the hygiene to the ChangeCwd handler. Pre-fix it ran the hygiene only when `model_changed`; users with one default model across projects took the model-unchanged path and got cross-project state drift (project A's plan visible in project B; saves resolving against project B's directory with project A's session id).

---

## 14. Mutex-poison recovery (M6.31 PM3 + PM4)

`note_step_attempt` and the broadcaster's `fire()` previously silently swallowed mutex poisoning:

- `note_step_attempt` returned `usize::MAX` on poison → driver's `attempt > MAX_RETRIES_PER_STEP` check (3) → immediately force-Failed the next plan step
- `fire` returned no-op on poison → sidebar stopped updating, `plan_snapshot` events stopped persisting to JSONL

Both fixed via `unwrap_or_else(|p| p.into_inner())` — same recovery pattern `slot()` / `update_step()` / `force_step_done()` already used. Mutex poisoning means a concurrent thread panicked while holding the lock; the data inside is still valid, the panic flag just signals "be careful." Recovery is safe for these data structures (the mutated bits are atomic-equivalent — the counter, the broadcaster Box).

No unit test for these fixes — a test that deliberately poisons the process-global mutex leaves it poisoned for the entire `cargo test` process, breaking subsequent tests that depend on a clean slate. Verified by code inspection.

---

## 15. Testing surface

`tools::plan_state::tests` (plan_state.rs:507) has 22 tests covering:

| Category | Tests |
|---|---|
| Submit validation | `submit_assigns_id_and_resets_statuses_to_todo`, `submit_rejects_empty_plan`, `submit_rejects_duplicate_step_ids`, `submit_rejects_blank_id_or_title`, `submit_replaces_prior_plan` |
| Gate transitions | `legal_happy_path_done_steps_advance_one_at_a_time`, `skipping_ahead_is_rejected`, `marking_done_without_in_progress_is_rejected`, `todo_to_failed_with_note_is_legal` (M6.7), `todo_to_failed_without_note_is_rejected`, `failed_step_can_be_retried`, `done_step_cannot_be_re_opened`, `update_without_active_plan_errors`, `unknown_step_id_errors` |
| Step-id queries | `current_step_idx_tracks_in_progress` |
| Force-Done | `force_step_done_bypasses_gate_and_records_note`, `force_step_done_unknown_id_errors` |
| Retry budget | `note_step_attempt_counts_consecutive_attempts`, `note_step_attempt_resets_on_step_change`, `submit_resets_step_attempts`, `clear_resets_step_attempts` |
| Cross-step output | `set_step_output_persists_value`, `set_step_output_truncates_oversize_values`, `set_step_output_handles_multibyte_utf8_at_truncation_boundary` (M6.9), `set_step_output_clear_with_none`, `set_step_output_unknown_id_errors` |
| Stalled detector | `note_turn_completed_without_progress_increments_monotonically` (M6.31 PM2 — pins rising-edge behavior) |

All tests acquire a process-local `test_lock()` mutex (poison-tolerant via `into_inner()`) because they touch the same process-global plan slot, stall counter, and step-attempt counter.

PM3 + PM4 (mutex-poison recovery) — no unit test (see §14).

PM1 (ChangeCwd hygiene) — no unit test. Worker handler is heavyweight (full `WorkerState`, `plan_persist_path` Arc, broadcaster wiring). Verified by code inspection + the existing model_changed branch's test coverage of the same hygiene primitives.

---

## 16. M6.31 audit recap (the most recent sprint)

| Bug | Severity | Fix |
|---|---|---|
| PM1 | MED | ChangeCwd handler runs unconditional hygiene block (was model-conditional). Cross-project state drift fixed. |
| PM2 | LOW | Stalled-turn detector uses `==` instead of `>=` — fires once when threshold first crossed; any plan mutation re-arms. |
| PM3 | LOW | `note_step_attempt` recovers from mutex poison via `into_inner()` instead of returning `usize::MAX` (which force-Failed the next step). |
| PM4 | LOW | Broadcaster `fire()` recovers from mutex poison instead of silently dropping all subsequent broadcasts. |

See `dev-log/147-plan-mode-m6-31-audit-fixes.md` for the full write-up. The plan-mode subsystem is now in a "well-audited, low-bug" state — the remaining deferred items are perf or capability gaps, not correctness issues.

---

## 17. Known gaps (deferred)

LOW-severity items surfaced in the M6.31 audit but not fixed:

- **PM5** — CLI doesn't have the per-step driver (§9). Manual "continue" between steps required.
- **PM6** — `Session::plan` field stays stale during a session. Currently benign; only consumed via `Session::load_from`.
- **PM7** — Mutex vs RwLock perf (every `get()` takes the slot mutex; could be RwLock for read-heavy access).
- **PM8** — Retry budget includes intermediate updates (any UpdatePlanStep counts as an attempt, not just terminal transitions).
- **PM9** — `SubmitPlan` unblocked during awaiting-approval — by design, the model's "I changed my mind" channel. Worth documenting more loudly in the prompt.
- **PM10** — Single-slot `pre_plan_mode_slot` — deeply nested plan-mode would lose history. Currently impossible (re-entry is no-op) but a future "nested plan" feature would need a stack.
- **PM11** — `/goal` + plan mode interaction (both can drive the loop independently — UB on overlap).
- **PM12** — `# name: body` shortcut bypass for memory might escape plan-mode block (TodoWrite is blocked but memory shortcut isn't routed through Tool dispatch).
- **PM13** — Broadcaster test cleanup (the `fire()` slot retains the closure across tests — usually OK because each test re-registers, but a missed registration would leak state).
- **PM14** — Approver yolo flag during plan execution (Approve flips to Auto so the flag isn't consulted; if a future change reverts to Ask during execution, the flag would silently auto-approve).

---

## 18. What lives where (source-line index)

| Concern | File | Symbol / line |
|---|---|---|
| `EnterPlanMode` / `ExitPlanMode` tools | `tools/plan.rs` | `EnterPlanModeTool`, `ExitPlanModeTool` |
| `SubmitPlan` / `UpdatePlanStep` tools | `tools/plan.rs` | `SubmitPlanTool`, `UpdatePlanStepTool` |
| Plan + step structs | `tools/plan_state.rs` | `Plan`, `PlanStep`, `StepStatus` |
| Process-global slot | `tools/plan_state.rs` | `slot()` |
| Submit / update / force-done / clear | `tools/plan_state.rs` | `submit`, `update_step`, `force_step_done`, `clear`, `restore_from_session` |
| Cross-step output (M6.3) | `tools/plan_state.rs` | `set_step_output`, `MAX_STEP_OUTPUT_LEN`, `truncate_at_char_boundary` |
| Stalled detector | `tools/plan_state.rs` | `stall_counter`, `note_turn_completed_without_progress`, `STALL_TURN_THRESHOLD` |
| Per-step retry budget | `tools/plan_state.rs` | `step_attempts`, `note_step_attempt`, `MAX_RETRIES_PER_STEP`, `reset_step_attempts_external` |
| Broadcaster | `tools/plan_state.rs` | `set_broadcaster`, `fire` |
| System reminder (high-level) | `agent.rs:86` | `build_plan_reminder` |
| Layer-2 narrowed view | `agent.rs:313` | `build_execution_reminder` |
| Per-attempt continuation prompt | `agent.rs:485` | `build_step_continuation_prompt` |
| TodoWrite plan-mode block | `agent.rs:1133` | M6.20 BUG M1 |
| Generic mutating-tool block | `agent.rs:1163` | M2 |
| Approval-window gate | `agent.rs:1204` | M3 |
| Mode + pre-plan stash | `permissions.rs` | `PermissionMode::Plan`, `stash_pre_plan_mode`, `take_pre_plan_mode` |
| Plan-completion auto-restore | `tools/plan_state.rs` (inside `update_step` / `force_step_done`) | `if all_done { take_pre_plan_mode() }` |
| Driver loop | `shared_session.rs:1819` | `drive_turn_stream` |
| Step-boundary compaction | `compaction.rs` | `compact_for_step_boundary`, `clear_for_step_boundary` |
| Plan persist path | `shared_session.rs:661` | `plan_persist_path` Arc + broadcaster registration |
| JSONL persistence | `session.rs` | `PlanSnapshotEvent`, `Session::append_plan_snapshot` |
| GUI sidebar handlers | `gui.rs:1639` | `plan_approve`, `plan_cancel`, `plan_retry_step`, `plan_skip_step`, `plan_stalled_continue` |
| GUI ViewEvents | `gui.rs:257`, `gui.rs:282` | `PlanUpdate`, `PlanStalled` |
| CLI `/plan` slash | `repl.rs:4777` | `SlashCommand::Plan` |
