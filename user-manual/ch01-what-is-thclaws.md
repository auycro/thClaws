# Chapter 1 — What is thClaws?

![logo](../user-manual-img/logo/thClaws-logo-line-art-banner.png)

thClaws is a **native-Rust AI agent workspace** that runs locally on
your machine. Not just coding — it edits code, automates workflows,
searches your knowledge bases, and coordinates teams of agents, all in
one binary. You tell it what you want in natural language; it reads
your files, runs commands, uses tools, and talks back to you while it
works.

Four surfaces ship as one binary, sharing a single `Agent` loop, `Session`, and tool registry:

- **Desktop GUI** (`thclaws` with no flags) — a native window with a
  Terminal tab running the same interactive REPL as `--cli` mode, a
  streaming Chat tab, a Files browser, and an optional Team tab.
- **CLI REPL** (`thclaws --cli`) — an interactive terminal prompt for
  SSH sessions, headless servers, or when you want zero GUI overhead.
- **Non-interactive mode** (`thclaws -p "prompt"`, long form `--print`)
  — runs a single turn and exits. Handy for scripts, CI pipelines, and
  shell one-liners. Add `-v` / `--verbose` to see per-turn token usage
  on stderr without polluting stdout.
- **Webapp** (`thclaws --serve --port 7878`) — same engine over
  WebSocket/HTTP, served from your laptop. Reach it remotely via SSH
  tunnel for "thClaws anywhere" without opening a port.

## What makes it different

- **Multi-provider.** Anthropic (native + Claude Agent SDK via Claude
  Code auth), OpenAI (Chat Completions + Responses/Codex), Google
  Gemini & Gemma, Alibaba DashScope (Qwen), DeepSeek, Z.ai (GLM Coding
  Plan), NVIDIA NIM, NSTDA Thai LLM (OpenThaiGPT, Typhoon, Pathumma,
  THaLLE), OpenRouter, Agentic Press, Azure AI Foundry, Ollama (local,
  local Anthropic-compatible, and Ollama Cloud), LMStudio, plus a
  generic **OpenAI-compatible** slot (`oai/*`) for LiteLLM / Portkey /
  Helicone / vLLM / internal proxies — auto-detected by model name
  prefix. Switch models mid-session with `/model` (validated against
  the provider's catalogue) or swap the whole provider with `/provider`.
- **Any knowledge worker, not just engineers.** The Chat tab is a
  streaming conversation panel anyone can drive — researchers,
  analysts, PMs, ops, legal, marketing, finance. Ask in natural
  language; the agent reads your files, edits documents, searches
  your knowledge base, drafts outputs. Engineers prefer the Terminal
  tab's REPL. Both share the same sessions and config, so a mixed
  team can switch between interfaces freely without losing context.
- **File viewer & editor in the Files tab.** A working-directory file
  tree with a syntax-highlighted preview pane (CodeMirror 6, ~40
  languages) and server-rendered GFM markdown in a sandboxed iframe.
  Click the pencil icon to edit `.md` in a WYSIWYG editor (TipTap) or
  code in a highlighted editor (CodeMirror) — Cmd/Ctrl+S to save,
  native OS confirm dialog before discarding edits. Auto-refresh
  polling pauses while you're editing so concurrent `Write`/`Edit`
  tool calls from the agent can't clobber your in-progress buffer.
- **Open standards, not a walled garden.** thClaws is built on the
  conventions the agent-tooling industry is converging on, not on
  bespoke formats you have to learn only for us. The
  [Model Context Protocol](https://modelcontextprotocol.io/) for
  tool servers. [`AGENTS.md`](https://agents.md) for project
  instructions — the vendor-neutral standard stewarded by the Agentic
  AI Foundation and adopted by Google, OpenAI, Factory, Sourcegraph,
  and Cursor. `SKILL.md` with YAML frontmatter for packaged workflows.
  `.mcp.json` for MCP server configuration. Your configuration is
  portable — between thClaws, other agents that speak the same
  standards, and whatever comes next.
- **Skills.** Reusable expert workflows packaged as a directory with
  `SKILL.md` (YAML frontmatter + Markdown instructions the model
  follows) and optional scripts. The agent picks the right skill
  automatically when a user request matches the `whenToUse` trigger,
  or you can invoke one explicitly as `/<skill-name>`. Install with
  `/skill install` from a git URL or `.zip` archive. Discovery looks
  in `.thclaws/skills/`, `~/.config/thclaws/skills/`, plus
  `.claude/skills/` as a fallback location.
- **MCP servers.** The Model Context Protocol lets you plug in tools
  built by third parties — GitHub, filesystems, databases, browsers,
  Slack, and more. Both stdio (spawned subprocess) and HTTP Streamable
  transports are supported, with OAuth 2.1 + PKCE for protected
  servers. Add one with `/mcp add` or ship a `.mcp.json` in your
  project; discovered tools are namespaced by server name and the
  agent can call them like any built-in.
- **Plugin system.** Skills + commands + agent definitions + MCP
  servers bundled under a single manifest (`.thclaws-plugin/plugin.json`
  or `.claude-plugin/plugin.json`), installable from a git URL or a
  `.zip` archive. One install, one uninstall, one version to pin —
  ideal for sharing a team's extensions.
- **Memory & project instructions.** Drop an `AGENTS.md` (or
  `CLAUDE.md`) in your repo — thClaws walks up from cwd and injects
  every match into the system prompt, the same way git resolves
  `.gitignore`. A
  separate persistent memory store at `~/.config/thclaws/memory/`
  holds longer-lived facts the agent has learned about you, your
  preferences, and each project, classified as `user` / `feedback` /
  `project` / `reference` and indexed as markdown files you can read,
  edit, or commit. Both survive restart.
- **Knowledge bases (KMS).** Per-project and per-user wikis the agent
  can search and read on demand. Drop markdown pages under
  `.thclaws/kms/<name>/pages/`, give each a one-line entry in
  `index.md`, tick the box in the sidebar, and the agent gets a
  table of contents every turn plus a full mutation surface
  (`KmsRead` / `KmsSearch` / `KmsWrite` / `KmsAppend` / `KmsDelete`)
  for active wiki maintenance. No embeddings — grep + read, following
  Andrej Karpathy's LLM-wiki pattern. Run `/dream` and a built-in
  side-channel agent mines the 10 most recent sessions, dedupes pages,
  surfaces new insights, and writes a dated audit-trail page — review
  with `git diff`.
- **Three tiers of agent orchestration.**
  - **`Task` tool** — model-driven subagents that block the parent's
    turn. Each gets its own tool registry, recurses up to 3 levels
    deep. Right when the parent's reasoning should decide whether and
    when to delegate.
  - **`/agent <name> <prompt>`** — user-driven concurrent
    side-channels. Spawned on a fresh tokio task, runs in parallel with
    main, never enters main's history, has its own cancel token. Right
    when *you* know exactly what you want a specialist to do
    (`/agent translator แปลไฟล์ x` while you keep coding).
  - **Agent Teams** — multiple thClaws processes coordinating through
    a shared mailbox and task queue, each teammate in its own tmux
    pane and optional git worktree. One agent writes your backend
    while a teammate builds the frontend in parallel; lead calls
    `TeamMerge` when both are done.
- **Plan mode.** For multi-step work, the agent can `EnterPlanMode`,
  propose an ordered list of steps, and let *you* review and approve
  before execution. Each step runs sequentially with its own retry
  budget; failures stop the chain so you can decide. Same UX in GUI
  (sidebar with Approve / Cancel / Skip / Retry per step) and REPL
  (`/plan` slash command).
- **Schedule recurring jobs.** `/schedule add` runs an agent on cron
  (`0 9 * * MON-FRI`), at fixed intervals, or whenever a watched
  directory changes (`watchWorkspace`). Three composable layers:
  manual `/schedule run`, in-process scheduler (lives as long as your
  REPL), and a native daemon (`launchd` on macOS / `systemd-user` on
  Linux) that survives reboots. Per-job working directory, optional
  model override, full output capture.
- **Long-running loops & overnight builds.** `/loop` for
  fixed-interval iteration. `/goal` for audit-driven completion (the
  agent works toward a goal until an audit prompt confirms "done" or
  hits the budget). Compose them: `/goal --auto` is a Ralph-style
  overnight builder that keeps going until the goal is satisfied or
  you wake up.
- **Document workflow.** Native PDF, DOCX, PPTX, XLSX read + edit +
  create tools, plus image rendering. The agent can ingest a 50-page
  PDF, summarize it into KMS, and produce a follow-up PowerPoint deck
  — all in one conversation, no separate file-conversion step.
- **Hooks.** Run shell scripts on agent lifecycle events:
  `pre_tool_use`, `post_tool_use`, `permission_denied`, `session_start`,
  `pre_compact`, etc. Audit every Bash invocation, gate `Edit`/`Write`
  through your linter, fire a Slack notification when long sessions
  end. Eight events × per-event environment variables × timeout-with-
  SIGKILL guarantees.
- **Settings.** Every runtime knob — permission mode, thinking budget,
  allowed/disallowed tools, provider endpoints, KMS attachments,
  max output tokens — is one JSON file: `.thclaws/settings.json`
  (project, commit it with the repo) or
  `~/.config/thclaws/settings.json` (user-global).
  `~/.claude/settings.json` is read as a fallback location. API
  keys go in the OS keychain by default (Windows Credential Manager
  / macOS Keychain / Linux Secret Service) with `.env` fallback for
  CI and headless servers. The gear icon in the desktop GUI is a
  visual editor for keys, global/folder `AGENTS.md`, and the secrets
  backend choice.
- **Session resume.** `thclaws --resume last` picks up where you left
  off; `thclaws --resume <id>` jumps to a specific session. Sessions
  live as JSONL under `.thclaws/sessions/` — git-friendly,
  grep-friendly, never opaque.
- **Safety first.** A filesystem sandbox scopes file tools to the
  working directory. Destructive shell commands are flagged before
  execution. You approve every mutating tool call unless you've opted
  into auto-approve. Permission requests label which agent is asking
  when multiple are running concurrently (main vs. side-channel vs.
  subagent), so you don't approve a translator's `Bash` thinking it's
  main's.
- **Offline-capable.** Ollama (native and Anthropic-compat) lets you run
  entirely against a local model.
- **Deploy what you build.** thClaws doesn't stop at authoring —
  ship the landing pages, web apps, APIs, and AI agents you create
  through [Agentic Press Hosting](https://agentic-press.com)
  (partnered with SIS Cloud Service and Artech.Cloud) or any other
  host you prefer. Schedule agents on cron, respond to webhooks,
  stream from public URLs. The deploy flow ships as a plugin
  (`/plugin install …-deploy`) so hosts are swappable — the client
  never locks you in.
- **Shell escape.** Prefix any REPL line with `!` to run the rest as a
  shell command directly in your terminal — no tokens, no approval
  prompt, no agent round-trip (e.g. `! git status`).

## What you need

- A supported OS: macOS (arm64 or x86_64), Linux (arm64 or x86_64), or
  Windows (arm64 or x86_64).
- At least one LLM API key — Anthropic, OpenAI, Gemini, OpenRouter,
  Agentic Press, DashScope, DeepSeek, Z.ai, NVIDIA NIM, NSTDA Thai LLM,
  or Azure AI Foundry. (Or a local Ollama / LMStudio install if you'd
  rather stay offline.)

[Chapter 2](ch02-installation.md) walks through installation and first
launch. [Chapter 6](ch06-providers-models-api-keys.md) covers where
and how to paste keys.

## How this manual is organised

**Part I** (chapters 2–19) is reference material: how to install it,
and then every user-facing feature explained once with the commands
and configuration you need — sessions, memory, KMS, tools, skills,
hooks, MCP, subagents, plugins, teams, plan mode, and scheduling.

**Part II** — walkthroughs of real projects from scratch (static
landing page, reservation site, news-aggregation agent, ending with
deploys to Agentic Press Hosting) — is planned for a future revision.

If you're new, read chapter 2 next. If you're migrating from Claude
Code, skip to chapters 6, 7, 11, and 13. If you're already familiar
with the basics and want what's new, the recent additions live in
chapter 9 (`/dream`), chapter 15 (`/agent` side-channels), chapter 18
(plan mode), and chapter 19 (scheduling).
