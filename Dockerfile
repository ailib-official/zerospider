# syntax=docker/dockerfile:1.7

# ── Stage 1: Build ────────────────────────────────────────────
FROM rust:1.98-slim@sha256:bce1476d4be4d78b83705bc5f428b86d640eeeea33e9dadafbc037b5703a53bf AS builder

WORKDIR /app

# Install build dependencies
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
    apt-get update && apt-get install -y \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

# 1. Copy manifests and toolchain pin to cache dependencies with the same compiler
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates/robot-kit/Cargo.toml crates/robot-kit/Cargo.toml
COPY crates/velaclaw-config/Cargo.toml crates/velaclaw-config/Cargo.toml
COPY crates/velaclaw-agent-runtime/Cargo.toml crates/velaclaw-agent-runtime/Cargo.toml
# Create dummy targets declared in Cargo.toml so manifest parsing succeeds.
RUN mkdir -p src benches crates/robot-kit/src crates/velaclaw-config/src crates/velaclaw-agent-runtime/src \
    && echo "fn main() {}" > src/main.rs \
    && echo "fn main() {}" > benches/agent_benchmarks.rs \
    && echo "pub fn placeholder() {}" > crates/robot-kit/src/lib.rs \
    && echo "pub fn placeholder() {}" > crates/velaclaw-config/src/lib.rs \
    && echo "pub fn placeholder() {}" > crates/velaclaw-agent-runtime/src/lib.rs
RUN --mount=type=cache,id=velaclaw-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=velaclaw-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=velaclaw-target,target=/app/target,sharing=locked \
    cargo build --release --locked
RUN rm -rf src benches crates/robot-kit/src crates/velaclaw-config/src crates/velaclaw-agent-runtime/src

# 2. Copy only build-relevant source paths (avoid cache-busting on docs/tests/scripts)
COPY src/ src/
COPY benches/ benches/
COPY crates/ crates/
COPY firmware/ firmware/
RUN --mount=type=cache,id=velaclaw-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=velaclaw-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=velaclaw-target,target=/app/target,sharing=locked \
    cargo build --release --locked && \
    cp target/release/velaclaw /app/velaclaw && \
    strip /app/velaclaw

# Prepare runtime directory structure and default config inline (no extra stage)
RUN mkdir -p /velaclaw-data/.velaclaw /velaclaw-data/workspace && \
    cat > /velaclaw-data/.velaclaw/config.toml <<EOF && \
    chown -R 65534:65534 /velaclaw-data
workspace_dir = "/velaclaw-data/workspace"
config_path = "/velaclaw-data/.velaclaw/config.toml"
api_key = ""
default_provider = "openai/gpt-5.2"
default_model = "openai/gpt-5.2"
default_temperature = 0.7

[gateway]
port = 3000
host = "[::]"
allow_public_bind = true
EOF

# ── Stage 2: Development Runtime (Debian) ────────────────────
FROM debian:trixie-slim@sha256:4e401d95de7083948053197a9c3913343cd06b706bf15eb6a0c3ccd26f436a0e AS dev

# Install essential runtime dependencies only (use docker-compose.override.yml for dev tools)
RUN apt-get update && apt-get install -y \
    ca-certificates \
    curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /velaclaw-data /velaclaw-data
COPY --from=builder /app/velaclaw /usr/local/bin/velaclaw

# Overwrite minimal config with DEV template (Ollama defaults)
COPY dev/config.template.toml /velaclaw-data/.velaclaw/config.toml
RUN chown 65534:65534 /velaclaw-data/.velaclaw/config.toml

# Environment setup
# Use consistent workspace path
ENV VELACLAW_WORKSPACE=/velaclaw-data/workspace
ENV HOME=/velaclaw-data
# Defaults for local dev (Ollama) - matches config.template.toml
ENV PROVIDER="ollama"
ENV VELACLAW_MODEL="llama3.2"
ENV VELACLAW_GATEWAY_PORT=3000

# Note: API_KEY is intentionally NOT set here to avoid confusion.
# It is set in config.toml as the Ollama URL.

WORKDIR /velaclaw-data
USER 65534:65534
EXPOSE 3000
ENTRYPOINT ["velaclaw"]
CMD ["gateway"]

# ── Stage 3: Production Runtime (Distroless) ─────────────────
FROM gcr.io/distroless/cc-debian13:nonroot@sha256:c31ff9abcb1910f3ab25c7957bdaf0bfe12a01eb546e8df2282f1c8f682b606c AS release

COPY --from=builder /app/velaclaw /usr/local/bin/velaclaw
COPY --from=builder /velaclaw-data /velaclaw-data

# Environment setup
ENV VELACLAW_WORKSPACE=/velaclaw-data/workspace
ENV HOME=/velaclaw-data
# Default provider (model is set in config.toml, not here,
# so config file edits are not silently overridden)
ENV PROVIDER="openrouter"
ENV VELACLAW_GATEWAY_PORT=3000

# API_KEY must be provided at runtime!

WORKDIR /velaclaw-data
USER 65534:65534
EXPOSE 3000
ENTRYPOINT ["velaclaw"]
CMD ["gateway"]
