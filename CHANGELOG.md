# Changelog

All notable changes to ZeroSpider will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.1] - 2026-02-21

### Added
- **Project Fork**: ZeroSpider forked from [ZeroClaw](https://github.com/zeroclaw-labs/zeroclaw) with enhanced features
- **Raspberry Pi Support**: Cross-compilation for aarch64-unknown-linux-gnu target (64-bit ARM)
- **Upstream Sync Script**: `sync-upstream.sh` for tracking zeroclaw-labs/zeroclaw main branch
  - `--dry-run` mode for preview
  - `--list` mode to show upstream changes
  - `--cherry-pick <commit>` for selective merging
- **New Tools from Upstream**:
  - `pdf_read` - Extract text from PDF files
  - `glob_search` - Secure file pattern search with glob support

### Fixed
- **Provider Fixes**: Ollama and ReliableProvider tool calling restored
- **Telegram**: Message overflow prevention from continuation markers
- **Gemini OAuth**: Series of fixes for OAuth envelope and payload handling
- **Cron**: JobType persistence and conversion fixes
- **Onboard**: Explicit overwrite confirmation for existing config
- **Build**: Release-fast profile compilation errors resolved

### Changed
- **Project Name**: Renamed from ZeroClaw to ZeroSpider
- **License**: Dual MIT OR Apache-2.0 license
- **Author**: Luqiang Wang
- **Repository**: https://github.com/ailib-official/zerospider

### Security
- **Cron Tools**: Security policy now passed to cron tools in registry

### Documentation
- Restored AGENTS.md and CLAUDE.md as functional documentation
- Updated README with ZeroSpider branding

## [0.2.0] - 2026-02-21

### Added
- **Dashboard**: `GET /dashboard` and `GET /api/dashboard` for monitoring status, cost, and runtime
- **ai-protocol feature**: ai-lib-rust from crates.io (v0.8), protocol providers via `protocol:provider/model`
- **README**: Aligned EN/ZH, added dashboard and dependency source docs

### Changed
- **Dependencies**: ai-lib-rust now from crates.io (was path); ai-protocol remains env-based (clone from GitHub)
- **User Guide**: Aligned docs to ZeroSpider branding (ZeroClaw → ZeroSpider, zeroclaw → zerospider, ~/.zeroclaw → ~/.zerospider)


## [0.3.0] - 2026-02-23

### Added
- **Remote Deployment Feature**: Complete SSH-based remote deployment system for ZeroSpider
  - New feature flag: `--features remote-deploy`
  - CLI commands: `deploy deploy`, `deploy status`, `deploy health-check`, `deploy list`, `deploy rollback`, `deploy update`, `deploy sync-config`
  - Multiple deployment modes: Direct (binary), Docker, and systemd
  - Health monitoring with automated health checks
  - Rollback support for safe deployments
  - Configuration synchronization to remote servers
- **Deploy Configuration Schema**:
  - `[deploy.servers]` for defining deployment targets (host, port, user, ssh_key, labels)
  - `[deploy.settings]` for deployment parameters (mode, binary_path, working_dir, auto_start, etc.)
- **Deployment Module** (`src/deploy/`):
  - `RemoteDeployer` for managing deployments
  - `DeploymentTarget` for server configuration
  - `DeploymentConfig` for deployment settings
  - `DeploymentStatus` for tracking deployment state
- **User Guide Chapter**: `docs/user-guide/16-remote-deployment.md` with comprehensive deployment documentation
- **Unit Tests**: Comprehensive test coverage for deploy module (`src/deploy/remote.rs`)

### Changed
- **Main Config struct**: Added `deploy` field for deployment configuration
- **Config Schema**: Added `DeployConfig`, `DeploymentTargetConfig`, and `DeploymentSettingsConfig` structs
- **Main CLI**: Added `Deploy` command variant with subcommands
- **Wizard**: Updated wizard to include default `deploy` configuration in generated configs
- **README**: Updated with Remote Deployment section and Deploy commands documentation
- **rust-toolchain.toml**: Fixed toolchain configuration (changed from incorrect Windows toolchain to stable)

### Security
- Deployment uses SSH key-based authentication, avoiding password authentication
- Supports custom SSH key paths for different deployment environments

### Documentation
- README.md: Added "Remote Deployment" section with commands and configuration examples
- docs/user-guide/16-remote-deployment.md: Complete user guide for remote deployment
- Updated user guide chapters list in README.md

Technical Notes:
- Library builds successfully with all deploy features
- Note: The binary has a pre-existing compilation error in `src/gateway/mod.rs` related to `crate::cost::CostTracker` resolution that's unrelated to the deploy feature


## [Unreleased]

### Security
- **Legacy XOR cipher migration**: The `enc:` prefix (XOR cipher) is now deprecated. 
  Secrets using this format will be automatically migrated to `enc2:` (ChaCha20-Poly1305 AEAD)
  when decrypted via `decrypt_and_migrate()`. A `tracing::warn!` is emitted when legacy
  values are encountered. The XOR cipher will be removed in a future release.

### Added
- `SecretStore::decrypt_and_migrate()` — Decrypts secrets and returns a migrated `enc2:` 
  value if the input used the legacy `enc:` format
- `SecretStore::needs_migration()` — Check if a value uses the legacy `enc:` format
- `SecretStore::is_secure_encrypted()` — Check if a value uses the secure `enc2:` format
- **Telegram mention_only mode** — New config option `mention_only` for Telegram channel.
  When enabled, bot only responds to messages that @-mention the bot in group chats.
  Direct messages always work regardless of this setting. Default: `false`.

### Deprecated
- `enc:` prefix for encrypted secrets — Use `enc2:` (ChaCha20-Poly1305) instead.
  Legacy values are still decrypted for backward compatibility but should be migrated.

### Fixed
- **Onboarding channel menu dispatch** now uses an enum-backed selector instead of hard-coded
  numeric match arms, preventing duplicated pattern arms and related `unreachable pattern`
  compiler warnings in `src/onboard/wizard.rs`.
- **OpenAI native tool spec parsing** now uses owned serializable/deserializable structs,
  fixing a compile-time type mismatch when validating tool schemas before API calls.

## [0.1.0] - 2026-02-13

### Added
- **Core Architecture**: Trait-based pluggable system for Provider, Channel, Observer, RuntimeAdapter, Tool
- **Provider**: OpenRouter implementation (access Claude, GPT-4, Llama, Gemini via single API)
- **Channels**: CLI channel with interactive and single-message modes
- **Observability**: NoopObserver (zero overhead), LogObserver (tracing), MultiObserver (fan-out)
- **Security**: Workspace sandboxing, command allowlisting, path traversal blocking, autonomy levels (ReadOnly/Supervised/Full), rate limiting
- **Tools**: Shell (sandboxed), FileRead (path-checked), FileWrite (path-checked)
- **Memory (Brain)**: SQLite persistent backend (searchable, survives restarts), Markdown backend (plain files, human-readable)
- **Heartbeat Engine**: Periodic task execution from HEARTBEAT.md
- **Runtime**: Native adapter for Mac/Linux/Raspberry Pi
- **Config**: TOML-based configuration with sensible defaults
- **Onboarding**: Interactive CLI wizard with workspace scaffolding
- **CLI Commands**: agent, gateway, status, cron, channel, tools, onboard
- **CI/CD**: GitHub Actions with cross-platform builds (Linux, macOS Intel/ARM, Windows)
- **Tests**: 159 inline tests covering all modules and edge cases
- **Binary**: 3.1MB optimized release build (includes bundled SQLite)

### Security
- Path traversal attack prevention
- Command injection blocking
- Workspace escape prevention
- Forbidden system path protection (`/etc`, `/root`, `~/.ssh`)

[0.3.0]: https://github.com/ailib-official/zerospider/releases/tag/v0.3.0
[0.2.0]: https://github.com/ailib-official/zerospider/releases/tag/v0.2.0
[0.1.1]: https://github.com/ailib-official/zerospider/releases/tag/v0.1.1
[0.1.0]: https://github.com/theonlyhennygod/zeroclaw/releases/tag/v0.1.0
