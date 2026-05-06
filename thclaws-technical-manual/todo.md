# TodoWrite

Casual, low-ceremony scratchpad the model uses to track its own multi-step work. One JSON tool call replaces the entire list (full state replacement, not append). Persists as a markdown checklist at `<cwd>/.thclaws/todos.md`. Invisible in the chat surface unless the user opens the file directly. Distinct from the structured plan tools (`SubmitPlan` / `UpdatePlanStep` — sidebar-rendered, sequential-gated, audit-required) and from the in-memory `TaskCreate/Update/Get/List` tools (process-only, no persistence).

This doc covers: the concept + when to use vs other planning tools, the wire format + on-disk markdown shape, the M6.30 validation chain (symlink defense, content sanitization, status validation, duplicate-id check), the per-turn `build_todos_reminder` system-prompt injection, the plan-mode block, the GUI custom-renderer checklist card, the sandbox carve-out, and the testing surface.

**Source modules:**
- `crates/core/src/tools/todo.rs` — `TodoWriteTool`, `TodoItem`, validation chain (`sanitize_field`, `validate_status`, `check_unique_ids`, `check_thclaws_not_symlinked`, `validate_todos`)
- `crates/core/src/agent.rs` — `build_todos_reminder` (per-turn injection at line 580); plan-mode TodoWrite block at line 1133 (M6.20 BUG M1)
- `crates/core/src/default_prompts/system.md` — TodoWrite framing (line 30 + 36 + 49); resume-from-existing rule
- `crates/core/src/gui.rs` — `chat_tool_call` IPC envelope carries `tool_name` + `input` so the frontend can pick the custom checklist renderer (line 164)
- `frontend/src/components/ChatView.tsx` — custom checklist card renderer (line 629-723); keys on `tool_name === "TodoWrite"`

**Cross-references:**
- [`built-in-tools.md`](built-in-tools.md) §9 — TodoWrite tool surface (concise)
- [`agentic-loop.md`](agentic-loop.md) — per-turn pipeline that fires the reminder + plan-mode block
- [`permissions.md`](permissions.md) — approval-gate posture, plan-mode tool blocking
- [`app-architecture.md`](app-architecture.md) §8 — `.thclaws/` sandbox write-policy + intentional carve-outs (TodoWrite, KmsWrite, MemoryWrite share the pattern)
- [`memory.md`](memory.md), [`kms.md`](kms.md) — sibling tools that share the same sandbox carve-out + symlink-defense pattern
- [`loop-and-goal.md`](loop-and-goal.md) — `/loop /goal continue` is the structured-objective alternative when you need audit-driven completion

---

## 1. Concept

TodoWrite is the **lowest-ceremony planning tool** in thClaws's three-tier hierarchy:

```
ceremony     │  user visibility  │  enforcement       │  use when
─────────────┼───────────────────┼────────────────────┼─────────────────────
TodoWrite    │  invisible        │  none              │  informal multi-step
             │  (file only)      │                    │  work; 2-4 subtasks;
             │                   │                    │  finish in 1-2 turns
─────────────┼───────────────────┼────────────────────┼─────────────────────
SubmitPlan + │  sidebar with     │  sequential gating │  user wants to
UpdatePlanStep│ checkmarks       │  per-step audit    │  review the plan
             │                   │  approval-required │  before execution
─────────────┼───────────────────┼────────────────────┼─────────────────────
TaskCreate/  │  CLI /tasks only  │  none              │  in-process tracking
Update/Get/  │  (in-memory)      │                    │  that doesn't need
List         │                   │                    │  to survive restart
```

The model picks based on weight class:
- **Quick edit / single-file change / focused investigation → TodoWrite.** No approval gates needed. Each "verification" is implicit (it compiled, the diff looks right).
- **User asked for a plan to review before executing → SubmitPlan.** Sidebar appears, user clicks Approve.
- **Ephemeral progress tracking inside one process → TaskCreate.** Lives in `Arc<Mutex<TaskStore>>`; no disk.

If the model picks wrong, the user can correct mid-conversation. The system prompt's framing (see §6) and the tool descriptions (see §3) both nudge toward the right choice.

### Why a separate tool from `Write`?

The model COULD use the generic `Write` tool to maintain its own todos.md. Three reasons it doesn't:
1. **Sandbox**: `Write` rejects paths inside `.thclaws/` (reserved for team state). TodoWrite has the carve-out (§8).
2. **Approval ergonomics**: `Write` shows the full file content at every approval; TodoWrite's approval shows just the structured `todos` array.
3. **GUI rendering**: the chat surface picks a custom checklist card for `tool_name === "TodoWrite"`. Generic `Write` would just show a "wrote N bytes" indicator.

---

## 2. On-disk layout

```
<cwd>/.thclaws/todos.md      # the only file TodoWrite touches
```

Path is **relative to process cwd** at write time. When the GUI's "change directory" modal swaps workspace via `std::env::set_current_dir`, subsequent TodoWrite calls land in the new project's `.thclaws/`. Worktree teammates get their own per-worktree `todos.md` (cwd is the worktree root).

The file's content is a markdown checklist:

```markdown
# Todos

- [-] Investigate auth bug (id: 1)
- [ ] Add regression test (id: 2)
- [ ] Update changelog (id: 3)
- [x] Reproduce locally (id: 4)
```

| Glyph | Status |
|---|---|
| `[ ]` | `pending` |
| `[-]` | `in_progress` |
| `[x]` | `completed` |

Empty list renders as `_No todos._` (so the file isn't blank — easier to grep, easier to see at-a-glance).

---

## 3. Wire format

### Input schema

```json
{
  "type": "object",
  "properties": {
    "todos": {
      "type": "array",
      "description": "The complete list of todo items. This replaces the entire existing list.",
      "items": {
        "type": "object",
        "properties": {
          "id":      {"type": "string", "description": "Unique identifier"},
          "content": {"type": "string", "description": "Description of the task"},
          "status":  {"type": "string", "enum": ["pending", "in_progress", "completed"]}
        },
        "required": ["id", "content", "status"]
      }
    }
  },
  "required": ["todos"]
}
```

### Replacement semantics

The `todos` array IS the new state. Every call overwrites the file completely. To "add" a new todo, the model must include the existing items + the new one in a single call. To "remove" a todo, omit it from the array. To update one item's status, send all items with that one's status changed.

This is intentional — partial-update semantics would require the model to remember ids it set in earlier calls, which gets brittle across history compaction. Full replacement keeps the protocol stateless from the model's perspective: read, modify, write.

### Output

```
"Wrote N todo(s) to .thclaws/todos.md (P pending, I in progress, C completed)"
```

Always small (<100 chars). Never triggers the `TOOL_RESULT_CONTEXT_LIMIT = 50_000` truncate-to-disk path.

---

## 4. Validation chain (M6.30)

Before any disk write, every input is validated. First error wins; nothing partially written. Errors are model-actionable — they name the field index, the offending character, the bad enum value, or the duplicate id.

```
parse_todos(input)              ← serde_json deserialization
  ↓
validate_todos(&todos):
    for each (idx, todo):
        sanitize_field(&t.id,      "todos[idx].id",      MAX_ID_LEN=64)
        sanitize_field(&t.content, "todos[idx].content", MAX_CONTENT_LEN=500)
        validate_status(&t.status)
    check_unique_ids(todos)
  ↓
check_thclaws_not_symlinked()   ← TW1: refuse symlinked .thclaws/
  ↓
build markdown + std::fs::write
```

### `sanitize_field` (TW2)

| Rule | Why |
|---|---|
| Non-empty | Empty `id` produces `(id: )` in markdown (parsing ambiguity); empty `content` produces a blank-text bullet (UX confusion) |
| No control chars (`\n`, `\r`, `\t`, `\0`, U+0001-001F, etc.) | Newlines especially: a multi-line `content` would corrupt the markdown bullet structure (the second line wouldn't be a `[ ]` checkbox) AND poison `build_todos_reminder`'s `lines().any(starts_with("- ["))` check |
| Length cap (id ≤ 64, content ≤ 500) | IDs are slug-like; content is a one-line task description. Forces longer descriptions into a separate notes file rather than ballooning the scratchpad |

Error message names the offender:
```
"todos[2].content must not contain control characters (found newline (\\n))"
"todos[1].id too long (87 chars; max 64)"
"todos[0].content must not be empty"
```

### `validate_status` (TW3)

Pre-M6.30 the schema's `enum` was the only enforcement. Provider compliance varies — Anthropic and OpenAI usually respect; Gemini and local Ollama may not. An off-spec `"InProgress"` would silently render as `[ ]` AND count as zero in all three categories (counter only matched exact `"pending"` / `"in_progress"` / `"completed"` strings).

Post-fix:

```rust
fn validate_status(s: &str) -> Result<()> {
    match s {
        "pending" | "in_progress" | "completed" => Ok(()),
        other => Err(Error::Tool(format!(
            "invalid status '{other}' — must be 'pending' | 'in_progress' | 'completed'"
        ))),
    }
}
```

Error echoes the bad value so the model fixes the right field on retry.

### `check_unique_ids` (TW4)

`HashSet<&str>` over the slice; first duplicate triggers:

```
"duplicate todo id: '1' — every todo must have a unique id"
```

Pre-fix consequences: file kept both bullets, frontend logged React key collision warnings (only first rendered correctly), next-read state was ambiguous.

### `check_thclaws_not_symlinked` (TW1, **HIGH**)

```rust
fn check_thclaws_not_symlinked() -> Result<()> {
    if let Ok(md) = std::fs::symlink_metadata(".thclaws") {
        if md.file_type().is_symlink() {
            return Err(Error::Tool(
                "refusing to write — .thclaws/ is a symlink. Remove the \
                 symlink (or its target) and let TodoWrite create a real \
                 directory."
                    .into(),
            ));
        }
    }
    Ok(())
}
```

Pre-fix `std::fs::write(.thclaws/todos.md, ...)` followed the symlink. An attacker-planted `.thclaws -> /tmp/anywhere` symlink (from a malicious clone or shared misconfigured workspace) let TodoWrite escape the project root. **Verified empirically before fix** with a 30-line repro:

```
LEAKED: write escaped via .thclaws symlink → /var/folders/.../outside-N/todos.md
```

The `requires_approval=true` gate did not protect — the approval modal shows the tool name + JSON input, not the resolved disk path. User has no way to see the write is escaping. KMS (`kms::writable_page_path`) and Memory (`memory::writable_entry_path`) both defended via `symlink_metadata` + `is_symlink()`; TodoWrite was the missing piece in the carve-out family.

### Validation table

| Failure mode | Pre-M6.30 | Post-M6.30 |
|---|---|---|
| `content: "line one\nline two"` | File corrupted; reminder parser confused | `must not contain control characters (found newline (\n))` — model removes newline + retries |
| `status: "InProgress"` | Silently `[ ]`; counter `0/0/0` | `invalid status 'InProgress' — must be ...` — model uses canonical form |
| Two todos with `id: "1"` | Both bullets in file; React warnings | `duplicate todo id: '1' — every todo must have a unique id` |
| `.thclaws -> /tmp/x` symlink | Write escapes project | `refusing to write — .thclaws/ is a symlink. ...` |

---

## 5. Per-turn reminder injection

`crate::agent::build_todos_reminder()` is called every turn before the agent runs ([agent.rs:580](thclaws/crates/core/src/agent.rs#L580)). It reads `.thclaws/todos.md` from cwd and returns `Some(reminder_text)` when:

- File exists AND
- File is non-empty AND
- File contains at least one incomplete checkbox (`- [ ]` pending OR `- [-]` in_progress)

Returns `None` when:
- File missing
- File empty / whitespace-only
- All items are `[x]` completed (no point nagging about a closed-out list)

When fired, the reminder looks like:

```markdown
## Existing todos (.thclaws/todos.md)

A scratchpad todo list from a prior session is present in this workspace.
Surface this to the user before asking what to work on, and offer to
resume incomplete items (`[ ]` pending or `[-]` in_progress) or replace
the list. Don't ask "what should we do?" while these answers are sitting
in front of you.

Current contents:

```markdown
<bounded contents>
```

If the user wants to resume, mark the next pending item as `in_progress`
via TodoWrite (passing the full list with that one item flipped) and
start work on it. If they want a fresh start, write an updated list via
TodoWrite that reflects the new direction.
```

### Caps (M6.18 BUG M6)

`crate::memory::truncate_for_prompt(raw, 80, 6_000, ".thclaws/todos.md")` — 80 lines / 6 KB cap. An unmaintained list (200+ entries) gets truncated with a notice. Cap is generous for a typical scratchpad: headers + bullets average ~50 bytes/line.

### Why always-on instead of every-N-turns

Claude Code's analog (`todo_reminder` in `claude-code-src/utils/messages.ts:3663`) fires every N turns. thClaws fires every turn for two reasons:
1. We don't have the turn-count tracking infrastructure
2. The content is small enough (~200 bytes for a typical 3-5 item list, 6KB worst case) that always-on is acceptable

Real-world testing showed prompt-only guidance ("check `.thclaws/todos.md` before asking") wasn't enough on some models — gpt-4.1 in particular still asked the user instead of reading the file. Auto-injecting the contents removes the model's option to ignore the rule.

### Composition with plan reminder

Both `build_plan_reminder` (active plan in plan-mode) and `build_todos_reminder` may fire. They're chained:

```rust
let chained = match (plan_reminder, todos_reminder) {
    (Some(p), Some(t)) => Some(format!("{p}\n\n{t}")),
    (Some(p), None) => Some(p),
    (None, Some(t)) => Some(t),
    (None, None) => None,
};
```

Both surface; the model sees both pieces of context. Order is plan-then-todos (plan dominates the structural attention).

---

## 6. Plan-mode block (M6.20 BUG M1)

In plan mode, mutating tools are blocked at dispatch with a structured "use Read/Grep/Glob/Ls; SubmitPlan when ready" message. **TodoWrite gets a separate, more specific block fired BEFORE the generic one** ([agent.rs:1133](thclaws/crates/core/src/agent.rs#L1133)):

```rust
if matches!(permission_mode, PermissionMode::Plan) && name == "TodoWrite" {
    let blocked = "Blocked: TodoWrite is the casual scratchpad outside plan mode. \
                   In plan mode, call SubmitPlan to publish your plan to the \
                   sidebar — UpdatePlanStep tracks progress per step.";
    // ... return blocked tool_result
}
```

Pre-M6.20 the generic block ran first (because TodoWrite has `requires_approval=true`, the generic block's matchers fired ahead of any TodoWrite-specific arm). The model always saw "Use Read/Grep/Glob/Ls" instead of "Use SubmitPlan." This confused models in tests — they'd `TodoWrite` a draft list AND `SubmitPlan` the same content. The TodoWrite-first ordering directs the model to the right tool for plan-mode work.

---

## 7. GUI custom rendering

The chat surface picks a custom checklist-card renderer for `tool_name === "TodoWrite"` instead of the generic one-line tool indicator.

### IPC envelope ([gui.rs:164](thclaws/crates/core/src/gui.rs#L164))

```json
{
  "type": "chat_tool_call",
  "name": "TodoWrite",                  // human-readable label
  "tool_name": "TodoWrite",             // unmangled, used as render key
  "input": {
    "todos": [
      {"id": "1", "content": "Investigate bug", "status": "in_progress"},
      ...
    ]
  }
}
```

`tool_name` is the dispatch key. `input` carries the raw todos array so the renderer doesn't need a follow-up IPC round-trip.

### Frontend renderer ([ChatView.tsx:629-723](thclaws/frontend/src/components/ChatView.tsx))

```tsx
const todos = (() => {
  if (msg.toolKind !== "TodoWrite") return null;
  const inp = msg.toolInput as { todos?: unknown } | undefined;
  if (!inp || !Array.isArray(inp.todos)) return null;
  return inp.todos as TodoItemInput[];
})();

{todos && todos.length > 0 && (
  <div className="mt-1 rounded border px-2 py-1.5">
    {todos.map((t) => (
      <div key={t.id} className="flex items-baseline gap-2">
        <span style={{color: colorForStatus, fontFamily: "Menlo, ..."}}>
          {glyphForStatus}
        </span>
        <span style={{textDecoration: t.status === "completed" ? "line-through" : "none"}}>
          {t.content}
        </span>
      </div>
    ))}
  </div>
)}
```

| Status | Glyph | Color | Style |
|---|---|---|---|
| `completed` | `✓` | success green | strikethrough |
| `in_progress` | `◉` | warning amber | normal |
| `pending` | `☐` | text-secondary | normal |

`t.id` is used as React `key`. Pre-M6.30 duplicate ids triggered React warnings; the M6.30 server-side dedup eliminates the case at the source.

XSS-safe: React's default text-node escaping renders `t.content` literally (any HTML in content shows as text, not parsed markup). Combined with M6.30's control-char rejection, the renderer can trust the input.

---

## 8. Sandbox carve-out

`<cwd>/.thclaws/` is normally **write-blocked** by `Sandbox::check_write` ([app-architecture.md §8](app-architecture.md)) — that directory holds team state (`settings.json`, `agents/`, `mailboxes/`, `sessions/`, `kms/`, `memory/`). Generic `Write` / `Edit` / `Bash mv` etc. all reject paths inside it.

TodoWrite intentionally bypasses this check. It calls `std::fs::write` directly with the hardcoded path `.thclaws/todos.md`, never going through `Sandbox::check_write`. Same intentional carve-out family as `KmsWrite` / `KmsAppend` (writes inside `.thclaws/kms/`) and `MemoryWrite` / `MemoryAppend` (writes inside `.thclaws/memory/`).

### Why bypass is safe

| Property | TodoWrite |
|---|---|
| Path is user-controlled? | No — hardcoded `.thclaws/todos.md` |
| Path traversal vector? | No — no `..`, no separators in user input that touches the path |
| Symlink escape? | **Defended by M6.30 `check_thclaws_not_symlinked`** |
| Approval-gated? | Yes (`requires_approval = true`) — Ask mode prompts |
| Limit on write size? | Soft — input validated (max 64 + 500 chars/field) but no array-length cap (TW5 deferred) |

The carve-out's safety relies on the path being constant + the validation chain (M6.30) catching the symlink case. Both invariants hold.

---

## 9. Code organization

```
crates/core/src/tools/
└── todo.rs (~570 LOC)              ── TodoWriteTool, TodoItem,
                                       validation chain (sanitize_field,
                                       validate_status, check_unique_ids,
                                       check_thclaws_not_symlinked,
                                       validate_todos), markdown rendering,
                                       tests

crates/core/src/agent.rs              build_todos_reminder() — per-turn
                                       system-prompt injection (line 580+)
                                      Plan-mode TodoWrite block (line 1133+;
                                       M6.20 BUG M1)

crates/core/src/gui.rs                chat_tool_call IPC envelope
                                       carries tool_name + input (line 164+)

crates/core/src/default_prompts/
└── system.md                         TodoWrite framing (lines 30, 36, 49):
                                       "scratchpad", "small job → use",
                                       "BEFORE asking for context, check
                                       `.thclaws/todos.md`"

frontend/src/components/
└── ChatView.tsx                      Custom checklist card renderer
                                       (lines 629-723); keys on
                                       tool_name === "TodoWrite"
```

---

## 10. Testing

| Test | Coverage |
|---|---|
| `write_todos_creates_markdown` | Multi-status round-trip; counter accuracy |
| `write_empty_todos` | Empty array → `_No todos._` |
| `overwrites_existing_todos` | Full replacement (no stale lines) |
| `missing_todos_field_errors` | Missing `todos` field |
| `tool_metadata` | Name + approval posture + schema shape |
| `description_positions_todowrite_as_scratchpad` | Description framing vs SubmitPlan |
| `description_tells_model_to_resume_from_existing_todos_md` | Resume rule present |
| `description_includes_iteration_discipline` | One-in-progress rule, immediate-completion rule, no-batching rule |
| `todo_item_markdown_rendering` | Glyph mapping per status |
| `parse_todos_from_json` | JSON deserialization |
| **M6.30:** | |
| `rejects_newline_in_content` | TW2; verifies error names "newline" |
| `rejects_tab_in_id` | TW2; tab variant |
| `rejects_empty_id` | TW2; empty edge case |
| `rejects_oversized_content` | TW2; 501-char content |
| `rejects_invalid_status` | TW3; `"InProgress"` rejected with offender echoed |
| `rejects_hyphenated_status` | TW3; common-typo variant |
| `rejects_duplicate_ids` | TW4; error names the duplicate |
| `rejects_symlinked_thclaws_dir` | TW1; plants `.thclaws -> outside`, asserts rejection AND that no write leaked to symlink target |
| `clean_inputs_still_write` | Regression guard — validation chain doesn't reject legitimate use |

19 tests in `tools::todo::tests` (was 11 pre-M6.30, +8 net new).

The symlink test mutates process cwd (via `std::env::set_current_dir`) so it shares the `kms::test_env_lock` (a `pub(crate)` mutex) to serialize against parallel tests. Restores cwd before any assertion can panic — explicit cleanup matters because tempfile teardown depends on the parent dir being valid.

---

## 11. Migration / known limitations

### Backwards compatibility

The M6.30 validation chain is backwards compatible for all well-formed callers — every new error fires on inputs that were previously silently corrupting state:
- Pre-existing `.thclaws/todos.md` files render correctly (validation only applies to new writes)
- Models that pass valid JSON Schema-conforming inputs see no change
- The model now sees clear errors it can correct on retry where it previously saw silent corruption

If a user has `.thclaws` as a symlink (rare but possible — some shared workspaces, some clone scripts), TodoWrite now errors. Same posture as KMS / Memory. Workaround: replace the symlink with a real directory.

### Known limitations (deferred from M6.30 audit)

| ID | Severity | Issue |
|---|---|---|
| TW5 | LOW | No cap on todo count. 10K todos × 50 bytes = 500KB file. Reminder caps prompt output; the file itself doesn't. Soft cap at 200 entries would close it. |
| TW6 | LOW | `(id: foo)` in `content` confuses any future regex extractor. Currently nothing extracts ids from rendered markdown — purely defensive. Could move id to HTML comment (`<!-- id:1 -->`) for visual invisibility + parser uniqueness. |
| TW7 | LOW | Concurrent TodoWrite calls = last-writer-wins (no `flock`). Same posture as KMS pre-M6.24. Wrap in `fs2::FileExt::lock_exclusive` for parity. Not currently a real footgun (TodoWrite rarely called in parallel). |

### Not currently planned

- **Multiple lists** — a single `.thclaws/todos.md` per project. No `/todo new <name>` for separate lists. KMS (`/kms use <name>`) is the precedent for multi-list patterns.
- **History / undo** — full replacement loses prior state. The user can `git diff .thclaws/todos.md` if `.thclaws/` is committed; otherwise gone.
- **Sidebar surfacing** — by design, TodoWrite is invisible in the chat surface (no sidebar entry). The custom checklist card in chat is the only UI affordance. Users who want sidebar-rendered plans use `SubmitPlan`.

---

## 12. Sprint chronology

- **Phase 9** (initial) — `TodoWriteTool`, basic schema, markdown rendering, plan-mode dispatch
- **M6.18 BUG M6** (`dev-log/136`) — `build_todos_reminder` cap at 80 lines / 6 KB; shared `truncate_for_prompt` helper
- **M6.20 BUG M1** (`dev-log/138`) — TodoWrite-specific plan-mode block fires BEFORE generic block; sends model to SubmitPlan instead of "use Read/Grep/Glob/Ls"
- **M6.30** (`dev-log/146`) — symlink defense (TW1, HIGH; verified empirically), content/id sanitization (TW2, MED), server-side status validation (TW3, MED), duplicate-id check (TW4, LOW); +8 regression tests
