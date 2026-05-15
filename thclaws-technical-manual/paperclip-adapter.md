# Paperclip adapter

External npm package — [`@thclaws/paperclip-adapter`](https://www.npmjs.com/package/@thclaws/paperclip-adapter) — that lets a [Paperclip](https://paperclip.ai) instance hire a thClaws agent as one of its built-in runtimes (alongside `claude_local`, `codex_local`, `cursor_local`, …). Source lives in the workspace at `paperclip-adapter/` (TypeScript, ~675 LOC across 6 files); not bundled with the desktop binary, not synced to the public `thClaws` mirror.

Shipped in v0.9.5 (v0.1 MVP). The adapter is a thin Node subprocess wrapper around `thclaws -p` (print mode); it does not embed any thClaws Rust code.

## Contract

Paperclip's plugin loader (`paperclip/server/src/adapters/plugin-loader.ts`) imports the package's main entry, calls `createServerAdapter()`, and registers the returned `ServerAdapterModule` in its mutable adapter registry. The contract is published as `@paperclipai/adapter-utils`; thClaws's adapter satisfies the v0.1 shape:

```ts
interface ServerAdapterModule {
  type: string;                    // "thclaws_local"
  execute: (ctx) => Promise<AdapterExecutionResult>;
  testEnvironment: (ctx) => Promise<AdapterEnvironmentTestResult>;
  models: AdapterModel[];          // curated short-list for the UI picker
  agentConfigurationDoc: string;   // markdown blob shown in the agent settings page
}
```

Factory is at `paperclip-adapter/src/server/index.ts:16`; it composes the four field implementations from sibling modules.

### Metadata layer (entry point)

`paperclip-adapter/src/index.ts:17` exports `type = "thclaws_local"` and `label = "thClaws (local)"`. The file is intentionally dependency-free — Paperclip's UI imports it to read `type` / `label` / `models` / `agentConfigurationDoc` without dragging in `node:child_process`, so adapter metadata is visible in browser bundles even before the server-side factory runs.

The `models` array (`paperclip-adapter/src/index.ts:28`) is a six-item curated short-list (Claude Sonnet 4.6, Claude Opus 4.7, Claude Haiku 4.5, Codex gpt-5.4, GPT-4o, Gemini 2.5 Flash). This is what populates Paperclip's UI dropdown — the user-typed `model` field maps verbatim to `thclaws -m <id>`, so anything thClaws's `ProviderKind::detect` recognizes also works at runtime; the short-list is purely cosmetic.

`agentConfigurationDoc` (`paperclip-adapter/src/index.ts:37`) is a single template-literal markdown blob (when-to-use / when-to-skip / field reference / operational notes) that Paperclip renders directly on the agent settings page. It duplicates the user-manual chapter content intentionally — the user-manual is for the thClaws operator, this string is for the Paperclip operator who may never read the thClaws docs.

## Spawn flow (`execute`)

`paperclip-adapter/src/server/execute.ts:65` — the function signature receives a Paperclip-issued `ctx` carrying `config` (the adapter block on the agent), `context` (the job's prompt + Paperclip workspace metadata), and stream callbacks (`onLog`, `onMeta`, `onSpawn`).

Pipeline:

1. **Config parse.** Helper functions `asString` / `asNumber` / `asStringArray` / `asEnvRecord` defensively extract typed values from the untrusted JSON config. Defaults: `command="thclaws"`, `model="claude-sonnet-4-6"`, `cwd=process.cwd()`, `extraArgs=[]`, `timeoutSec=0`, `promptTemplate="{{prompt}}"`. (`paperclip-adapter/src/server/execute.ts:70`)
2. **Prompt template.** Paperclip stuffs the job's user prompt at `ctx.context.prompt`. The optional `promptTemplate` field passes through a minimal `{{key}}` substitution (`renderTemplate()` at `execute.ts:223`) — no escaping, since Paperclip is the trust boundary for prompt content.
3. **Argv assembly.** `args = ["-p", prompt, "-m", model, ...extraArgs]`. Print-mode (`-p`) means single-turn non-interactive; no `--resume`, no stream-json.
4. **Env composition.** `process.env` ∪ `config.env` ∪ any `PAPERCLIP_*` keys from `ctx.context`. Workspace metadata reaches user hooks via the `PAPERCLIP_WORKSPACE_*` convention, matching `claude_local` / `codex_local`.
5. **`onMeta` callback.** Records `{adapterType, command, commandArgs, cwd, env: safeEnv, prompt}` to Paperclip's audit log. `safeEnv` is filtered to `PAPERCLIP_*` keys only (`execute.ts:108`) — provider API keys never reach the audit trail even though they exist in the spawned process's env.
6. **`spawn(command, args, {cwd, env, stdio: ["ignore", "pipe", "pipe"]})`.** stdin is ignored — there's no interactive turn. `child.pid` is reported via `ctx.onSpawn()` so Paperclip can show the process in its run dashboard.
7. **Stream capture.** stdout + stderr both stream to `ctx.onLog(stream, chunk)` *and* accumulate in `stdoutBuf` / `stderrBuf` for the final result. v0.1 surfaces the full stdout as one transcript block (no incremental tool-call rendering); the streaming `onLog` fires per chunk regardless, so terminals that want raw bytes can subscribe.
8. **Timeout.** When `timeoutSec > 0`, a `setTimeout` sends `SIGTERM`, then `SIGKILL` after a 5s grace — matching `claude_local`'s behavior.
9. **Close handler.** Resolves with `AdapterExecutionResult { exitCode, signal, timedOut, errorMessage, errorCode, errorFamily: null, model, summary: stdoutBuf.trim(), resultJson: null }`. `errorFamily` is left null deliberately — the v0.1 adapter-utils contract only accepts `"transient_upstream"` there, and neither timeout nor non-zero exit fits.

## Diagnostic probe (`testEnvironment`)

`paperclip-adapter/src/server/test.ts:29` — the function Paperclip's Settings → Adapter page calls when the user clicks "Test environment". Returns `AdapterEnvironmentTestResult { adapterType, status, checks[], testedAt }` where `status` is the max severity across the checks.

Two checks in v0.1:
1. **`cwd_check`** — `fs.access(cwd, R_OK | X_OK)`; fails with a hint to set `cwd` to an absolute path.
2. **`thclaws_version`** — spawns `<command> --version` with a 5s timeout, captures stdout, fails on non-zero exit / no output / spawn error. Hint points at the install instructions.

Provider auth / model availability is explicitly **not** verified — thClaws does its own keychain / `.env` discovery on each run, and a missing API key surfaces at `execute()` time, not in the diagnostic. This is intentional: the probe needs to be cheap and idempotent, and `thclaws -p "hi" -m <model>` would burn provider quota for a "did you wire it up right?" question.

## Output parsing helpers

`paperclip-adapter/src/server/parse.ts` exports two helpers for callers that want to surface the trailing `[tokens: 1234in/567out · 12.3s]` line separately from the assistant text:

- `extractTokenSummary(stdout)` — returns `{ inputTokens, outputTokens, durationSec }` or `null`.
- `stripTokenSummary(stdout)` — returns the stdout with the summary line stripped from the tail.

The regex (`parse.ts:19`) matches the exact format thClaws's print mode emits today. Neither helper is called by `execute()` itself — the v0.1 surface returns the raw stdout — but they exist so Paperclip's transcript view (or a future caller) can render the token line as metadata rather than body text.

## Limits / non-goals (v0.1 MVP)

- **No multi-turn session continuation.** Each Paperclip run is a fresh `thclaws -p`; no `--resume` between runs. Lifts once thClaws ships `--output-format stream-json` — the flag is declared in the CLI but not wired through `run_print_mode` yet (see `thclaws/crates/core/src/repl.rs:3542`).
- **No incremental tool-call rendering.** stdout buffers per chunk into `onLog` but Paperclip's transcript view shows the final block; no per-tool framing until stream-json lands.
- **No structured `resultJson`.** Returned as `null` until stream-json is available — the typed event stream is what would populate it.
- **No provider-side credential management.** API keys come from env (`config.env` per-agent injection or the host shell), `.env` files, or the OS keychain — whatever thClaws's normal `secrets::resolved_backend()` lookup chain finds. The adapter never reads or writes credentials.
- **Single binary handle.** The `command` field is a string, not a list — paths with shell metacharacters won't work (use a wrapper script if you need them).

## Source layout

```
paperclip-adapter/
├── README.md
├── package.json              # @thclaws/paperclip-adapter
├── src/
│   ├── index.ts              # type/label/models/agentConfigurationDoc + createServerAdapter re-export
│   ├── ui-parser.ts          # optional client-side token-summary parser (mirror of server/parse)
│   └── server/
│       ├── index.ts          # createServerAdapter() factory
│       ├── execute.ts        # spawn flow
│       ├── test.ts           # testEnvironment probe
│       └── parse.ts          # extractTokenSummary / stripTokenSummary
```

Build is plain `tsc` to `dist/`; no bundler. Published to npm under the `@thclaws` org. Workspace-only — the public mirror at `~/__2026/thClaws` does not contain `paperclip-adapter/` (it lives behind `make sync-public`'s exclude list because the npm package is its own release surface).

## See also

- User manual: [chapter 22](../user-manual/ch22-paperclip-adapter.md) (operator-facing how-to)
- [`running-modes.md`](running-modes.md) — explains print mode (`-p`), which is the surface this adapter wraps
- [`sso.md`](sso.md) — how thClaws's credential lookup works (the chain `execute()` defers to)
