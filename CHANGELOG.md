# Changelog

All notable changes to ZeroSpider will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **PT-074 BYOK availability:** protocol provider discovery now delegates credential availability to ai-lib-rust's unified credential chain (endpoint.auth, V1 auth, conventional env fallback, and keyring when enabled) instead of maintaining a ZeroSpider-only env scan.
- **Default Cargo features** now include only `ai-protocol` (not `legacy-providers`), aligning with the ai-lib migration plan’s “protocol-first” default. Enable `--features legacy-providers` (or a non-default dist profile that turns it on) for built-in HTTP vendor adapters and the full legacy provider factory test matrix.

### Dependencies

- Bumped `ai-lib-rust` to **0.9.4** (optional; `--features ai-protocol`).
- Added optional `async-stream` and `serde_yaml` for the protocol adapter and manifest registry.

### Added

- **`legacy-providers`** feature: gates built-in HTTP vendor adapters (`openrouter`, `anthropic`, `custom:`, …). Use `--no-default-features` with `--features ai-protocol` for manifest-only builds.
- **`ai-protocol`**: protocol-backed providers, `protocol_registry` scan, and `zerospider models protocol-providers` / `protocol-models` CLI.
- Quick setup warns when `provider/model` is used without a usable local `AI_PROTOCOL_DIR`.
- **`routing_mvp`** optional feature: forwards `ai-lib-rust/routing_mvp` for experimental routing; CI runs a `cargo check` compile gate with `ai-protocol` (ZS-ML-004).

### Documentation

- **`docs/migration-legacy-to-protocol.md`**: maps legacy string keys / `custom:` / feature flags to `provider/model` + `AI_PROTOCOL_DIR`; describes default vs `legacy-providers` CI matrix (ZS-ML-005).
- **`docs/ai-lib-migration.md`**: compatibility window (pre-1.0 cadence, ai-protocol pin policy) (ZS-ML-006).
- **`docs/ai-lib-migration.md`**: resilience boundaries — transport retry in `ProtocolBackedProvider` vs `[reliability]` fallback chains.
- **`docs/ai-lib-migration.md`**: Phase 2 — minimal TOML for `provider/model` logical ids and `[reliability]` fallbacks (ZS-ML-003).
- **`docs/ai-lib-migration.md`**: optional `routing_mvp` feature and `ProtocolBackedProvider` note on transport retry vs app-level fallbacks (ZS-ML-004).

### Testing

- **PT-074 BYOK smoke:** protocol registry unit tests cover V2 endpoint.auth.token_env and conventional env fallback availability without legacy-providers.
- **Config**: regression test that TOML accepts protocol-style `default_provider` and `[reliability]` entries (ZS-ML-003).
- **Protocol env**: unit tests for `protocol_root_from_path_value` (reject HTTP URLs; accept existing directories) (ZS-ML-006).
- **Docs**: contract test for `docs/migration-legacy-to-protocol.md` (ZS-ML-005).

### Changed (UX / errors)

- **Protocol path**: clearer errors from `resolve_ai_client` / `ProtocolBackedProvider::new` when `provider/model` resolution fails, pointing to `AI_PROTOCOL_DIR` and the migration doc (ZS-ML-006).
- **Quick setup**: stronger tip when `provider/model` is used without a valid local protocol root (ZS-ML-006).

### Fixed

- **`Cargo.toml`**: `ai-lib-rust` was accidentally nested under `[target.'cfg(unix)'.dependencies]`, so it was missing on Windows. It is now in the main `[dependencies]` table.

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

### Changed (ai-lib / ai-protocol rectification, 2026-04)

- **Streaming / tool calls** (ZS-ML-007, PR #19): `ProtocolBackedProvider` maps ai-lib-rust `StreamingEvent` tool-call lifecycle (`ToolCallStarted`, `PartialToolCall`, `ToolCallEnded`) into `StreamChunk`, adding structured `StreamToolCallDelta` for streaming tool use.
- **Dependencies** (ZS-ML-009, PR #21): stop enabling unused `ai-lib-rust` Cargo features `embeddings`, `batch`, and `telemetry` until ZeroSpider has real call sites; document deferral and the `observability-otel` vs ai-lib metrics boundary in migration docs.

### Testing (ai-lib / ai-protocol rectification, 2026-04)

- **CI** (ZS-ML-008, PR #20): run `cargo test -p zerospider --no-default-features --features ai-protocol` (in addition to the manifest-only `cargo check` gate).
- **Resilience** (ZS-ML-008, PR #20): `ReliableProvider` unit tests for protocol-style logical model fallbacks and for app-layer retry budgeting vs inner transport retries.

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

## [0.2.0] - 2026-02-21

### Added
- **Dashboard**: `GET /dashboard` and `GET /api/dashboard` for monitoring status, cost, and runtime
- **ai-protocol feature**: ai-lib-rust from crates.io (v0.8), protocol providers via `protocol:provider/model`
- **README**: Aligned EN/ZH, added dashboard and dependency source docs

### Changed
- **Dependencies**: ai-lib-rust now from crates.io (was path); ai-protocol remains env-based (clone from GitHub)
- **User Guide**: Aligned docs to ZeroSpider branding (ZeroClaw → ZeroSpider, zeroclaw → zerospider, ~/.zeroclaw → ~/.zerospider)

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
