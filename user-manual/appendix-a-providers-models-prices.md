# Appendix A — Providers, models & prices

This appendix is the **thClaws.cloud gateway** catalogue — the models you can call when
you point an agent at `https://thclaws.cloud/gateway` (or run [thClaws.cloud](ch27-thclaws-cloud.md)
self-hosted). Desktop / CLI builds have their own catalogue covering 22 providers; see
[Chapter 6 — Providers, models & API keys](ch06-providers-models-api-keys.md) for that
list.

> Catalogue refreshed **2026-06-02**. To re-pull the latest models + rates from upstream
> APIs and [LiteLLM's pricing feed](https://github.com/BerriAI/litellm),
> run `python3 scripts/refresh-model-catalogue.py` from the repo root.

## Pricing model

Rates below are **what you pay** — that is, upstream cost × platform markup. Internally
the gateway keeps two pieces:

| Layer | What | Where |
|---|---|---|
| **DB row rate** | Raw upstream cost from LiteLLM (USD per token) | `model_pricing` table, seeded by `scripts/refresh-model-catalogue.py` |
| **Platform markup** | `1.25×` multiplier applied at meter time | `THCLAWS_PLATFORM_MARKUP` env var on the gateway service |

This means: if Anthropic raises Opus 4.8 to $6/M, you only update LiteLLM (or wait for
the next sync) and the gateway picks up the new rate on the next pricing refresh — the
markup stays a single env-var knob.

All prices are in **US dollars per 1,000,000 tokens** (1M tokens). The DB stores these
as microcents per 1k tokens (`µ¢/kt`); the formula is `$/M = µ¢/kt / 100,000`.

## Anthropic

| Model | Tier | Input ($/M) | Output ($/M) | Remark |
|---|---|---:|---:|---|
| `claude-haiku-4-5-20251001` | starter | $1.25 | $6.25 | Latest Haiku (dated alias of `claude-haiku-4-5`) |
| `claude-opus-4-8` | enterprise | $6.25 | $31.25 | Current flagship Opus |
| `claude-opus-4-7` | enterprise | $6.25 | $31.25 | Previous flagship |
| `claude-opus-4-6` | enterprise | $6.25 | $31.25 | |
| `claude-opus-4-5-20251101` | enterprise | $6.25 | $31.25 | |
| `claude-opus-4-1-20250805` | enterprise | $18.75 | $93.75 | Legacy Opus 4.1 — Anthropic still serves it but for new work prefer Opus 4.8 |
| `claude-opus-4-20250514` | enterprise | $18.75 | $93.75 | Legacy Opus 4.0 |
| `claude-sonnet-4-6` | pro | $3.75 | $18.75 | Current default model |
| `claude-sonnet-4-5-20250929` | pro | $3.75 | $18.75 | |
| `claude-sonnet-4-20250514` | pro | $3.75 | $18.75 | Legacy Sonnet 4.0 |

> **Note** — the `tier` column is **display-only**. thClaws.cloud dropped the
> tier-gating ladder in v0.28 — any user with positive credit can call any active model;
> per-call price differential is the only gate. See ch27 § "Why no tier gate" for the
> rationale.

## OpenAI

| Model | Tier | Input ($/M) | Output ($/M) | Remark |
|---|---|---:|---:|---|
| `gpt-4o` | pro | $3.125 | $12.50 | |
| `gpt-4o-mini` | starter | $0.1875 | $0.75 | Cheapest chat-capable OpenAI model on the gateway |
| `o1` | enterprise | $18.75 | $75.00 | Reasoning model — output includes hidden reasoning tokens |

OpenAI exposes ~76 chat-capable models via `/v1/models` (every gpt-5.x, o3, o4 variant,
plus dated snapshots). Only the three above are currently seeded into the gateway. If
you need a specific model, run the refresh script with `--providers openai --apply`
to add it; it will pull pricing from LiteLLM automatically. The full list available
upstream:

- `gpt-5.5`, `gpt-5.5-pro`, `gpt-5.4`, `gpt-5.4-pro`, `gpt-5.4-mini`, `gpt-5.4-nano`,
  `gpt-5.3-codex`, `gpt-5.3-chat-latest`
- `gpt-5.2`, `gpt-5.2-pro`, `gpt-5.2-codex`, `gpt-5.2-chat-latest`
- `gpt-5.1`, `gpt-5.1-codex`, `gpt-5.1-codex-max`, `gpt-5.1-codex-mini`
- `gpt-5`, `gpt-5-pro`, `gpt-5-codex`, `gpt-5-mini`, `gpt-5-nano`, `gpt-5-search-api`
- `gpt-4.1`, `gpt-4.1-mini`, `gpt-4.1-nano`
- `o3`, `o3-pro`, `o3-mini`, `o3-deep-research`, `o4-mini`, `o4-mini-deep-research`,
  `o1-pro`
- Legacy: `gpt-4`, `gpt-4-turbo`, `gpt-3.5-turbo*`

## Google (Gemini)

| Model | Tier | Input ($/M) | Output ($/M) | Remark |
|---|---|---:|---:|---|
| `gemini-2.0-flash` | starter | $0.125 | $0.50 | Cheapest model on the gateway across all providers |
| `gemini-2.0-pro` | pro | $1.95 | $7.81 | ⚠ See note below — **not in LiteLLM**, rate is from initial seed and may be stale |

Google exposes 20+ Gemini models via `/v1beta/models` (gemini-2.5-flash, gemini-2.5-pro,
gemini-3-pro-preview, gemini-3.1-flash-lite, etc.). Run the refresh script to seed them
with current pricing.

## OpenRouter

| Model | Tier | Input ($/M) | Output ($/M) | Remark |
|---|---|---:|---:|---|
| `openrouter/auto` | starter | (pass-through) | (pass-through) | OpenRouter's auto-router; actual cost determined by the routed model. The gateway forwards usage as-is. |

The DB row for `openrouter/auto` holds `0/0` because the upstream cost varies per request.
We rely on OpenRouter's own metering in this case.

## Inactive / deprecated rows

| Model | Reason |
|---|---|
| `anthropic/claude-haiku-4-5` | Replaced by the dated alias `claude-haiku-4-5-20251001` — Anthropic's `/v1/models` no longer returns the bare alias. Kept in the table with `active=FALSE` so historical usage rows still join. |

## Remarks on accuracy

| Row | Status |
|---|---|
| 14 of 16 active rows | **Exact match** with LiteLLM's published pricing as of 2026-06-02 |
| `google/gemini-2.0-pro` | **Not in LiteLLM** — its current rate is from the initial migration 006 seed. Google may have changed it since. The refresh script can't reprice this row automatically; review periodically against [ai.google.dev/pricing](https://ai.google.dev/pricing). |
| `openrouter/auto` | Pass-through, no canonical cost |

If you spot a price drift, run:

```bash
python3 scripts/refresh-model-catalogue.py --reprice-only            # dry-run
python3 scripts/refresh-model-catalogue.py --reprice-only --apply    # commit
```

## Adjusting the platform markup

The 1.25× multiplier is a single env var on the gateway service:

```yaml
# thclaws-cloud/docker-compose.yml
gateway:
  environment:
    THCLAWS_PLATFORM_MARKUP: ${THCLAWS_PLATFORM_MARKUP:-1.25}
```

Changing it to `1.50` and bouncing the gateway is the fastest way to widen margin
uniformly across every model. The gateway clamps any value below `1.0` (running at
exact upstream cost is the minimum — anything lower would mean losing money per call)
and emits a warning.

For per-model differential pricing, edit the `model_pricing` row directly — but note
that the refresh script will reprice it back to LiteLLM on the next `--reprice` run.
Mark such rows by setting `active=FALSE` then `active=TRUE` with a custom rate, and
exclude them from refresh with `--providers` scoping.

## Where the rates live in code

| Concern | File |
|---|---|
| Cost computation | `thclaws-cloud/gateway/src/pricing.rs` (`cost_cents_with_markup`) |
| Markup env-var parsing | `thclaws-cloud/gateway/src/config.rs` (`platform_markup`) |
| Meter call-site | `thclaws-cloud/gateway/src/meter.rs` |
| Pricing table schema | `thclaws-cloud/api/alembic/versions/006_*.py` |
| Refresh script | `scripts/refresh-model-catalogue.py` |
