# Contributors

thClaws is built primarily by [@mozeal](https://github.com/mozeal). The
contributors below have shipped real fixes and features — credit where
it's due.

## Community contributors

In rough order of first contribution.

### [@bombman](https://github.com/bombman) (Nuttapong Maneenate)
- **PR [#2](https://github.com/thClaws/thClaws/pull/2)** — Enable
  `cargo build` from the repository root via a workspace
  `Cargo.toml`. First contribution that made the build story sane
  for anyone landing on the repo cold.

### [@parintorns](https://github.com/parintorns) (Parintorn Sukhowatanakit)
- **PR [#3](https://github.com/thClaws/thClaws/pull/3)** — Bumped
  the documented minimum Rust version from 1.78 to 1.85 after the
  codebase started leaning on newer features.
- **PR [#4](https://github.com/thClaws/thClaws/pull/4)** — Removed
  a duplicate `sessions_list` branch in the Sidebar.
- **PR [#6](https://github.com/thClaws/thClaws/pull/6)** —
  Tightened `.gitignore` to cover the entire `.thclaws/` directory.
- **PR [#7](https://github.com/thClaws/thClaws/pull/7)** — Resolved
  React lint errors in `TerminalView` and `App`.
- **PR [#8](https://github.com/thClaws/thClaws/pull/8)** — Improved
  type safety and resolved lint errors across the GUI.
- **PR [#9](https://github.com/thClaws/thClaws/pull/9)** —
  Refactored: split `ThemeProvider` out of the `useTheme` hook.
- **PR [#10](https://github.com/thClaws/thClaws/pull/10)** —
  Resolved `react-hooks/exhaustive-deps` warnings.
- **PR [#37](https://github.com/thClaws/thClaws/pull/37)** —
  Prevented Tab focus escape and force-closed the slash-popup on
  accept.
- **PR [#38](https://github.com/thClaws/thClaws/pull/38)** —
  Returned focus to the active tab after modal close and tab
  switch.

### [@gokusenz](https://github.com/gokusenz) (Nattawut Ruangvivattanaroj)
- **PR [#11](https://github.com/thClaws/thClaws/pull/11)** —
  Documented the thClaws frontend.
- **PR [#12](https://github.com/thClaws/thClaws/pull/12)** — Added
  chat-message copy buttons.
- **PR [#27](https://github.com/thClaws/thClaws/pull/27)** — Use
  Gemini 2.5 Flash as the default Gemini model.

### [@Kinzen-dev](https://github.com/Kinzen-dev)
- **PR [#16](https://github.com/thClaws/thClaws/pull/16)** —
  Handle GUI ask-prompts and macOS Cmd+W close shortcuts so
  dialogs and windows behave consistently.

### [@triok-t](https://github.com/triok-t)
- **PR [#16](https://github.com/thClaws/thClaws/pull/16)
  (co-author)** — Co-authored the GUI ask-prompts + macOS Cmd+W
  close-shortcut fix with @Kinzen-dev.

### [@siharat-th](https://github.com/siharat-th) (Siharat Thammaya)
- **PR [#20](https://github.com/thClaws/thClaws/pull/20)** — Added
  the slash-command popup to chat + terminal tabs — auto-complete
  as you type `/...`. Quality-of-life win felt on every session.
- **PR [#22](https://github.com/thClaws/thClaws/pull/22)** —
  Terminal arrow-key + home/end caret movement support; fixed the
  cursor-trap on long lines.
- **PR [#94](https://github.com/thClaws/thClaws/pull/94)** —
  Allow trusted MCP-Apps servers to opt into `allow-same-origin`
  for iframe-based extensions.
- **PR [#101](https://github.com/thClaws/thClaws/pull/101)** —
  Loopback `/v1` bridge + auto-size for MCP-Apps iframes.
- **PR [#102](https://github.com/thClaws/thClaws/pull/102)** —
  `THCLAWS_LOOPBACK_BIND` env so Docker / WSL containers can reach
  the loopback bridge.
- **PR [#107](https://github.com/thClaws/thClaws/pull/107)** —
  Keep the MCP-App iframe alive across Fullscreen / PIP / Back-
  to-chat transitions so state isn't dropped.
- **PR [#112](https://github.com/thClaws/thClaws/pull/112)** —
  Pop the partial assistant message on `max_tokens` escalation
  retry so it doesn't fork the conversation.
- **PR [#132](https://github.com/thClaws/thClaws/pull/132)** —
  Embedded a self-contained `/quiz` in thClaws and dropped the
  gamedev-MCP dependency.

### [@Parinya-chab](https://github.com/Parinya-chab) (joparin)
- **PR [#21](https://github.com/thClaws/thClaws/pull/21)** — Added
  the Azure AI Foundry provider for Claude, routing through
  Foundry's Anthropic-compatible surface.

### [@Av0cadoo](https://github.com/Av0cadoo) (Muninthorn Thongnuch)
- **PR [#28](https://github.com/thClaws/thClaws/pull/28)** —
  Implemented the Ollama Cloud provider (re-scope from an earlier
  Ollama-only effort).

### [@chawasit](https://github.com/chawasit) (Chawasit Tengtrairatana)
- **PR [#33](https://github.com/thClaws/thClaws/pull/33)** —
  Added a third Anthropic cache breakpoint on rolling conversation
  history; long turns keep more of their state cached.
- **PR [#34](https://github.com/thClaws/thClaws/pull/34)** — Added
  `scripts/build.{sh,ps1}` build helpers.

### [@SalmonRK](https://github.com/SalmonRK)
- **PR [#35](https://github.com/thClaws/thClaws/pull/35)** — Added
  `ProviderKind::OpenAICompat` for generic OpenAI-compatible
  endpoints (LiteLLM, Portkey, Helicone, vLLM, internal proxies).

### [@m4rshallz](https://github.com/m4rshallz) (rsnz)
- **PR [#43](https://github.com/thClaws/thClaws/pull/43)** —
  Implicit-thinking fix + Thai font fallbacks. Repaired the
  silent-thinking flow on providers that don't echo thinking
  events, and added Thai-script font fallbacks for the GUI.

### [@NuttapongPun](https://github.com/NuttapongPun) (Nuttapong Pungasem)
- **PR [#44](https://github.com/thClaws/thClaws/pull/44)** — CLI
  slash-command Tab completion + ghost-text hints.
- **PR [#73](https://github.com/thClaws/thClaws/pull/73)** —
  Synced the README's provider list with the current
  `ProviderKind` registry.

### [@sunchiro](https://github.com/sunchiro) (Phruetthiphong)
- **PR [#55](https://github.com/thClaws/thClaws/pull/55)** —
  Windows fixes for CLI lifecycle issues.
- **PR [#77](https://github.com/thClaws/thClaws/pull/77)** —
  Surface the missing confirmation dialog on the web-browser
  surface.

### [@vjumpkung](https://github.com/vjumpkung) (Chanrich Pisitjing)
- **PR [#60](https://github.com/thClaws/thClaws/pull/60)** —
  Windows: correct CLI readline behavior and console lifecycle.

### [@Tanabat-Hamtaro](https://github.com/Tanabat-Hamtaro)
- **PR [#61](https://github.com/thClaws/thClaws/pull/61)** —
  Escape `\u2028`, `\u2029`, and `\0` in `escape_for_js` so
  JSON-in-JS injections survive line-separator and null bytes.

### [@supakornkim](https://github.com/supakornkim) (Supakorn Kimhajan)
- **PR [#62](https://github.com/thClaws/thClaws/pull/62)** — Fixed
  a broken download link in the installation instructions.

### [@mansuang](https://github.com/mansuang) (Mansuang Pawong)
- **PR [#64](https://github.com/thClaws/thClaws/pull/64)** —
  Synced `Cargo.lock` to `thclaws-core 0.7.7`.

### [@wiztechth](https://github.com/wiztechth)
- **PR [#67](https://github.com/thClaws/thClaws/pull/67)** — Added
  direct NVIDIA NIM provider support.
- **PR [#93](https://github.com/thClaws/thClaws/pull/93)** — Fixed
  userid handling when running in a container.
- **PR [#147](https://github.com/thClaws/thClaws/pull/147)** —
  Fixed schedule-timezone handling.

### [@wingyplus](https://github.com/wingyplus) (Thanabodee Charoenpiriyakij)
- **PR [#75](https://github.com/thClaws/thClaws/pull/75)** — Show
  the real MCP tool counts in the MCP Servers sidebar panel (was
  hardcoded to 0).

### [@nazt](https://github.com/nazt) (Nat)
- **PR [#88](https://github.com/thClaws/thClaws/pull/88)** — Added
  the ChatGPT-subscription Codex provider — backbone of the later
  `chatgpt-codex` integration.

### [@baslenvm](https://github.com/baslenvm)
- **PR [#103](https://github.com/thClaws/thClaws/pull/103)** —
  OpenCodeGo: deduct cached tokens from `input_tokens` so cost
  accounting matches the underlying billing.

### [@dome](https://github.com/dome) (Dome C.)
- **PR [#110](https://github.com/thClaws/thClaws/pull/110)
  (closed → adopted into [#113](https://github.com/thClaws/thClaws/pull/113))** —
  Make `--model` flag actually work across CLI, GUI, and serve
  modes. The original PR was closed when @mozeal opened the
  follow-up #113 incorporating the fix; the merge commit
  ([a697cc2](https://github.com/thClaws/thClaws/commit/a697cc2))
  lists @dome as co-author so credit lands where the work was
  done.

### [@ultramcu](https://github.com/ultramcu) (MaIII Themd)
- **PR [#115](https://github.com/thClaws/thClaws/pull/115)** —
  Surface prompt and cache token usage from `message_start` in the
  Anthropic stream.
- **PR [#120](https://github.com/thClaws/thClaws/pull/120)** —
  Throttle LINE reconnect after a clean WebSocket close.
- **PR [#121](https://github.com/thClaws/thClaws/pull/121)** —
  Reject an empty `old_string` in the Edit tool to prevent silent
  no-ops.

### [@modtanoii](https://github.com/modtanoii) (Chaiwat Chanavirat)
- **PR [#135](https://github.com/thClaws/thClaws/pull/135)** — Helm
  chart for self-hosted Kubernetes deployment. First-class k8s
  install path beyond docker-compose.
- **PR [#137](https://github.com/thClaws/thClaws/pull/137)** — Review
  follow-ups on the Helm chart.
- **PR [#140](https://github.com/thClaws/thClaws/pull/140)** — Bump
  the MiniMax default to MiniMax-M3 to match the current api.minimax.io
  flagship.

### [@gobikom](https://github.com/gobikom)
- **PR [#139](https://github.com/thClaws/thClaws/pull/139)** —
  Cap spinner line width to terminal columns. Stopped the REPL
  spinner from wrapping on narrow terminals.

### [@sc28249782](https://github.com/sc28249782) (Somchai Pongkasem)
- **Issue [#141](https://github.com/thClaws/thClaws/issues/141)** —
  Reported, root-caused, AND tested a fix for the
  `split_shell_segments` UTF-8 panic. The fix proposal landed
  essentially as-is in v0.30.0. Exactly the kind of bug report that
  closes itself.

### [@minkbear](https://github.com/minkbear) (WJ)
- **PR [#146](https://github.com/thClaws/thClaws/pull/146)** —
  Backfilled changelog sections for v0.21.0 → v0.33.0 that had
  been added without changelog entries during a fast-iteration
  stretch.

### [@JonusNattapong](https://github.com/JonusNattapong) (Dek1milliontoken)
- **PR [#153](https://github.com/thClaws/thClaws/pull/153)** — Reset
  `cursorPos` on terminal line-clear events. Two paths cleared
  `lineBuffer` without resetting the cursor (slash-popup Escape +
  engine `terminal_clear`), so the caret drifted off the buffer.
  Clean 2-line fix matching the existing Ctrl+C handler pattern.
  Shipped in v0.42.0.
- **PR [#157](https://github.com/thClaws/thClaws/pull/157)** —
  Escape `</` in injected values to prevent HTML script breakout in
  the gui-shell bridge. A malicious shell manifest could plant
  `</script>` in its `id` and break out of the injected
  `<script>` tag; the canonical `<\/` replacement neutralises it.
  Defence-in-depth on hosted-cloud / `--serve` surfaces and any
  future gui-shell marketplace.

### [@pok29dev](https://github.com/pok29dev)
- **Issue [#156](https://github.com/thClaws/thClaws/issues/156)** —
  Reported the DashScope picker double-prefix bug
  (`dashscope/dashscope/<model>`) and pinned the expected canonical
  shape (`dashscope/<model>`). That pointer drove the fix in v0.44.0:
  DashScope routing now follows the same `dashscope/` prefix pattern
  as `zai/` / `qc/` / `ap/` — strip on the wire, keep canonical in
  the catalogue + picker.

## How to be listed

1. **Open a PR** — even small ones (typo fix, documentation,
   regression test). Every merged PR earns a line.
2. **File a high-quality bug report** with a tested fix — like #141.
   We'll usually apply the fix and credit you in the next release.
3. **Contribute to the user manual** — translations, new chapters,
   diagrams.

Be courteous in discussions, follow the existing style of the file
you're editing, and keep PRs small + scoped. CI must be green
before merge.

If you've shipped a fix and aren't on this list, open an issue
titled "add me to CONTRIBUTORS" — we'll add you.
