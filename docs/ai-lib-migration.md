# ZeroSpider ↔ ai-lib-rust / ai-protocol migration

English summary + 中文：本页固定 **版本矩阵** 与本地开发方式，对应 `ZEROSPIDER_AI_LIB_MIGRATION_PLAN.md` Phase 0。

## Version matrix (pin for reproducible builds)

| Component | Recommended | Notes |
|-----------|-------------|--------|
| `ai-lib-rust` (crates.io) | **0.9.4+** | Workspace facade over `ai-lib-core` + policy layers; use same minor as protocol QA. |
| `ai-protocol` (Git) | tag or commit documented in team runbook | Manifest YAML + JSON Schema; set `AI_PROTOCOL_DIR` to a checkout root. |

Patch bumps (0.9.x) should stay semver-compatible; re-run `cargo test --features ai-protocol` after any bump.

## Environment

| Variable | Purpose |
|----------|---------|
| `AI_PROTOCOL_DIR` | Root of an `ai-protocol` clone (contains provider manifests / schema). Required for manifest-driven `AiClient` resolution at runtime. |

Optional: `AI_PROTOCOL_PATH` is recognized by some ai-lib tooling as an alias—prefer `AI_PROTOCOL_DIR` for ZeroSpider docs consistency.

## Local development with a git checkout

`Cargo.toml` (workspace root):

```toml
[patch.crates-io]
ai-lib-rust = { path = "../ai-lib-rust/crates/ai-lib-rust" }
```

Use only for local debugging; do not commit `[patch.crates-io]` unless the team explicitly maintains a fork workflow.

## Build / CI commands

```bash
# Protocol provider graph (required in CI)
cargo check --features ai-protocol
cargo test --features ai-protocol

# Manifest-only (no built-in vendor HTTP adapters)
cargo test -p zerospider --no-default-features --features ai-protocol
```

Feature flags:

- **`ai-protocol`** — enables optional `ai-lib-rust`, `protocol_registry`, and protocol CLI.
- **`legacy-providers`** — built-in vendor adapters (`openrouter`, `anthropic`, `custom:`, …). Omit via `--no-default-features` when you only use `provider/model` + `AI_PROTOCOL_DIR`.

## CLI: manifest introspection

With `AI_PROTOCOL_DIR` set to a **local** ai-protocol checkout:

```bash
zerospider models protocol-providers
zerospider models protocol-models
zerospider models protocol-providers --json
```

## Next steps

See `active/projects/zerospider/ZEROSPIDER_AI_LIB_MIGRATION_PLAN.md` in **ai-lib-plans** for phased PRs (Phase 1 = dependency bump + adapter alignment, etc.).
