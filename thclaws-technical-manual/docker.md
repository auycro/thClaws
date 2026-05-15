# Docker

Container packaging for `thclaws --serve`. The deploy unit is one
container per project: bind-mount the project at `/workspace`, expose
port 8443, run.

Source: `Dockerfile` + `docker-compose.yml` + `.dockerignore` at the
repo root. Image published to Docker Hub as
[`thclaws/thclaws`](https://hub.docker.com/r/thclaws/thclaws). Issue
[#92](https://github.com/thClaws/thClaws/issues/92) is the tracking
ticket.

## Why a single image, --serve only

The container ships **one mode**: `thclaws --serve --bind 0.0.0.0
--port 8443`. No `--cli` REPL container, no `-p` / print container —
those are easier to invoke as `docker run --rm -it … thclaws -p
"…"` ad-hoc with the same image, but the long-running shape (and the
one that maps cleanly to "deploy thClaws on a server") is `--serve`.

The implication: `--serve` is gated behind the `gui` Cargo feature
(see [`serve-mode.md`](serve-mode.md) and `bin/app.rs:308` for the
`#[cfg(feature = "gui")]` block). The binary inside the image is
therefore built with `--features gui`, which pulls in
`tao` / `wry` / `comrak` / `rfd` / `native-dialog`. The container
never opens a window — `server::run` never reaches into `gui::run` —
but the binary is **dynamically linked** to GTK + WebKit2GTK at load
time and won't start without those libs present. That's why the
runtime image carries `libgtk-3-0` + `libwebkit2gtk-4.1-0`. A future
refactor that splits `crate::server` out of the `gui` feature gate
will let us drop those and shrink the image significantly. Tracking
in the deferred work list — not blocking v1.

## Three-stage build

```
┌──────────────────┐    ┌────────────────────┐    ┌────────────────────┐
│ frontend         │ →  │ builder            │ →  │ runtime            │
│ node:22-bookworm │    │ rust:1-bookworm    │    │ debian:bookworm-   │
│ pnpm install     │    │ apt-get gtk+wk2gtk │    │  slim              │
│ vite build       │    │ cargo build        │    │ libgtk-3-0         │
│                  │    │  --features gui    │    │ libwebkit2gtk-4.1  │
│ → dist/          │    │  --bin thclaws     │    │ + binary           │
└──────────────────┘    └────────────────────┘    └────────────────────┘
```

**Stage 1 — `frontend`** (`node:22-bookworm-slim`).

Mirrors the host build chain: pnpm (via corepack), `pnpm install
--frozen-lockfile`, `pnpm run build`. Output is `frontend/dist/`,
copied into the next stage. Pinned to Node 22 because the workspace
`.nvmrc` is 22; CI uses Node 20 but both produce identical bundles
for our purposes. Splitting the frontend into its own stage means
`pnpm install` only re-runs when `frontend/pnpm-lock.yaml` changes,
not on every Rust edit.

**Stage 2 — `builder`** (`rust:1-bookworm`).

Installs the GTK + WebKit2GTK *dev* packages (`libgtk-3-dev`,
`libwebkit2gtk-4.1-dev`, `libsoup-3.0-dev`,
`libjavascriptcoregtk-4.1-dev`, `libxdo-dev`, `libssl-dev`,
`pkg-config`). Copies `Cargo.toml` + `Cargo.lock` + `crates/` +
the prebuilt `frontend/dist/` from stage 1 (the latter satisfies
`include_str!("../../../frontend/dist/index.html")` at
`crates/core/src/server.rs:54` and `gui.rs:65`).

The build line is `cargo build --release --features gui --bin
thclaws`. Two BuildKit cache mounts (`/usr/local/cargo/registry`
and `/src/target`) make repeat builds fast — the first build takes
~25 minutes on `linux/amd64` (most of it the cargo dep tree); the
second is single-digit minutes for incremental Rust changes.

**Stage 3 — `runtime`** (`debian:bookworm-slim`).

`libgtk-3-0` + `libwebkit2gtk-4.1-0` (runtime variants of the dev
packages — needed because the binary is dynamically linked) +
`ca-certificates` (TLS trust roots for the agent's outbound HTTPS) +
`git` (used by skills / CLAUDE.md cascade / many user workflows) +
`curl` (HEALTHCHECK target + general utility) + `ripgrep` (used by
the agent's Grep tool — bundled so users don't have to remember to
mount `rg` from the host).

The compiled `thclaws` binary lands at `/usr/local/bin/thclaws`.
`WORKDIR /workspace`, `EXPOSE 8443`, `ENV THCLAWS_INSIDE_DOCKER=1`
(reserved for future code paths that want to detect the container —
not consumed yet). HEALTHCHECK hits `/healthz` every 30s after a
20s start-grace.

Final image size lands around 600–700 MB after the GTK / WebKit2GTK
runtime libs and the bundled tools. Distroless or alpine would cut
that in half but we'd lose the in-container shell needed for `docker
exec` debugging — debian-slim is the practical default for a
developer-facing tool.

## Why `--bind 0.0.0.0` inside the container

The default `--serve` bind is `127.0.0.1:8443` (security-relevant
invariant pinned by `default_serve_config_binds_localhost` in
`crates/core/src/server.rs:626`). Inside a container, that bind
means "the container's loopback only" — host port-forwarding can't
reach it, so the docker CMD overrides to `--bind 0.0.0.0`.

The compose file then binds the host side as `127.0.0.1:8443:8443`
so the *host* loopback restriction is preserved. Net result: the
service is reachable only from the host machine itself; remote
access still requires an SSH tunnel or reverse proxy with auth, the
same as the bare-metal `--serve` deploy.

If you change the compose port mapping to `8443:8443` (without the
`127.0.0.1:` prefix), the container becomes reachable on the host's
LAN — only safe behind your own auth perimeter. Phase 1 has no
application-level auth (see [`serve-mode.md`](serve-mode.md) trust
model).

## Volumes

- `./:/workspace` — the project folder. thClaws treats `/workspace`
  as cwd and writes `./.thclaws/{settings.json, sessions/, team/,
  worktrees/, todos.md, …}` to the host filesystem.
- `thclaws-config:/root/.config/thclaws` — user-level settings
  (catalogue cache, recent dirs, SSO marker, etc.). Named volume so
  it persists across `docker compose down` cycles.

The container runs as root by default. On Linux this means files
written into the bind mount are owned by `root` on the host —
annoying but not destructive. Override with `user: "1000:1000"` in
compose if you want host-UID-owned writes; on macOS Docker Desktop
handles UID translation transparently so the override isn't needed.

## Credentials

The container has **no OS keychain** — `keyring` calls would need
`gnome-keyring` or similar daemon, which isn't worth the dep weight
for a server deploy. So:

1. `--env-file .env` (compose `env_file: [.env]`) is the primary
   path: drop `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` /
   `GEMINI_API_KEY` / etc. into a `.env` file and Docker injects
   them as process env vars. thClaws's `secrets::resolved_backend`
   reads from env transparently — no keychain prompt because no
   keychain.
2. Or mount a `.env` directly into `/workspace/.thclaws/.env` —
   thClaws's `.env` discovery picks it up the same way.

Don't bake API keys into the image. The `.dockerignore` excludes
`.env*` from the build context for exactly this reason.

## Tagging strategy

- **`:latest`** — most recent published release (re-tagged on each
  ship).
- **`:edge`** — current `main` (re-built from source on each merge,
  no version semantics).
- **`:0.9.9`** (and similar release tags) — pinned, immutable.
  Recommended for any production deploy.

Multi-arch (`linux/amd64` + `linux/arm64`) — `docker pull` picks
the matching variant per host. Built via `docker buildx build
--platform linux/amd64,linux/arm64 --push`, which uses QEMU emulation
for whichever arch isn't native to the build host. Push from a
machine with reasonable headroom — emulated builds are slow.

## Publish workflow

Done from the public mirror (where `Dockerfile` lives):

```sh
cd ~/__2026/thClaws

docker login -u thclaws -p "$DOCKERHUB_ACCESS_TOKEN"
docker buildx create --use --name thclaws-builder >/dev/null 2>&1 || true

# Edge builds from current main
docker buildx build --platform linux/amd64,linux/arm64 \
  -t thclaws/thclaws:edge \
  -t thclaws/thclaws:latest \
  --push .

# On a release cut, also tag the version:
docker buildx build --platform linux/amd64,linux/arm64 \
  -t thclaws/thclaws:0.9.9 \
  -t thclaws/thclaws:latest \
  --push .
```

The PAT lives in `~/__2026/agentic-workspace/.env` as
`DOCKERHUB_ACCESS_TOKEN` (Dotenv backend, same as everything else
under that workspace). `docker login` consumes it once per shell;
the credential helper caches it after that.

## Known limits / follow-up

- **Image bloat from `gui` feature.** Server-only build would drop
  GTK + WebKit2GTK and shave ~250 MB. Needs `crate::server` decoupled
  from `crate::gui`'s feature gate (see top of file).
- **No CI auto-publish.** `:edge` is currently a manual `buildx
  build --push`. A `release.yml` GHA step that triggers on tag push
  would close the loop, but isn't wired yet.
- **No container-side keyring.** Intentional (see Credentials), but
  documented here so the absence isn't surprising.
- **Single-binary image only.** `thclaws-cli` isn't bundled
  separately — `docker run --rm -it … thclaws -p "…"` works against
  the same image, but if someone wants a slim CLI-only image (no
  GUI feature), that's a separate Dockerfile.

## See also

- [`serve-mode.md`](serve-mode.md) — what `--serve` actually does;
  the trust model the container inherits.
- [`running-modes.md`](running-modes.md) — full mode matrix; this
  doc covers only the container-friendly slice.
- User manual: [chapter 2 — Installation](../user-manual/ch02-installation.md)
  has the operator-facing `docker run` / compose recipe.
