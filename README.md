# ZeroSpider 🕷️

**Protocol-driven autonomous AI agent runtime with intelligent model selection and multi-model negotiation.**

[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](LICENSE-MIT)
[![Rust](https://img.shields.io/badge/rust-2021-orange.svg)](https://www.rust-lang.org/)

[English](README.md) · [简体中文](README.zh-CN.md)

[Contributing](CONTRIBUTING.md) · [ai-lib migration (matrix & env)](docs/ai-lib-migration.md)

---

## Overview

ZeroSpider is a Rust-first autonomous AI agent runtime that integrates with the [ai-protocol](https://github.com/ailib-official/ai-protocol) ecosystem for intelligent, protocol-driven AI operations.

### Key Features

| Feature | Description |
|---------|-------------|
| **Protocol-driven Providers** | Configure AI providers via YAML files without code changes |
| **Intelligent Model Selection** | Automatically select the best model for each task based on cost, speed, quality, and reliability |
| **Multi-model Negotiation** | Get multiple AI opinions and synthesize the best response |
| **Parallel Task Execution** | Execute independent tasks concurrently for faster results |
| **Remote Deployment** | Deploy agents to remote servers with controlled access |
| **Hardware Integration** | Support for GPIO, STM32, and other peripherals |

---

## Quick Start

### Prerequisites

- Rust 1.70+ (2021 edition)
- AI Protocol directory (optional, for protocol-driven providers)

### Build

```bash
# Clone the repository
git clone https://github.com/ailib-official/zerospider.git
cd zerospider

# Build with all features
cargo build --features ai-protocol,smart-routing,multi-model,remote-deploy

# Or build with default features only
cargo build
```

### Cross-Compile for Raspberry Pi (aarch64)

```bash
# Install target
rustup target add aarch64-unknown-linux-gnu

# Build release binary
cargo build --release --target aarch64-unknown-linux-gnu

# Binary location
ls target/aarch64-unknown-linux-gnu/release/zerospider
```

### Run

```bash
# Enable smart model selection
cargo run --features smart-routing -- --smart

# Enable multi-model negotiation
cargo run --features multi-model -- --negotiate

# Enable all features
cargo run --features ai-protocol,smart-routing,multi-model,remote-deploy
```

---

## Configuration

### Environment Variables

```bash
# AI Protocol directory (for protocol-driven providers)
# Clone from: git clone https://github.com/ailib-official/ai-protocol
export AI_PROTOCOL_DIR=/path/to/ai-protocol

# Provider API keys
export OPENAI_API_KEY=sk-...
export ANTHROPIC_API_KEY=sk-ant-...
```

### Config File

```yaml
# ~/.zerospider/config.yaml
routing:
  smart_selection: true
  
negotiation:
  enabled: true
  min_providers: 2
  consensus_threshold: 0.7

deployment:
  remote_enabled: true
  allowed_hosts:
    - server1.example.com
    - server2.example.com
```

---

## Documentation

- [User Guide (Chinese)](docs/user-guide.zh-CN.md) - Complete user documentation
- [Integration Guide](docs/ai-protocol-integration-guide.md) - Developer integration guide
- [Upstream Requirements](docs/upstream-requirements.md) - Requirements for ai-lib-rust and ai-protocol

### User Guide Chapters

1. [Getting Started](docs/user-guide/01-getting-started.md)
2. [Basic Concepts](docs/user-guide/02-basic-concepts.md)
3. [Chat with AI](docs/user-guide/03-chat-with-ai.md)
4. [Providers](docs/user-guide/04-providers.md)
5. [Smart Routing](docs/user-guide/05-smart-routing.md)
6. [Channels](docs/user-guide/06-channels.md)
7. [Telegram](docs/user-guide/07-telegram.md)
8. [Tools](docs/user-guide/10-tools.md)
9. [Negotiation](docs/user-guide/13-negotiation.md)
10. [Automation](docs/user-guide/14-automation.md)
11. [Deployment](docs/user-guide/15-deployment.md)
12. [Remote Deployment](docs/user-guide/16-remote-deployment.md)
13. [Security](docs/user-guide/17-security.md)
14. [Commands](docs/user-guide/18-commands.md)
15. [Configuration](docs/user-guide/19-config.md)
16. [FAQ](docs/user-guide/20-faq.md)

---

## Feature Flags

| Flag | Description |
|------|-------------|
| `ai-protocol` | Enable ai-lib-rust integration (protocol-driven providers via `protocol:provider/model`) |
| `smart-routing` | Enable provider scoring and adaptive model selection |
| `multi-model` | Enable multi-model negotiation and parallel tasks |
| `remote-deploy` | Enable controlled remote deployment |
| `hardware` | Enable hardware peripherals support |
| `channel-matrix` | Enable Matrix channel with E2EE |

## Dashboard

When running the gateway (`zerospider gateway`), a monitoring dashboard is available at `GET /dashboard`:

- **Status**: Health and pairing state
- **Cost**: Session, daily, monthly costs and token usage (when `[cost] enabled = true`)
- **Runtime**: Component health snapshot

The dashboard auto-refreshes every 30 seconds.

---

## Remote Deployment

ZeroSpider supports controlled remote deployment via SSH with the `--features remote-deploy` flag.

### Deploy Commands

```bash
# Deploy to a remote server
zerospider deploy deploy --server <server-id>

# Check deployment status
zerospider deploy status --server <server-id>

# Run health check
zerospider deploy health-check --server <server-id>

# List configured deployment targets
zerospider deploy list

# Rollback to previous deployment
zerospider deploy rollback --server <server-id>

# Update to new version
zerospider deploy update --server <server-id> --version <version>

# Sync configuration
zerospider deploy sync-config --server <server-id>
```

### Configuration

Configure deployment targets in your `config.toml`:

```toml
[deploy]
[[deploy.servers]]
id = "prod-001"
host = "192.168.1.100"
port = 22
user = "deploy"
ssh_key = "~/.ssh/id_ed25519"
labels = ["env:production", "region:us-west"]

[deploy.settings]
mode = "direct"  # Options: direct, docker, systemd
binary_path = "/usr/local/bin/zerospider"
working_dir = "/opt/zerospider"
auto_start = true
health_check_interval_secs = 30
restart_on_failure = true
max_restarts = 3
```


## Architecture

ZeroSpider uses a trait-driven, modular architecture:

- **Providers**: AI model backends (OpenAI, Anthropic, local models, etc.)
- **Channels**: Communication platforms (Telegram, Discord, Matrix, etc.)
- **Tools**: Extensible tool execution (shell, file, browser, etc.)
- **Memory**: Persistent storage backends (SQLite, PostgreSQL, etc.)
- **Security**: Policy enforcement and secret management

---

## Upstream Dependencies

ZeroSpider integrates with:

- [ai-lib-rust](https://crates.io/crates/ai-lib-rust) - Protocol-driven AI API client (crates.io, enable with `--features ai-protocol`)
- [ai-protocol](https://github.com/ailib-official/ai-protocol) - Provider YAML configs (clone and set `AI_PROTOCOL_DIR`)

### Sync with Upstream

ZeroSpider tracks [zeroclaw-labs/zeroclaw](https://github.com/zeroclaw-labs/zeroclaw) for updates:

```bash
# List upstream changes
./sync-upstream.sh --list

# Preview merge
./sync-upstream.sh --dry-run

# Merge upstream
./sync-upstream.sh

# Cherry-pick specific commit
./sync-upstream.sh --cherry-pick <commit-hash>
```

---

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

---

## Author

**Luqiang Wang**

---

## Acknowledgments

ZeroSpider is a fork of [ZeroClaw](https://github.com/ZeroClaw-Labs/zeroclaw) with additional features:
- ai-protocol integration
- Provider scoring and smart routing
- Multi-model negotiation
- Parallel task execution
- Remote deployment
