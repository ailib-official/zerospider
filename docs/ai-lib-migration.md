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

# Manifest-only (same as default features today: `ai-protocol` only)
cargo test -p zerospider --no-default-features --features ai-protocol

# Full legacy HTTP factory + integration tests (optional; also used in CI)
cargo test -p zerospider --features "ai-protocol legacy-providers"
```

Feature flags:

- **`ai-protocol`** — enables optional `ai-lib-rust`, `protocol_registry`, and protocol CLI. **On by default.**
- **`legacy-providers`** — built-in vendor adapters (`openrouter`, `anthropic`, `custom:`, …). **Off by default**; pass `--features legacy-providers` when you need the old string-key factory or to run `tests/provider_resolution.rs`.
- **`routing_mvp`** — forwards `ai-lib-rust`’s experimental routing feature (optional). **Off by default.** Enable with `--features "ai-protocol routing_mvp"` when you need that code path; CI runs `cargo check -p zerospider --features "ai-protocol routing_mvp" --lib` to prevent bitrot. **Metrics:** if `AiClient` exposes a metrics API in a future `ai-lib-rust` release, wire it to your observability layer without duplicating transport retry counters already covered here vs `[reliability]`.

## CLI: manifest introspection

With `AI_PROTOCOL_DIR` set to a **local** ai-protocol checkout:

```bash
zerospider models protocol-providers
zerospider models protocol-models
zerospider models protocol-providers --json
```

## Resilience boundaries (Phase 4)

ZeroSpider layers **two** independent mechanisms; keep them from overlapping in confusing ways:

| Layer | What it does | Where |
|-------|----------------|--------|
| **Transport retry** | `ai-lib-rust` returns `Error::is_retryable` / `retry_after` → limited retries inside `ProtocolBackedProvider` (`execute_chat_with_retry`). | `src/providers/protocol_adapter.rs` |
| **App failover** | `ReliableProvider` switches to another **provider name** or per-model alternatives from config after repeated failures. | `[reliability]` → `fallback_providers`, `model_fallbacks` |

**Guidance**

- Prefer **one** layer to own a given failure class: e.g. let ai-lib handle 429 backoff for a single logical model; use `fallback_providers` when you truly want a different backend (another provider id or `custom:` URL).
- Optional ai-lib features such as **`routing_mvp`** or **`AiClient::metrics()`** are not required for the manifest path; enable deliberately when you add routing or SLO dashboards.

## Next steps

See `active/projects/zerospider/ZEROSPIDER_AI_LIB_MIGRATION_PLAN.md` in **ai-lib-plans** for phased PRs (Phase 1 = dependency bump + adapter alignment, etc.).
