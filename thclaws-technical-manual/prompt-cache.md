# Prompt cache

A cross-provider reference for prompt caching in thClaws: how the `Usage` struct surfaces cache hits, how each provider implements (or doesn't implement) the cache, what byte-stability rules the request builder follows to keep caches warm, where cache stats land in the GUI/CLI displays, and how daily cache totals are persisted.

Prompt caching matters because:
- **Cost** — cached input tokens are billed at 10-25% of the regular input rate (Anthropic: 10% read, 25% write; OpenAI: 50%; DeepSeek: 10%; Gemini implicit: 25%).
- **Latency** — cached prefixes don't go through the prefill phase, so first-token latency drops dramatically for long system prompts.
- **Capacity** — agentic loops re-send the same system prompt + tool defs every turn. Without caching, that's the dominant input-token cost; with caching, it's near-zero amortized.

This doc covers ALL the cache surfaces in thClaws. For per-provider deep-dives see [`provider-anthropic.md`](provider-anthropic.md) (most detailed cache impl), [`provider-openai.md`](provider-openai.md), [`provider-gemini.md`](provider-gemini.md), [`provider-responses.md`](provider-responses.md), [`provider-agentsdk.md`](provider-agentsdk.md).

**Source modules:**
- `crates/core/src/providers/mod.rs::Usage` — the canonical cache field shape
- `crates/core/src/providers/anthropic.rs` — only provider with EXPLICIT cache control (cache_control markers)
- `crates/core/src/providers/openai.rs::parse_openai_usage`
- `crates/core/src/providers/openai_responses.rs` — Responses API usage parser
- `crates/core/src/providers/gemini.rs` — Gemini usageMetadata parser
- `crates/core/src/providers/agent_sdk.rs` — Claude Code subprocess result-frame parser
- `crates/core/src/usage.rs::UsageStore` — per-day cache totals persisted to `~/.config/thclaws/usage/<provider>/<model>.json`
- `crates/core/src/repl.rs:~4800` — per-turn cache pill rendering

---

## 1. The canonical `Usage` shape

```rust
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_creation_input_tokens: Option<u32>,
    pub cache_read_input_tokens: Option<u32>,
}
```

`input_tokens` = **uncached new** input tokens this turn (NOT the total prompt). Matches Anthropic's wire semantics.

`cache_creation_input_tokens` = tokens written to the cache this turn (Anthropic's "cache write"; you pay 25% premium for these but they're cheap to read on subsequent turns).

`cache_read_input_tokens` = tokens read FROM the cache this turn (cheap — 10% of normal input rate for Anthropic).

Total billable input = `input_tokens` (uncached) + `cache_creation_input_tokens` (write premium) + `cache_read_input_tokens` (read discount).

For providers that report a TOTAL prompt count (OpenAI's `prompt_tokens`, Gemini's `promptTokenCount`) the parser **subtracts the cached portion** to produce the canonical `input_tokens` field. Without subtraction, `usage.rs::record` would double-count: `entry.input += 5000` AND `entry.cache_read += 4500` = 9500 contribution from a turn that actually consumed 5000 tokens billable.

`Option<u32>` because not all providers report cache info — `None` means "not surfaced for this provider", NOT "zero cache activity". The provider may still have cached server-side; we just can't see it. After M6.22, OpenAI/OpenAI-Responses/Gemini/DeepSeek all surface `cache_read_input_tokens` correctly; previously they hardcoded `None`.

`Usage::accumulate(&other)` (mod.rs:537-553) sums the cache fields across iterations of one turn — `(Some(a), Some(b))` → `Some(a+b)`, `(Some(a), None)` → `Some(a)`, etc. So a multi-iteration turn with 3 server calls sees the cache write amortized across them.

---

## 2. Cache-implementation matrix

| Provider | Cache type | Reads `Usage` fields? | Writes `cache_control`? | Cost reduction |
|---|---|---|---|---|
| **Anthropic** | explicit (`cache_control: ephemeral`) | ✓ `cache_creation_input_tokens` + `cache_read_input_tokens` | ✓ system + last tool + 2nd-to-last msg | 10% read, 25% write |
| **AgentSdk** | implicit (Claude Code manages it) | ✓ via subprocess `result` frame | n/a (delegated) | same as Anthropic |
| **OpenAI** | implicit (server-side ≥1024 tokens) | ✓ since M6.22 — `prompt_tokens_details.cached_tokens` → `cache_read_input_tokens` | n/a (auto) | 50% read |
| **OpenAIResponses** | implicit (server-side, includes cross-call via `previous_response_id`) | ✓ since M6.22 — `input_tokens_details.cached_tokens` | n/a (auto) | 50% read |
| **OpenRouter** (via OpenAI provider) | depends on upstream | ✓ since M6.22 if upstream emits standard fields | n/a | depends on routed-to provider |
| **AgenticPress / DashScope / ZAi / LMStudio / OpenAICompat / ThaiLLM** (via OpenAI provider) | depends on upstream | ✓ since M6.22 if upstream emits `prompt_tokens_details.cached_tokens` | n/a | varies |
| **DeepSeek** (via OpenAI provider) | implicit (`prompt_cache_hit_tokens`/`prompt_cache_miss_tokens`) | ✓ since M6.22 — defensive dual-check in `parse_openai_usage` | n/a (auto) | 10% read |
| **Gemini** | implicit + explicit `cachedContents` resource | ✓ since M6.22 — `cachedContentTokenCount` for implicit; explicit not wired | ✗ explicit cache not wired (G6 deferred) | 25% (implicit) |
| **Ollama** | none (local, no caching concept) | n/a — hardcoded None | n/a | n/a |
| **OllamaCloud** | none surfaced | n/a — hardcoded None | n/a | n/a |
| **AzureAIFoundry** (via Anthropic provider) | implicit via Anthropic shim | ✓ via Anthropic parser | ✓ via Anthropic builder | 10% read |
| **OllamaAnthropic** (via Anthropic provider) | none (Ollama doesn't cache) | parser returns 0 | builder sends `cache_control` (Ollama ignores) | n/a |

After M6.22 every provider with server-side prompt caching surfaces the cached portion to the user. Pre-M6.22, OpenAI/OpenAIResponses/Gemini/DeepSeek users got the cost discount silently but couldn't tell from the per-turn pill or daily totals.

---

## 3. Anthropic — the gold standard implementation

The only provider with full explicit cache control. See [`provider-anthropic.md`](provider-anthropic.md) §3 for the request body details; here's the cache-specific rationale.

### Three breakpoints (of Anthropic's 4 max per request)

```
┌────────────────────────────────┐
│ system prompt                  │ ← breakpoint 1 (ephemeral)
├────────────────────────────────┤
│ tool defs (...)                │
│ ...                            │
│ tool def N                     │ ← breakpoint 2 (ephemeral) — covers all tools
├────────────────────────────────┤
│ msg 1                          │
│ msg 2                          │
│ ...                            │
│ msg N-2 (second-to-last)       │ ← breakpoint 3 (ephemeral; only when msgs ≥ 3)
├────────────────────────────────┤
│ msg N-1 (newest user turn)     │ ← never cached (live input)
└────────────────────────────────┘
```

#### Why these three?

1. **System prompt** — byte-stable across turns until composer rebuilds it (CLAUDE.md change, memory mutation, KMS swap). Most stable single chunk in the request.
2. **Last tool definition** — `cache_control` on the last tool covers the entire tools array (Anthropic uses prefix caching, so a marker on element N caches elements 1..N+1). Tool defs change only when MCP servers connect/disconnect or skills install — rare.
3. **Second-to-last message** — the rolling-conversation breakpoint. Index `N-2` is byte-stable for the next call (the last message is the live user turn, uncached by definition; the second-to-last is the assistant's reply to the prior user, frozen in history).

#### Why NOT the last message?

The newest user turn is uncached input by definition — that's where the new content lives. A breakpoint there would cache content that won't reappear in any future request (the user types something different next turn).

#### Why skip the rolling breakpoint when history < 3?

Anthropic's minimum cacheable prefix is 1024 tokens. 1-2 messages rarely qualify — a 200-token user message + 800-token assistant reply might or might not hit the threshold. The breakpoint slot is more valuable on a later turn when the prefix is definitely cacheable.

### Byte-stability contract

The 3-breakpoint pattern is worth nothing if the cached prefix changes byte-for-byte between turns. The composer ([`context-composer.md`](context-composer.md)) is built around this — system prompt sections are concatenated in deterministic order, memory entries are sorted, KMS indices are deterministic, etc.

The provider has its own contribution: `build_body` produces a byte-stable body across two calls with the same `StreamRequest`. Test `build_body_message_cache_is_byte_stable_across_calls` (anthropic.rs:516-538) explicitly guards this — calls `build_body` twice with the same input, asserts both serializations are equal. Without this test, a `serde_json` change could silently break caching.

The third breakpoint shifts every turn (the `N-2` index moves as N grows), but the CONTENT at that index doesn't shift — turn 7's `msg[5]` is the same bytes as turn 8's `msg[5]`. So the cache prefix grows monotonically.

### Request fields parsed back

`message_delta` event carries:
```json
"usage": {
    "input_tokens": 100,
    "output_tokens": 50,
    "cache_creation_input_tokens": 1500,
    "cache_read_input_tokens": 12000
}
```

`anthropic.rs::parse_sse_event` (line 326-337) maps these directly to the `Usage` struct.

---

## 4. AgentSdk — passthrough from Claude Code

`agent_sdk.rs:336-346` parses the terminal `result` frame:
```rust
"result" => {
    let usage = v.get("usage").map(|u| super::Usage {
        input_tokens: u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0) as u32,
        output_tokens: u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0) as u32,
        cache_creation_input_tokens: u.get("cache_creation_input_tokens")
            .and_then(Value::as_u64).map(|v| v as u32),
        cache_read_input_tokens: u.get("cache_read_input_tokens")
            .and_then(Value::as_u64).map(|v| v as u32),
    });
    yield ProviderEvent::MessageStop { stop_reason: Some("end_turn".into()), usage };
}
```

Claude Code manages cache_control internally and reports the same Anthropic-shape usage object that thClaws's parser already understands. The cache benefits are real — they show up in your subscription billing and in thClaws's per-day usage totals — but thClaws doesn't manage the markers itself.

---

## 5. Other providers' implicit caching

OpenAI, Gemini, DeepSeek, OpenAI Responses all do server-side automatic prompt caching. The user gets the cost savings WITHOUT any client-side action — but thClaws also doesn't surface them in the UI today (see §6).

### OpenAI (Chat Completions)

Auto-caches prefixes ≥ 1024 tokens. Returns cached counts under:
```json
"usage": {
    "prompt_tokens": 5000,
    "completion_tokens": 200,
    "prompt_tokens_details": {
        "cached_tokens": 4500
    }
}
```

`cached_tokens` represents the cached portion of `prompt_tokens` (NOT additive). 50% discount on cached portion.

### OpenAI Responses

Same `prompt_tokens_details.cached_tokens` shape under `response.usage`. Plus server-side conversation continuation via `previous_response_id` is technically a form of cache (the server already knows the prior turn).

### DeepSeek

Different field names than OpenAI (DeepSeek went their own way):
```json
"usage": {
    "prompt_tokens": 5000,
    "prompt_cache_hit_tokens": 4500,
    "prompt_cache_miss_tokens": 500,
    "completion_tokens": 200
}
```

90% discount on `prompt_cache_hit_tokens` portion.

### Gemini

Two flavors:
- **Implicit caching** (auto, ≥4096 tokens for Pro/Flash) — surfaces as `usageMetadata.cachedContentTokenCount`
- **Explicit `cachedContents` resource** — separate API to create a cached content blob, then reference it via `cached_content` in the request. Cost is per-storage-hour PLUS per-cached-token-read. Useful for huge fixed prefixes (long system prompts, codebase dumps).

```json
"usageMetadata": {
    "promptTokenCount": 5000,
    "candidatesTokenCount": 200,
    "cachedContentTokenCount": 4500,
    "totalTokenCount": 5200
}
```

25% discount on the cached portion (implicit).

### Ollama / OllamaCloud

No prompt caching. Local models keep KV-cache across contiguous turns IF the model file isn't unloaded — but that's GPU/CPU state, not surfaced via API. OllamaCloud doesn't surface caching either.

---

## 6. Cache-stat parsing history (M6.21 audit + M6.22 shipped)

### G1 — OpenAI provider hardcodes cache fields to None [SHIPPED in M6.22]

`openai.rs::parse_openai_usage` (line 471-487):
```rust
fn parse_openai_usage(v: &Value) -> Option<Usage> {
    let u = v.get("usage")?;
    let input = u.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0);
    let output = u.get("completion_tokens").and_then(Value::as_u64).unwrap_or(0);
    if input == 0 && output == 0 { return None; }
    Some(Usage {
        input_tokens: input as u32,
        output_tokens: output as u32,
        cache_creation_input_tokens: None,    // ← never reads prompt_tokens_details.cached_tokens
        cache_read_input_tokens: None,
    })
}
```

OpenAI's auto-cache works server-side (you ARE getting the cost discount), but `usage.prompt_tokens_details.cached_tokens` never lands in `Usage`. The CLI/GUI never displays cache hits for OpenAI / OpenRouter / OpenAICompat / DashScope / ZAi / LMStudio / AgenticPress / ThaiLLM (everyone routing through `OpenAIProvider`).

**Impact:** the per-turn token pill shows `5000 in / 200 out` for an OpenAI turn that actually consumed `500 fresh + 4500 cached`. User can't tell the cache is working. Daily totals also undercount cache reads.

**Fix:** add a third extraction:
```rust
let cached = u.pointer("/prompt_tokens_details/cached_tokens").and_then(Value::as_u64);
Some(Usage {
    input_tokens: (input - cached.unwrap_or(0).min(input)) as u32,   // uncached portion
    output_tokens: output as u32,
    cache_creation_input_tokens: None,                                // OpenAI doesn't separate writes
    cache_read_input_tokens: cached.map(|v| v as u32),
})
```

OpenAI doesn't distinguish writes from reads (auto-managed; user pays the write premium silently the first time). Map cached_tokens → cache_read_input_tokens; leave cache_creation as None.

### G2 — OpenAI Responses parser hardcodes cache fields [SHIPPED in M6.22]

`openai_responses.rs:386-393`:
```rust
let usage = v.get("response").and_then(|r| r.get("usage"))
    .map(|u| Usage {
        input_tokens: ...,
        output_tokens: ...,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
    });
```

Same problem. Responses API also exposes `cached_tokens` (under `response.usage.input_tokens_details.cached_tokens` per the latest OpenAI spec).

### G3 — Gemini parser hardcodes cache fields [SHIPPED in M6.22]

`gemini.rs:540-545`:
```rust
let usage = v.get("usageMetadata").map(|u| Usage {
    ...
    cache_creation_input_tokens: None,
    cache_read_input_tokens: None,
});
```

`cachedContentTokenCount` from Gemini's `usageMetadata` not parsed. Implicit caching savings invisible.

### G4 — DeepSeek cache hit/miss fields not recognized [SHIPPED in M6.22]

DeepSeek goes through `OpenAIProvider`. DeepSeek's wire format uses `prompt_cache_hit_tokens` and `prompt_cache_miss_tokens` (DIFFERENT field names from OpenAI's `prompt_tokens_details.cached_tokens`). Neither the proposed Fix-G1 patch nor the current parser would catch them.

**Fix:** detect DeepSeek's variant. Either:
1. Add a per-provider parser (pass `ProviderKind` into `OpenAIProvider`), OR
2. Defensively check both field names: `prompt_cache_hit_tokens` (DeepSeek) AND `prompt_tokens_details.cached_tokens` (OpenAI/most others).

The defensive option is simpler and unlikely to false-positive (no provider should expose both with conflicting meanings).

### G5 — Anthropic cache_control on tool_result blocks [SHIPPED in M6.22]

If the second-to-last message contains tool_result blocks (common — assistant response → tool dispatch → user message of tool_result), `cache_control` gets attached to the last tool_result block. Anthropic supports this per current API docs (verified: text, tool_use, tool_result, image, document, thinking all accept `cache_control`).

**Defensive safety net shipped:** if Anthropic ever regresses (or a specific model rejects), the provider now retries ONCE without cache markers and surfaces the result. Behavior:

1. Send request with all 3 cache_control breakpoints (current behavior).
2. If status is 200, parse + stream as normal.
3. If status is 400 AND body contains substring `"cache_control"`, log a yellow warning and retry the same request via `build_body_no_cache(req)` — strips ALL cache_control markers from system, tools, and second-to-last message.
4. If retry succeeds: stream normally (this turn loses the cache discount; next turn tries again with cache).
5. If retry fails: surface `http {status} (retry without cache also failed): {body}`.
6. Other 400s (content_too_long, invalid model, malformed messages) are NOT retried — surfaced unchanged so the user sees the actual issue.

The safety net is per-request (no provider-level "cache disabled" sticky flag). If the rejection was transient, the next turn tries cache mode again. If it's permanent (API regression), the user pays for an extra request per turn but the conversation works.

**Tests:** `cache_control_lands_on_text_block`, `cache_control_lands_on_tool_use_block`, `cache_control_lands_on_tool_result_block`, `build_body_no_cache_strips_all_cache_control_markers`, `cache_control_400_triggers_retry_without_cache`, `non_cache_400_does_not_trigger_retry`.

### G6 (LOW, deferred): Explicit Gemini `cachedContents` not wired

Gemini's `cachedContents` resource lets you create a cached blob (paid hourly) and reference it. Useful for huge fixed prefixes (massive codebases, long manuals). Not wired — every turn re-sends the full content.

**Status:** known limitation. Implementation would need a new Provider trait method (e.g. `prepare_cache(&self, content: ...) -> Result<CacheHandle>`) plus per-provider impls.

---

## 7. Cache stats surfaces

### Per-turn pill (CLI repl.rs ~4800)

```
[tokens: 5000 in (4500 cached) / 200 out · 3.2s]
```

The `(N cached)` portion only renders when `cache_read_input_tokens.is_some()` AND > 0. For providers where the field is always None (OpenAI, Gemini, etc. — the GAP G1-G3 issue), the parenthetical never shows.

### `/usage` slash command

Aggregates from `~/.config/thclaws/usage/<provider>/<model>.json`. Each entry has `cache_write` and `cache_read` u64 counters that sum across turns. Source: `usage.rs::UsageStore::record`:
```rust
entry.cache_write += usage.cache_creation_input_tokens.unwrap_or(0) as u64;
entry.cache_read += usage.cache_read_input_tokens.unwrap_or(0) as u64;
```

`unwrap_or(0)` means providers reporting `None` contribute 0 to the daily totals — silently consistent with the per-turn pill, but undercount for OpenAI/Gemini/DeepSeek users.

### Session JSONL

`Session.messages` stores assistant messages with their `Usage` if the agent loop captured one. The on-disk format includes the cache fields when present (via `serde_json` skip-if-none). Tools that read the JSONL externally (audit scripts, billing exporters) can inspect cache data per-message — for providers that report it.

### GUI sidebar

`ViewEvent::TokenUsage(Usage)` broadcasts to the React sidebar's per-turn usage display. Renders the `cached: N` line only when non-None / non-zero — same gating as the CLI pill.

---

## 8. Cache invalidation triggers

Cached prefixes break when ANY byte in the cached prefix changes. Things that invalidate Anthropic's cache:

- **System prompt change** — CLAUDE.md edited, memory file added/removed, KMS swap, skills install, plugins refresh, marketplace cache update changing in-prompt skill descriptions
- **Tool def change** — MCP server connect/disconnect, skill install (if skill exposes tools), `/mcp add`
- **History compaction** — `compact_with_summary` rewrites the message list, breaking the second-to-last-msg breakpoint
- **Session swap** — different history → different cached prefix
- **Model swap** — caches are per-model on Anthropic's side
- **Provider swap** — obvious, different cache pool entirely

Things that DON'T invalidate (explicitly preserve cache):
- New user message (newest message is uncached by design)
- Adding a new tool_result to history (becomes part of the next turn's prefix, doesn't disturb prior)
- Changing model temperature, max_tokens — those don't enter the cached prefix
- Cancellation/retry of the same turn — the next attempt uses the same prefix

### Anthropic's 5-minute TTL

Anthropic's "ephemeral" cache has a 5-minute idle TTL. After 5 minutes without the prefix being referenced, it evicts. Practical implications:
- Quick consecutive turns (< 5 min) hit the cache
- Coming back after a coffee break — first turn re-pays the write premium, then subsequent turns are warm again
- The retry sleep in [`agentic-loop.md`](agentic-loop.md) (1, 2, 4, 8s exponential backoff up to 7s combined) stays inside the TTL

### Plan mode interactions

Entering Plan mode doesn't directly invalidate the cache, but the dynamic system-prompt rebuild ([`permissions.md`](permissions.md) §5) injects a plan-mode reminder as a per-turn suffix to the base system prompt. The byte-stability is preserved AS LONG AS the plan-state hasn't changed between turns. Submitting/updating a plan changes the suffix → cache invalidation on the system breakpoint.

---

## 9. Empirical cache effectiveness (Anthropic)

Typical agentic loop on Claude Sonnet 4 with `~10K` system prompt + `~3K` tool defs:

| Turn | Input tokens | Cache write | Cache read | Notes |
|---|---|---|---|---|
| 1 (cold) | 200 | 13000 | 0 | Pays write premium for system + tools |
| 2 | 50 | 100 | 13000 | Reads system + tools; writes new msg N-2 |
| 3 | 70 | 100 | 13100 | Reads system + tools + prior msg N-2 |
| ... | ... | ... | growing | |
| 10 | 80 | 100 | 14500 | Cache read ≈ entire prior conversation |

The dominant cost shifts from "input tokens" to "output tokens + new user message" within 2-3 turns. For a 20-turn conversation, total cost is ~25% of what it would be without caching.

The break-even point (where cache savings exceed the 25% write premium) is at turn 2 for any prefix that fits the cache.

---

## 10. Code organization

```
crates/core/src/providers/
├── mod.rs
│   ├── Usage struct                              ── canonical 4-field shape
│   └── Usage::accumulate                         ── per-iteration sum
├── anthropic.rs
│   ├── build_body                                ── 3 cache_control breakpoints
│   ├── parse_sse_event (message_delta)           ── reads cache fields
│   └── tests::                                   ── byte-stability + breakpoint coverage
├── agent_sdk.rs (result frame)                   ── reads Anthropic-shape cache fields
├── openai.rs::parse_openai_usage                 ── HARDCODES cache None (GAP G1)
├── openai_responses.rs                           ── HARDCODES cache None (GAP G2)
├── gemini.rs                                     ── HARDCODES cache None (GAP G3)
└── ollama.rs / ollama_cloud.rs                   ── HARDCODES cache None (no caching)

crates/core/src/usage.rs
├── DailyUsage { cache_write, cache_read, ... }   ── per-day aggregation
├── UsageStore::record                            ── unwrap_or(0) for None providers
└── tests::                                       ── round-trip + accumulation

crates/core/src/repl.rs
└── per-turn pill renders "(N cached)" if Some

frontend/src/components/...
└── ViewEvent::TokenUsage handler                 ── same gating
```

---

## 11. Migration / known limitations

### What works today
- **Anthropic** — full implementation, byte-stability tested, 3 breakpoints used.
- **AgentSdk** — passthrough; Claude Code's cache benefits surface via `result` frame.
- **AzureAIFoundry** — gets Anthropic's cache via the AnthropicProvider routing.
- **Daily usage rollup** — `UsageStore` correctly sums cache fields when reported.
- **CLI/GUI display** — gated on `Option::is_some()`, never lies about absent data.

### What's broken (as of M6.22 — none in cache visibility)
- All of G1-G5 shipped. Server-side caching IS active AND surfaced for OpenAI / OpenAI Responses / Gemini / DeepSeek. Anthropic cache_control retry safety net is in place.
- **Gemini explicit `cachedContents`** — not wired (gap G6, larger scope; needs Provider trait method).

### What's by design
- **Ollama** — no caching surface.
- **OllamaAnthropic / LMStudio** — they don't cache; the AnthropicProvider's cache_control markers are sent but ignored by the server.

### Sprint chronology

| Sprint | Dev-log | What shipped (cache-relevant) |
|---|---|---|
| Phase 5 | (initial) | Basic Usage struct, Anthropic basic parser |
| ~Phase 8 | `~110` | Anthropic cache_control on system prompt |
| ~Phase 9 | `~115` | Anthropic cache_control on tools |
| M6.x | `~125` | Rolling-message breakpoint (3rd cache slot) |
| M6.x | `~127` | UsageStore daily aggregation including cache_write/cache_read |
| AgentSdk | `~130` | Cache fields parsed from result frame |
| M6.21 | `139` | Audit identified gaps G1-G6 |
| M6.22 | `140` | Shipped G1+G2+G3+G4 — OpenAI/Responses/Gemini/DeepSeek cache stats now visible |

### Recommended fix order

1. **G1 + G2** (OpenAI / OpenAI Responses cached_tokens) — shared root cause, single ~10-line patch each. Fixes the most-impacted user surface.
2. **G3** (Gemini cachedContentTokenCount) — single-line addition to the existing parser.
3. **G4** (DeepSeek cache hit/miss) — defensive dual-check in `parse_openai_usage`.
4. **G5** (Anthropic tool_result cache_control) — verify against current API docs; fix if confirmed.
5. **G6** (explicit cachedContents) — larger scope; needs Provider trait extension.
