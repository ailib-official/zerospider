# Changelog

All notable changes to VelaClaw will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.3.0] - 2026-09-08

### Changed

- **Dependencies:** `tokio-rustls` 0.26.4 → 0.26.5 (#304).

## [1.2.0] - 2026-09-08

### Added

- **Hint peer fallback (VL-NA-021 / VL-NA-022):** `[agent].hint_peer_fallback` (dist default **off**) retries `[[model_routes]].fallbacks` after micro-retry. Switchable error **classes**: unavailable (410/EOL, `model_not_found`, vendor HTTP 404 Function Not Found) and quota (429/402). DNS/transport and workspace file-not-found do not switch. Max 5 attempts / 3 cross-provider; success pins the hint and blacklists the failed id.
- **DAG fail auto-replan (VL-NA-024):** `[agent].dag_fail_auto_replan` (dist default **off**) retries a failed live work node once (default model after unavailable/quota).
- **Scratch temps (VL-SEC-012):** file tools rewrite `/tmp` and `/var/tmp` to workspace `.velaclaw/tmp`; shell children get `TMPDIR` there. Host temp writes are not a silent policy deny.
- **Planner (VL-NA-023):** node count follows the task (1–8); each node’s **first** capability tag is Contact. Trial `code`/`tools`/`document` may still share one physical model — that is route-table, not a Contact bug.
- **Step captions (VL-UX-STEP-003):** skip `cd`/`export` setup in shell captions; DAG node labels show the resolved provider/model.

- **Native-app honesty (VL-NA-000):** `velaclaw doctor` reports `envelope_assemble`, Contact live-select flags (`host_decide` / `intent_capability_route`, default off), and `autonomy.level=full` does not mean unsandboxed. `/health` `execution` and Web Overview show the same fields. Default binary still does **not** compile `runtime-wasm`.

- **Security profiles + retractable elevation** (VL-SEC-011): `[security.profile]` = `isolated` | `local` | `readonly` unfolds existing sandbox/autonomy knobs (no second tool loop). `[sandbox_deny]` / `[needs_approval]` re-enter the same `ApprovalGate` even under Full; **Once** dies after that `execute`. **Never** persists to L2.5. Local inherits daemon env in `apply_shell_child_env` only. Credential basenames: unset profile still hard-denies; isolated **Ask**; local **Allow** (scrubbed). CLI stdin uses the same gate as Web (`Y`/`A`/`N`/`!`); cron/heartbeat reuse `loop_::run` but are **not** labeled `cli`, so they cannot HITL-elevate or skip wrap. See [config-reference.md](docs/config-reference.md#security).

- **Sandbox escape on approval** (opt-in policy B): `[security.sandbox] escape_on_approval = true` lets human-approved shell skip Landlock/`no_new_privs` for that invocation (sudo/apt parity). Default remains `false` (always sandboxed). Privilege/package commands require ApprovalHub even under Full when enabled. See [config-reference.md](docs/config-reference.md#securitysandbox).
- **Classified shell failures** (VL-SEC-010): `[policy_deny]` / `[needs_approval]` / `[sandbox_deny]` on the existing `ToolResult.error` string (no second HITL or wrap path). Unset profile still hard-denies credential basenames even when approved.
- **Isolated GitHub CLI token passthrough:** when `inherit_process_env` is off, `GH_TOKEN` / `GITHUB_TOKEN` from the daemon `EnvironmentFile` are injected **only** if the first policy segment is `gh` / `gh.exe` (same `base_executables`). Those children also get workspace `GH_CONFIG_DIR` (Landlock cannot read `~/.config/gh`). Local profile inherits the full daemon env instead. Values are not logged. PAT files remain `[policy_deny]`. See [troubleshooting.md](docs/troubleshooting.md).

## [1.1.0] - 2026-08-19

### Changed

- **Linux shell sandbox is fail-closed** (VL-MA-003): production `ShellTool` always calls `Sandbox::wrap_command`. Linux `backend=auto` is Landlock (child `pre_exec` only) or **fail-closed** — not silent Noop. **Hosts without Landlock refuse even allowlisted shell** until YOLO opt-out (`sandbox.enabled=false` or `backend=none`). Confirm with `velaclaw doctor` (`sandbox=landlock` or explicit YOLO). Non-Linux Auto stays application-layer Noop. See [config-reference.md](docs/config-reference.md#securitysandbox) and [troubleshooting.md](docs/troubleshooting.md).

- **Unwrapped tool IR** (VL-TTC-015): Decode order is native `tool_calls` → envelope codec (XML/DSML/invoke) → line-isolated `{name, arguments}` on the **assistant** turn only. Same `run_tool_call_loop` for CLI/Web/Channel. Display strips carriers before streaming. Unregistered isolated IR is dropped with a continue notice (conversation does not abort).

### Added

- **Turn cancel contract** (VL-UX-CANCEL-002): CLI Esc Esc and Web Stop share `classify_turn_result` (persist only on Completed). Mid-tool cancel promotes to `ToolLoopCancelled`. Shell uses `kill_on_drop`. Stop aborts ApprovalHub / HITL waiters. `POST /api/chat` remains uncancellable. See [cancel-contract.md](docs/cancel-contract.md).

- **Linux default sandbox + receipts** (VL-MA-003): allowlisted commands are still isolated. Approval does not widen `allowed_commands` (SEC-009). Workspace `.velaclaw/tool_receipts.jsonl` records allow/deny/sandbox_fail without secrets. `velaclaw doctor` reports sandbox name, source, and `production_path`. Autonomy Full does not disable the sandbox. `sandbox-landlock` is now a default Cargo feature.

- **Memory retrieve + doctor embedder** (VL-MA-002): `Memory::recall` fills `retrieve_kind=memory` extra chunks on the same `assemble_layered` entry when envelope assemble is on. `velaclaw doctor` reports effective embedder, source, and whether the path is production. Default `embedding_provider` stays `"none"` (Noop). Deterministic consolidation folds Conversation volume and keeps Core.

- **Plan/Build, git-if-present undo, session resume** (VL-MA-004): `--plan` / Web `host_phase=plan` blocks mutating tools on the existing `execute_tool_batch` path before HITL. Default remains Build. `velaclaw undo` restores tracked files only when the workspace already has `.git` (no init, no revert, no untracked clean). CLI `--session-id` uses `workspace/.velaclaw/chat_sessions` (same store as Web). `velaclaw doctor` reports undo availability. `template_dag` stays default-off.

- **SubAgent lifecycle** (VL-MA-005): dispatch/aggregate with fail-closed scope and Plan-mode pin; shared `run_tool_call_loop` (GOV-007).

- **WASM/WIT plugin sidecar** (VL-MA-006): opt-in `runtime-wasm` / `[runtime.wasm]`; `wasm_invoke` tool and `wit/velaclaw-plugin.wit` contract; default `runtime.kind` remains `native`.

- **Context Contract retrieve** (VL-MA-001): host declares `layers` / `retrieve_kind` / budget intent, fills workspace (and memory-shaped) chunks, then calls the existing `prepare_turn_history` → `MessageAssembler::assemble_layered` entry. No second assembler. Prompt P0–P3 stays in `prompt_composer`.

### Fixed

- **Web step expand** (VL-UX-STEP-002 follow-up): replace the in-flight `run` status with the result step so captions are not duplicated; show an explicit **expand** control. Local trial must rebuild `ui-chat` (`npm run build`) before `cargo build --release`, otherwise `/chat` embeds a stale SPA.

## [1.0.3] - 2026-08-15

### Changed

- **Runtime history roundtrip** (VL-REVIEW2-A2): `conversation_from_tool_loop_history` and `reintegrate_prepared_chat` live in `velaclaw-agent-runtime`. The production `run_tool_call_loop` body stays in the main crate (not VL-ARCH-011).
- **Tool-format IR repair** (VL-TTC-013): when Decode finds tool intent but no calls, run one isolated JSON extract (no executable tools) and inject allowlisted `name`/`arguments` into the shared `run_tool_call_loop`. Replaces the two actor re-ask ladder. CLI, Web, and Channel share this path. Extract miss still strips markup and shows the existing SoftFailSurface notice.
- **Turn step captions** (VL-UX-STEP-001): CLI and Web share `progress_caption` (verb + object). Progress lines no longer dump tool stdout or full shell scripts. Model status uses the logical id once (no `provider/model` double prefix). Default CLI no longer prints `── tool:… ──` result blocks.
- **Turn step expand** (VL-UX-STEP-002): default step line stays the caption. Web click/`<details>` and CLI `/expand <id>` show the same scrubbed output (capped). Approval modal is unchanged.
- **PeerContinue** (VL-TTC-014): after IR repair miss, retry the same `run_tool_call_loop` once with a catalog peer from `[[model_routes]]` (lexical pick, not cost / not `host_decide_failover`). Still StripFailClosed if the peer also misses.

## [1.0.2] - 2026-08-14

### Added

- **Turn cancel + step traces** (VL-UX-CANCEL-001): Web Chat Send becomes Stop (sends `cancel`); CLI interactive **Esc Esc** (TTY, 500ms) stops the current turn and returns to the prompt. Shared `CancellationToken` + Observer→status/step mapping; tool summaries use a distinct style from the final assistant reply. Cancelled turns are not persisted as successful replies.
- **Experimental generative inspect** (VL-GEN-001): `velaclaw models protocol-generative` and agent tool `generative_capability` declare PT-GEN keys (`image_generation` / `speech_to_text` / `text_to_speech`) against local `AI_PROTOCOL_DIR`. Omitted capability keys fail closed. No vendor HTTP; does not enable `ai-lib-rust/generative` drivers.
- **Experimental generative doctor** (VL-GEN-002): `velaclaw doctor generative [--capability] [--reachable-only] [--json]` lists PT-GEN declared/L-Exec rows plus query-time key reachability (no secrets). Does not mutate CR-CAP Tag tables.

### Changed

- **ai-lib-rust pin** (VL-TTC-012 / VL-GEN-001): git rev after 1.0.1 → `cc49a15` (bare invoke/parameter parse-aid) → `3bfda86` (ALR-GEN-002). No vendor generative HTTP.

## [1.0.1] - 2026-08-13

### Changed

- **1.0.x product shape** (VL-REL-PRODUCT-001): upgrade runbook (`docs/upgrade-1.0.md`); `/health` + dashboard expose `version` matching `velaclaw --version`; CI merge-bot fail-closed notes; bootstrap clone URL → `ailib-official/velaclaw`.
- **Ops-readonly + policy UX** (VL-UX-OPS-001 / VL-UX-POLICY-001): `examples/profiles/ops-readonly.toml` and seeded `agent-policy.yaml` (self_adjust for `autonomy.allowed_commands`); shell allowlist denials (CLI+Web) share next-step guidance; doctor maintenance points at the profile. Existing config/daemon.env never silently rewritten; SEC-009 preserved.
- **Module structure** (VL-REVIEW2-A1): split hotspots into responsibility modules — `config/schema/{mod,load}.rs`, `onboard/wizard/{mod,steps,models}.rs`, `agent/loop_/{mod,tool_loop}.rs`. Public Config/onboard/loop APIs unchanged; no second execution path (GOV-007).
- **Bootstrap unify** (VL-REVIEW2-A0 / GOV-007): CLI (`loop_::run` / `process_message`), Web (`Agent::from_config`), and Channel (`start_channels`) share `agent::assemble::assemble_runtime` for provider / memory / security / tools / dispatcher. Peripherals, ApprovalHub vs stdin, and channel listeners remain adapters only.

## [1.0.0] - 2026-08-12

### Milestone

- **GOV-007 context/tool-loop unify** (VL-CTX-001/002) + ORCH soft-fail wave + shell Always narrowing; history round-trip fix (#226). Tag **v1.0.0**.

### Added

- **Soft-fail UX + Channel surface** ([#217](https://github.com/ailib-official/velaclaw/pull/217), [#218](https://github.com/ailib-official/velaclaw/pull/218)): always-on notices when tool-format recovery exhausts or provider limit/quota hard-fails (CLI `/model`, Web picker, Channel `/models`); opt-in `[agent].host_decide_failover` advances session override along Decide fallbacks (default off).

### Changed

- **Context orchestration unify** (VL-CTX-001 / GOV-007): CLI, Web, and Channel share `context_orch::prepare_turn_history` (optional LLM compact → `assemble_layered`). `[agent].envelope_assemble` defaults to **`true`** (`false` = emergency trim-only kill-switch). Web now gets the same overflow compact path as CLI.
- **Tool-loop unify** (VL-CTX-002 / GOV-007): Web `Agent::turn` delegates tool iteration to `run_tool_call_loop`; ApprovalHub / human-input remain adapter injections (`ToolBatchGateExtras`), not a second loop body.
- **host_decide optimize honesty** (ORCH-HOST-006): keep contract values `cost` \| `latency` \| `balanced`; host embed no longer claims Eos-style `lowest_latency` / `balanced_score` or `used_cost_router=true` without real cost ranking / latency health (stub reasons instead).

### Fixed

- **Web history structured variants after VL-CTX-002** ([#226](https://github.com/ailib-official/velaclaw/pull/226)): map `run_tool_call_loop` Chat frames back to `AssistantToolCalls` / `ToolResults` instead of blanket `Chat`, restoring public `agent.history()` shape; Web soft-fail again receives `host_decide` for failover notices.
- **host_decide multi-segment logical ids** ([#216](https://github.com/ailib-official/velaclaw/pull/216)): preserve NIM/org wire segments via `compose_logical_model_id`; CAP index load failure soft-skips Decide instead of hard-failing the turn.
- **Shell Always over-broad session** (VL-SEC-009): human approval no longer bypasses `allowed_commands`; shell-policy Always remembers executable basenames only (`approval.session_shell_binaries`), and non-allowlisted commands are denied without an interactive risk prompt (single ApprovalGate path / GOV-007).

## [0.9.0] - 2026-08-07

### Milestone

- **GOV-007 Wave2**: Providers hygiene + pin `ai-lib-rust` **1.3.0**. Tag **v0.9.0**.

### Added

- **Host Decide + DAG (ORCH wave A/B, default-off)** ([#206](https://github.com/ailib-official/velaclaw/pull/206), [#208](https://github.com/ailib-official/velaclaw/pull/208), [#209](https://github.com/ailib-official/velaclaw/pull/209)):
  - `[agent].host_decide` (default `false`): CAP reachable ∩ embedded CostRouter-shaped pricing / stub; session override; CLI + Web via unified `resolve_turn_model` ladder (explicit user picks beat Decide).
  - `[agent].host_decide_optimize` (default `cost`): contract values `cost` \| `latency` \| `balanced` (host latency/balanced remain stub-grade without live latency signals — see config-reference).
  - Doctor/library DAG surfaces (`dag-view` / `dag-emit` / plan emit); **not** wired into live chat turns.
  - Observe: `velaclaw doctor host-decide --force` (`used_cost_router`, `NOT_PRODUCTION_SLA`).

### Fixed

- **GOV-007 providers / BYOK** ([#210](https://github.com/ailib-official/velaclaw/pull/210), [#211](https://github.com/ailib-official/velaclaw/pull/211)): remove glm orphan path; mark Experimental codex/python; drop BYOK double micro-retry around `AiClient::execute`.

## [0.8.0] - 2026-08-04

### Added

- **Tool-format recovery ladder** ([#199](https://github.com/ailib-official/velaclaw/pull/199), [#200](https://github.com/ailib-official/velaclaw/pull/200), [#201](https://github.com/ailib-official/velaclaw/pull/201)): corrective prompt → native-only reask → strip/fail-closed for unparsed tool markup; pin `ai-lib-rust` to ALR-TTC-016.
- **Aggregator logical model IDs** ([#202](https://github.com/ailib-official/velaclaw/pull/202)): compose `provider/wire` at protocol registry ingest so NIM-style org-qualified catalog keys bind under the host provider; remap bare wire ids in chat overrides.
- **GFM Markdown in Web Chat** ([#203](https://github.com/ailib-official/velaclaw/pull/203)): `marked` + DOMPurify for headings, tables, lists, and fenced code; terminate GFM tables before following paragraphs when models omit a blank line.

### Changed

- **Dependencies**: pin public `ailib-official/ai-lib-rust` to `ddee3ce` (ALR-TTC-016). Runtime protocol data continues to come from a local `AI_PROTOCOL_DIR` checkout of public [`ailib-official/ai-protocol`](https://github.com/ailib-official/ai-protocol) (not vendored into this release tag).

## [0.7.4] - 2026-07-11

### Changed

- **Supervised shell approval**: allowlist and medium/high-risk blocks now support interactive Y / A / N approval on CLI; injection-style constructs remain hard-denied.
- **Privilege hints**: sudo/su/root commands show config and approval guidance in CLI prompts and policy errors.
- **Protocol registry**: leniently index `metadata.models` from manifests missing strict V2 fields (e.g. azure v1); dedupe provider stems by priority.
- **Dependencies**: bump `ai-lib-rust` 1.0.1 → 1.1.0.

## [0.7.3] - 2026-07-11

### Added

- **Manifest-driven prompt budget** ([#130](https://github.com/ailib-official/velaclaw/pull/130)): `context_window` from ai-protocol scales system-prompt char budget (~15% of context, 4k–48k clamp); `compact_context` caps at 24k.
- **Heartbeat/Cron prompt phases** ([#131](https://github.com/ailib-official/velaclaw/pull/131)): dedicated overlays for daemon heartbeat tasks and cron agent jobs.

### Changed

- **CLI approval prompts** ([#131](https://github.com/ailib-official/velaclaw/pull/131)): unified `🔒 Security policy requires approval...` presentation for supervised tools.
- **Docs**: `compact_context` and CLI approval behavior in config reference (EN/VI) and policy-approval reference.

## [0.7.2] - 2026-07-11

### Added

- **Pyramid system-prompt composer** ([#126](https://github.com/ailib-official/velaclaw/pull/126)): P0–P3 tier assembly with Full/Minimal/Headline modes; headline-first truncation under char budget.
- **Phase-specific prompt sections** ([#127](https://github.com/ailib-official/velaclaw/pull/127)): Execute, Approval, Compact, and Delegate phases wired into agent loop, channel start, history compaction, and delegate subagents; `compact_context` applies ~24k system-prompt budget.

### Changed

- **CLI REPL formatting** ([#124](https://github.com/ailib-official/velaclaw/pull/124), [#125](https://github.com/ailib-official/velaclaw/pull/125)): `>` / `>>` speaker prefixes with `>>` only on the first line of multi-line agent replies; richer box tables.
- **Onboard workspace templates** ([#128](https://github.com/ailib-official/velaclaw/pull/128)): `AGENTS.md` / `SOUL.md` / `TOOLS.md` clarify separation from built-in system prompt; `compact_context` budget documented in config reference.

## [0.7.1] - 2026-07-10

### Changed

- **VL-REVIEW remediation**: module-level splits and shared bootstrap without behavior changes to the public CLI/config contract.
  - `channels/mod.rs` orchestration extracted to `runtime` / `dispatch` / `start` / `prompt` / `doctor` (entry ≤800 lines).
  - CLI `main()` and `gateway::run_gateway()` slimmed via `cli_dispatch`, `status`, webhook/http router helpers.
  - Agent/Gateway share `config::bootstrap_runtime`; default model fallbacks use `DEFAULT_PROTOCOL_MODEL_ID`.
  - Onboard wizard prefers ai-protocol manifest catalog with curated offline fallback.
  - Prism provider ordering respects `reliability.fallback_providers`.
  - Memory `auto_save` failures emit `tracing::warn!` instead of silent discard.
- Test isolation: config-resolution / quick-setup paths clear or pin `VELACLAW_CONFIG_DIR` under a shared lock so host env and sandboxes do not leak into unit tests.

## [0.7.0] - 2026-07-09

### Added

- **Unified policy layers (VL-SEC-001)**: L1 `config.toml` + L2 `agent-policy.yaml` v2 + L2.5 `.velaclaw/policy-overrides.yaml` merge via `EffectiveExecutionPolicy`.
- **ApprovalGate (VL-SEC-002)**: single human approval path for CLI, gateway, and channels; shell tool schema no longer exposes model-writable `approved`.
- **Channel inline approval (VL-SEC-003)**: Telegram/Discord supervised tool prompts via `approval_mode` (`inline` / `deny` / `gateway_redirect`).
- **Policy persistence (VL-SEC-004)**: operator **Always** decisions and audit trail persist to L2.5; session allowlist survives restart.
- **`policy_patch` tool (VL-SEC-005)**: `self_adjust` glob-enforced dot-path patches with `PolicyHandle` hot refresh ([#117](https://github.com/ailib-official/velaclaw/pull/117)).
- **`execute_tool_batch` (VL-UR-003 / VL-SEC-006)**: gate-aware parallel/sequential tool dispatch shared by CLI/channel/gateway loops ([#118](https://github.com/ailib-official/velaclaw/pull/118)).
- **`DenyApprovalBackend`**: runtime backend for channel deny profiles (exported; gate wiring follow-up as needed).

### Changed

- **Breaking**: models cannot self-approve shell via `approved` parameter; human consent is injected only after `ApprovalGate` approval.
- **Breaking**: channel supervised tools require configured `approval_mode`; legacy bypass paths removed.
- `Agent::execute_tool_call` now reports accurate `success` from tool execution results.

### Documentation

- **Policy & approval runtime reference (VL-SEC-007)**: `docs/policy-approval-reference.md`, `docs/migration-policy-v0.7.0.md`; updated config/channels/runbook/troubleshooting ([#119](https://github.com/ailib-official/velaclaw/pull/119)).

## [0.6.0] - 2026-07-09

### Added

- **CLI render layer (VL-CLI-RENDER-001..003)**: terminal Markdown→ANSI/box rendering with CJK-aware width (`unicode-width`), TTY/`NO_COLOR` detection, and interactive long-output folding.
- **`[cli_render]` config**: optional `fold_lines` (default `10`) and `markdown_enabled` (default `true`); absent section keeps backward-compatible defaults.
- **`velaclaw agent --no-color` / `--no-fold`**: force plain output and disable REPL folding.
- **REPL `/expand <id>`**: replay a folded tool/code block without re-rendering.

### Changed

- CLI `CliChannel::send` and agent loop print sites now route through `cli_render::render` / `RenderOpts` (non-TTY stays pipe-friendly plain with box-drawing preserved).

## [0.5.1] - 2026-07-05

### Added

- **TTC runtime chain (VL-TTC-003, #91)**: manifest `tool_calling` → `ToolCallingPolicy` → `ToolDispatcher` wired through `Agent::turn` and CLI `ExecutionHandle`.
- **Unified tool loop (VL-TTC-004, #92)**: channels, delegate, and `velaclaw agent` CLI share `build_tool_dispatcher()` / manifest-aware `ToolDispatcher`; CLI handles `/models` and `/model` slash commands locally.
- **DSML parameter aliases**: map common model variants (`file_path` → `path`, `cmd` → `command`) before tool execution.
- **CLI shell approval prompt**: interactive `approved=true` when security policy requires explicit consent in terminal sessions.

### Fixed

- **Telegram interrupt race**: newer in-flight task wins when message dispatch interrupts an active request under `ai-protocol`.
- **DeepSeek DSML**: multi-block DSML stripping via ai-lib-rust 1.0.1 dependency.

### Dependencies

- Bumped `ai-lib-rust` git pin to **1.0.1** (`4794d3c`).

### Changed

- **Breaking / ZS-ML-015:** removed the `legacy-providers` Cargo feature and built-in HTTP provider factory. Use `provider/model` ids backed by ai-protocol manifests; `custom:` / `anthropic-custom:` URL syntaxes now return migration errors.
- **PT-074 BYOK availability:** protocol provider discovery now delegates credential availability to ai-lib-rust's unified credential chain (endpoint.auth, V1 auth, conventional env fallback, and keyring when enabled) instead of maintaining a VelaClaw-only env scan.
- **Default Cargo features** now include only `ai-protocol`, aligning with the ai-lib migration plan’s protocol-first default.

### Dependencies

- Bumped `ai-lib-rust` to **0.9.4** (optional; `--features ai-protocol`).
- Added optional `async-stream` and `serde_yaml` for the protocol adapter and manifest registry.

### Added

- **`ai-protocol`**: protocol-backed providers, `protocol_registry` scan, and `velaclaw models protocol-providers` / `protocol-models` CLI.
- Quick setup warns when `provider/model` is used without a usable local `AI_PROTOCOL_DIR`.
- **`routing_mvp`** optional feature: forwards `ai-lib-rust/routing_mvp` for experimental routing; CI runs a `cargo check` compile gate with `ai-protocol` (ZS-ML-004).

### Documentation

- **`docs/migration-legacy-to-protocol.md`**: maps legacy string keys / `custom:` URLs to `provider/model` + `AI_PROTOCOL_DIR` and documents the ZS-ML-015 removal.
- **`docs/ai-lib-migration.md`**: compatibility window (pre-1.0 cadence, ai-protocol pin policy) (ZS-ML-006).
- **`docs/ai-lib-migration.md`**: resilience boundaries — transport retry in `ProtocolBackedProvider` vs `[reliability]` fallback chains.
- **`docs/ai-lib-migration.md`**: Phase 2 — minimal TOML for `provider/model` logical ids and `[reliability]` fallbacks (ZS-ML-003).
- **`docs/ai-lib-migration.md`**: optional `routing_mvp` feature and `ProtocolBackedProvider` note on transport retry vs app-level fallbacks (ZS-ML-004).

### Testing

- **PT-074 BYOK smoke:** protocol registry unit tests cover V2 endpoint.auth.token_env and conventional env fallback availability.
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
- **Dependencies** (ZS-ML-009, PR #21): stop enabling unused `ai-lib-rust` Cargo features `embeddings`, `batch`, and `telemetry` until VelaClaw has real call sites; document deferral and the `observability-otel` vs ai-lib metrics boundary in migration docs.

### Testing (ai-lib / ai-protocol rectification, 2026-04)

- **CI** (ZS-ML-008, PR #20): run `cargo test -p velaclaw --no-default-features --features ai-protocol` (in addition to the manifest-only `cargo check` gate).
- **Resilience** (ZS-ML-008, PR #20): `ReliableProvider` unit tests for protocol-style logical model fallbacks and for app-layer retry budgeting vs inner transport retries.

## [0.3.0] - 2026-02-23

### Added
- **Remote Deployment Feature**: Complete SSH-based remote deployment system for VelaClaw
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
- **User Guide**: Aligned docs to VelaClaw branding (VelaClaw → VelaClaw, velaclaw → velaclaw, ~/.velaclaw → ~/.velaclaw)

## [0.1.1] - 2026-02-21

### Added
- **Project Fork**: VelaClaw forked from [VelaClaw](https://github.com/velaclaw-labs/velaclaw) with enhanced features
- **Raspberry Pi Support**: Cross-compilation for aarch64-unknown-linux-gnu target (64-bit ARM)
- **Upstream Sync Script**: `sync-upstream.sh` for tracking velaclaw-labs/velaclaw main branch
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
- **Project Name**: Renamed from VelaClaw to VelaClaw
- **License**: Dual MIT OR Apache-2.0 license
- **Author**: Luqiang Wang
- **Repository**: https://github.com/ailib-official/velaclaw

### Security
- **Cron Tools**: Security policy now passed to cron tools in registry

### Documentation
- Restored AGENTS.md and CLAUDE.md as functional documentation
- Updated README with VelaClaw branding

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

[0.3.0]: https://github.com/ailib-official/velaclaw/releases/tag/v0.3.0
[0.2.0]: https://github.com/ailib-official/velaclaw/releases/tag/v0.2.0
[0.1.1]: https://github.com/ailib-official/velaclaw/releases/tag/v0.1.1
[0.1.0]: https://github.com/theonlyhennygod/velaclaw/releases/tag/v0.1.0
