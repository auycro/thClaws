# syntax=docker/dockerfile:1.7
#
# thClaws container image — runs `thclaws --serve` so the project at
# /workspace is reachable from a browser. The deploy unit is one
# container per project (cd into a project, mount it at /workspace,
# expose port 8443).
#
# Three stages:
#   1. frontend  — Vite + React build → dist/
#   2. builder   — cargo build --release --features gui --bin thclaws
#   3. runtime   — debian-slim with the binary + GTK/WebKit2GTK libs
#
# The `gui` feature pulls in tao/wry/comrak/rfd/native-dialog. --serve
# never opens a window, but the binary is dynamically linked to those
# libs so they must be present at runtime — hence GTK + WebKit2GTK in
# the runtime image. A future refactor that gates --serve independently
# of `gui` will let us drop those and shrink the image significantly.

ARG NODE_VERSION=22-bookworm-slim
ARG RUST_VERSION=1-bookworm
ARG RUNTIME_BASE=debian:bookworm-slim

# ──────────────────────────────────────────────────────────────────────
FROM node:${NODE_VERSION} AS frontend
WORKDIR /src

RUN corepack enable

COPY frontend/package.json frontend/pnpm-lock.yaml ./frontend/
WORKDIR /src/frontend
RUN pnpm install --frozen-lockfile

WORKDIR /src
COPY frontend/ ./frontend/
# TerminalView.tsx imports `../../../banner.txt?raw` — the file lives at
# the repo root, so it has to land at /src/banner.txt (the parent of
# frontend/) for Vite to resolve the relative path.
COPY banner.txt ./banner.txt
WORKDIR /src/frontend
RUN pnpm run build

# ──────────────────────────────────────────────────────────────────────
FROM rust:${RUST_VERSION} AS builder
WORKDIR /src

RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config \
        libssl-dev \
        libgtk-3-dev \
        libwebkit2gtk-4.1-dev \
        libsoup-3.0-dev \
        libjavascriptcoregtk-4.1-dev \
        libxdo-dev \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/
# crates/core/src/branding.rs has `include_str!("../../../banner.txt")`
# so the file has to land at the workspace root for cargo to pick it up.
COPY banner.txt ./banner.txt
COPY --from=frontend /src/frontend/dist ./frontend/dist

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --features gui --bin thclaws \
 && cp target/release/thclaws /usr/local/bin/thclaws

# ──────────────────────────────────────────────────────────────────────
FROM ${RUNTIME_BASE} AS runtime

# Run as non-root. Override USER_UID/USER_GID at build time so files
# written to the bind-mounted /workspace are owned by your host user:
#   docker build \
#     --build-arg USER_UID=$(id -u) \
#     --build-arg USER_GID=$(id -g) ...
ARG USERNAME=thclaws
ARG USER_UID=1000
ARG USER_GID=1000

RUN apt-get update && apt-get install -y --no-install-recommends \
        libgtk-3-0 \
        libwebkit2gtk-4.1-0 \
        ca-certificates \
        git \
        curl \
        ripgrep \
        # Media tooling — ffmpeg covers transcode / thumbnail / clip /
        # audio extract / animated WebP. ~100 MB with deps; users
        # creating media (the bulk of agent work alongside coding) get
        # it built-in instead of failing on `command not found`.
        ffmpeg \
        # Python for any agent that runs `python3 …` from Bash — most
        # AI agents lean on it for one-off scripts (data shaping,
        # plotting, quick HTTP). `python3-venv` so users can spin
        # their own venv under /workspace without sudo.
        python3 \
        python3-pip \
        python3-venv \
        # Node.js — pulled in by Playwright below, but exposing it
        # directly lets users `node …` / `npm …` for non-browser
        # tasks too (TypeScript scripts, bundlers, the usual).
        nodejs \
        npm \
    && rm -rf /var/lib/apt/lists/*

# Playwright + chromium for browser automation (e2e tests, web
# scraping that needs JS rendering, screenshot generation). `npm i -g`
# puts the cli at /usr/local/bin; `playwright install --with-deps`
# pulls chromium + every system .so it needs (libnss3, libatk-*,
# libcups2, libdrm2, … — ~100 MB of apt packages on top of the ~200
# MB chromium download). Total runtime image grows ~300 MB.
RUN npm install -g playwright \
    && npx playwright install --with-deps chromium \
    && rm -rf /root/.npm /tmp/npm-* /var/lib/apt/lists/*

# Create group + user matching host UID/GID
RUN groupadd --gid ${USER_GID} ${USERNAME} \
    && useradd --uid ${USER_UID} \
               --gid ${USER_GID} \
               --create-home \
               --shell /bin/bash \
               ${USERNAME}

COPY --from=builder /usr/local/bin/thclaws /usr/local/bin/thclaws

WORKDIR /workspace

EXPOSE 8443

ENV THCLAWS_INSIDE_DOCKER=1

HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8443/healthz || exit 1

# Switch to non-root user
USER ${USER_UID}:${USER_GID}

ENTRYPOINT ["thclaws"]
CMD ["--serve", "--bind", "0.0.0.0", "--port", "8443"]
