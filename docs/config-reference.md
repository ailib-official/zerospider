# VelaClaw Config Reference (Operator-Oriented)

This is a high-signal reference for common config sections and defaults.

Last verified: **February 19, 2026**.

Config path resolution at startup:

1. `VELACLAW_WORKSPACE` override (if set)
2. persisted `~/.velaclaw/active_workspace.toml` marker (if present)
3. default `~/.velaclaw/config.toml`

**Path layout (VL-MA-001):** config/credentials live in the config directory (`VELACLAW_CONFIG_DIR` / `~/.velaclaw`). Agent **workspace** is a separate tree (`workspace_dir`). Web chat sessions are stored at `<workspace_dir>/.velaclaw/chat_sessions` — inside the workspace, not in the config directory.

VelaClaw logs the resolved config on startup at `INFO` level:

- `Config loaded` with fields: `path`, `workspace`, `source`, `initialized`

Schema export command:

- `velaclaw config schema` (prints JSON Schema draft 2020-12 to stdout)

## Core Keys

| Key | Default | Notes |
|---|---|---|
| `default_provider` | `openrouter` | provider ID or alias |
| `default_model` | `anthropic/claude-sonnet-4-6` | model routed through selected provider |
| `default_temperature` | `0.7` | model temperature |

## `[observability]`

| Key | Default | Purpose |
|---|---|---|
| `backend` | `none` | Observability backend: `none`, `noop`, `log`, `prometheus`, `otel`, `opentelemetry`, or `otlp` |
| `otel_endpoint` | `http://localhost:4318` | OTLP HTTP endpoint used when backend is `otel` |
| `otel_service_name` | `velaclaw` | Service name emitted to OTLP collector |

Notes:

- `backend = "otel"` uses OTLP HTTP export with a blocking exporter client so spans and metrics can be emitted safely from non-Tokio contexts.
- Alias values `opentelemetry` and `otlp` map to the same OTel backend.

Example:

```toml
[observability]
backend = "otel"
otel_endpoint = "http://localhost:4318"
otel_service_name = "velaclaw"
```

## Environment Provider Overrides

Provider selection can also be controlled by environment variables. Precedence is:

1. `VELACLAW_PROVIDER` (explicit override, always wins when non-empty)
2. `PROVIDER` (legacy fallback, only applied when config provider is unset or still the protocol default `openai/gpt-5.2`)
3. `default_provider` in `config.toml`

Operational note for container users:

- If your `config.toml` sets an explicit provider/model id like `local-gateway/my-model`, a default `PROVIDER=openai/gpt-5.2` from Docker/container env will no longer replace it.
- Use `VELACLAW_PROVIDER` when you intentionally want runtime env to override a non-default configured provider.

## `[agent]`

### Turn-model wiring matrix

Same `[agent]` keys must not silently mean different things on different shells. Turn model resolution for **CLI** (`velaclaw agent`) and **Web** (`Agent::turn` via `/chat`, `/api/chat`, `/ws`) shares one ladder (`orchestration::resolve_turn_model`):

1. **Explicit user pick** — CLI `-p` / `--model`, Web `model_id`, or process session override (must be CAP-reachable when the index is available; otherwise fail closed)
2. **`host_decide`** (if enabled) — CAP reachable ∩ embedded CostRouter-shaped pricing
3. **`intent_capability_route`** (if enabled) — Tag/Hint → CAP reachable ∩ `[[model_routes]]`
4. **`query_classification`** / configured **`default_model`**

| Surface | Uses `resolve_turn_model` | Notes |
|---|---|---|
| CLI `velaclaw agent` | Yes | `-p/--model` counts as explicit |
| Web Local Control / `/chat` | Yes | Picker `model_id` counts as explicit (beats `host_decide`) |
| Channels (Telegram/Discord/…) | No | Use channel `route.model` only (documented; not ORCH parity yet) |
| Doctor observe | Independent | `--force` bypasses live flags |

**Shared pre-turn (CLI + Web + Channel):** `resolve_turn_model` (CLI/Web), **`context_orch::prepare_turn_history`** (compact + `assemble_layered`; VL-CTX-001 / GOV-007), and L2 `agent-policy.yaml` tool_dispatcher merge. **Shared tool loop (VL-CTX-002):** `run_tool_call_loop` is the single iteration body; Web injects ApprovalHub/HITL via gate extras (adapters, not a second policy). CLI stdin uses the same `ApprovalGate`; cron/heartbeat jobs reuse this loop with channel names `cron`/`heartbeat` (no stdin elevation). **Shared bootstrap (VL-REVIEW2-A0 / GOV-007):** `agent::assemble::assemble_runtime` is the canonical Config → provider/memory/security/tools/dispatcher entry for CLI, Web, and Channel hosts.

DAG-related keys below: **`template_dag` / candidate emit / shadow** stay library/doctor (AI-DAG frozen off the default turn). **`bounded_dag_live` is a separate opt-in** for a handwritten linear L2 graph on CLI + Web (not L4 emit).

| Key | Default | Purpose |
|---|---|---|
| `compact_context` | `false` | When true: `bootstrap_max_chars=6000`, `rag_chunk_limit=2`, and system-prompt budget capped at ~24k chars (pyramid truncation drops ambient sections first). When false, budget still scales from ai-protocol `context_window` when available (~15% of context, clamped 4k–48k chars). Use for smaller context windows |
| `max_tool_iterations` | `10` | Maximum tool-call loop turns per user message across CLI, gateway, and channels |
| `max_history_messages` | `50` | Maximum conversation history messages retained per session |
| `parallel_tools` | `false` | Enable parallel tool execution within a single iteration |
| `tool_dispatcher` | `auto` | Tool dispatch strategy |
| `envelope_assemble` | `true` | **VL-CTX-001 (normative):** run ai-lib `assemble_layered` via `prepare_turn_history` before each turn on CLI, Web, and channel dispatch. HardBudgetViolation fails the turn explicitly. Set `false` only as an emergency kill-switch (falls back to message-count trim). Requires `--features ai-protocol`. |
| `envelope_assemble_async` | `false` | **CR-L3-003 (opt-in):** when `envelope_assemble` is true, schedule assemble via ai-lib `AssemblePool` / `assemble_layered_async`. Default remains **off** (sync algorithm path). Requires `--features ai-protocol`. |
| `template_dag` | `false` | **Reserved / unused live gate.** CR-L2 `agent::dag_runner` APIs exist for library + doctor; this bool is **not read** by CLI/Web/channel turns. Observe with `velaclaw doctor template-dag --fixture <path>`. No LLM DAG generation. Requires `--features ai-protocol`. |
| `bounded_dag_live` | `false` | **VL-NA-011/015/020/025/026/028/029/030 (opt-in).** First hop is in-band `chat_only` or a 1–8 node linear DAG. Tools always run as DAG nodes (a `single_work` label or invalid JSON becomes a 1-node graph, then one split-refine chat). Never silent `chat_only`. Live hops use the session default model (Web picker is ignored). Work hops are not CostRouter. After each hop, observe may `replan_remaining` once (15s timeout fail-open). Session turns always run `prepare_turn_history` before the first hop (VL-CTX-001); VL-NA-019 skip is Plan-phase preview only. Dist default off. Not L4 `candidate_dag_emit`. |
| `bounded_dag_path` | *(empty)* | Non-empty JSON path **skips the planner** (operator-fixed graph). Empty → planner; invalid / non-linear JSON → embedded `code-fix-template` (`locate` → `patch` → `verify`). |
| `candidate_dag_shadow` | `false` | **Library/doctor gate (CR-L4-003).** When true, `maybe_run_candidate_shadow` may run for callers that invoke it. **Not wired** into live chat. Observe anytime via `velaclaw doctor candidate-dag`. Requires `--features ai-protocol`. |
| `candidate_dag_stagnation_limit` | `0` | **CR-L4-003:** optional consecutive assemble-output hash limit for shadow library runs (`0` = off). |
| `intent_capability_route` | `false` | **CR-CAP-005 (opt-in; CAP-003 wire):** live on CLI + Web via `resolve_turn_model` (after explicit pick / `host_decide`). Tag/Hint → host capability index → **reachable (local keys)** ∩ `[[model_routes]]`. Serde alias: `capability_index_route`. Empty reachable sets **fail closed**. Channels remain route-table only. Observe: `doctor capabilities` → `doctor capability-route --tag <Tag> --force`. Requires `--features ai-protocol`. |
| `host_decide` | `false` | **ORCH-HOST-001/002/003 (opt-in):** live on CLI + Web via `resolve_turn_model`. CAP reachable ∩ host Decide (embedded pricing; stub if unavailable). Preserves full multi-segment logical ids via `compose_logical_model_id` (e.g. `nvidia/deepseek-ai/...`). CAP index load failure **soft-skips** Decide (ladder continues). **Explicit user picks beat Decide.** Optimize via `host_decide_optimize`. Observe: `velaclaw doctor host-decide --force` (prints `used_cost_router`). Requires `--features ai-protocol`. |
| `host_decide_optimize` | `cost` | Optimize goal when `host_decide` is enabled: `cost` \| `latency` \| `balanced` (Decide contract; keep all three for Eos/prism-core parity). **Host honesty:** only `cost` may set `used_cost_router=true` and reason `lowest_cost` when priced. `latency` / `balanced` stay accepted config values but emit stub reasons (`host_reachable_latency_stub` / `host_reachable_balanced_stub`) until live latency health exists — never claim Eos-style `lowest_latency` / `balanced_score` on the host embed. Prefer `cost` until host latency signals exist (Eos/prism-core CostRouter can use `ProviderHealth` when available). |
| `host_decide_failover` | `false` | **ORCH-HOST-004 (opt-in):** when `host_decide` is true, tool-format recovery exhaustion or provider limit/quota hard-fail may set a process-local session override to the next Decide fallback logical id for the **next** turn, and append a user-visible notice. Always-on notices (switch hint) apply even when this is false. Requires `--features ai-protocol`. |
| `hint_peer_fallback` | `false` | **VL-NA-021 (opt-in).** After ReliableProvider micro-retry, try `[[model_routes]].fallbacks` for the same hint (max 5 attempts, 3 cross-provider). Success pins the hint for the rest of the session and blacklists the failed model id. Dist default off. Hop errors are classified (`unavailable` includes HTTP 410 and vendor HTTP 404 Function Not Found; not workspace file-not-found). DNS/transport does not switch. |
| `dag_fail_auto_replan` | `false` | **VL-NA-024 (opt-in).** After a live DAG work-node fail, retry that node once in the same turn. Unavailable/quota hops continue on the session default model; policy/other retry the same Contact; transport still stops. Dist default off. |
| `candidate_dag_emit` | `false` | **Library/doctor gate (ORCH-DAG-EMIT-001/002).** Schema-strict / LLM plan→emit helpers. **Not wired** into live chat. Observe: `velaclaw doctor dag-emit` / `velaclaw doctor dag-plan --force`. Requires `--features ai-protocol`. |

**See the linear DAG without changing the shipped default:** keep `bounded_dag_live = false` in dist. For a **local vision proof** (trial), set `bounded_dag_live = true`, empty `bounded_dag_path`, and `[[model_routes]]` for the three families you have keys for:

| Hint | Capability tags | Family (this proof) | Logical model |
|---|---|---|---|
| *(planner / default)* | — | DeepSeek | `deepseek/deepseek-v4-flash` |
| `document` / `code` / `tools` | document_understanding, coding, tool_calling | DeepSeek | `deepseek/deepseek-v4-flash` |
| `fast` | speed | Groq | `groq/openai/gpt-oss-20b` |
| `reasoning` | high-reasoning | NVIDIA NIM | `nvidia/nvidia/llama-3.1-nemotron-70b-instruct` |

PT-NIM-002: `nvidia/llama-3.3-nemotron-super-49b-v1.5` is **EOL** (NIM HTTP 410). Catalog `deprecated.maps_to` still lists the id. Host `create_routed_provider` rewrites tombstoned route primaries **before** the first HTTP hop (VL-NA-027). Dist still ships `hint_peer_fallback = false`. Trial should pin `reasoning` to the successor, not the tombstone.

Planner stays on session `default_model` (not `host_decide`). Invalid planner JSON still falls back to `locate` → `patch` → `verify`. Web Plan/Build send `host_phase`. This is not L4 emit.

Host-local capability discovery (CR-CAP-002/004, no config key): `velaclaw doctor capabilities [--tag <Tag>] [--rebuild] [--reachable-only]` builds a Tag→candidates **fact** cache at `<config_dir>/capability-index.json` from `$AI_PROTOCOL_DIR`, then applies a **query-time** reachable filter (local API keys / keyless providers — never stores secrets in the cache). This is **not** written into public ai-protocol manifests. Live selection uses the same reachable view when `[agent].host_decide` or `[agent].intent_capability_route` is enabled. Operator narrative: **capability-index routing** (not intent-product); CAP-003 remains trial wire.

`tool_dispatcher` values:

- `auto` (default): resolve via provider manifest `tool_calling` and `ExecutionHandle::tool_calling_policy()`. When the provider supports native tools and the manifest strategy prefers native (e.g. DeepSeek hybrid), `NativeToolDispatcher` is used; otherwise `XmlToolDispatcher`.
- `native`: always use `NativeToolDispatcher` (manifest-driven text parser, native `tool_calls` when reliable).
- `xml`: always use `XmlToolDispatcher` (text/XML tool calls only; no native tool specs on the wire).

### L2 — `agent-policy.yaml` (workspace)

Place `agent-policy.yaml` at the project root (or `workspace/agent-policy.yaml`). Discovery walks up from the current working directory and honors `VELACLAW_WORKSPACE`. Missing file means no L2 layer.

Merge priority for `tool_calling.dispatcher`: **session override** > `[agent].tool_dispatcher` (L1 `config.toml`) > L2 `agent-policy.yaml` > `auto`.

Example:

```yaml
version: 1
tool_calling:
  dispatcher: auto
self_adjust:
  allowed_writes:
    - memory.preferences.tone
  denied_writes:
    - security.*
    - channels.*.credentials
```

**Version 2** adds optional `autonomy` and `approval` override blocks (merged over L1 `[autonomy]`). See [policy-approval-reference.md](policy-approval-reference.md).

### L2.5 — `.velaclaw/policy-overrides.yaml` (workspace)

Persistent operator/agent policy patches live under:

```
<workspace>/.velaclaw/policy-overrides.yaml
```

Created automatically when:

- An operator chooses **Always** on a supervised tool prompt (appends to `approval.session_allowlist`).
- An operator chooses **Always** on a shell-policy risk prompt (appends executable basename to `approval.session_shell_binaries`; does not widen `allowed_commands`).
- The agent invokes the `policy_patch` tool with an allowed dot-path (`self_adjust` globs in L2).

Merged **after** L2 `agent-policy.yaml` when computing effective autonomy/approval. Autonomy patches hot-reload via `PolicyHandle` without process restart. `session_shell_binaries` is hydrated into runtime session state (not into `auto_approve`).

Example:

```yaml
version: 1
approval:
  session_allowlist:
    - file_write
  session_shell_binaries:
    - curl
autonomy:
  allowed_commands:
    - git
    - cargo
    - curl
```

See [policy-approval-reference.md](policy-approval-reference.md) and [migration-policy-v0.7.0.md](migration-policy-v0.7.0.md).

## `[cli_render]`

Optional. When omitted, runtime uses the same defaults as an explicit section.

| Key | Default | Purpose |
|---|---|---|
| `fold_lines` | `10` | Visible lines kept when folding long tool/code blocks in interactive REPL |
| `markdown_enabled` | `true` | When `false`, skip Markdown structure rendering (fenced blocks still dropped in plain mode) |

Notes:

- Folding applies only in interactive `velaclaw agent` (not one-shot `-m` / pipe / CI).
- Override with CLI: `velaclaw agent --no-color` / `--no-fold`.
- Env: `NO_COLOR` disables ANSI regardless of TTY.

```toml
[cli_render]
fold_lines = 10
markdown_enabled = true
```

Secret fields (`api_key`, `token`, `credentials`, etc.) are rejected at load time.

Notes:

- Setting `max_tool_iterations = 0` falls back to safe default `10`.
- If a channel message exceeds this value, the runtime returns: `Agent exceeded maximum tool iterations (<value>)`.
- In CLI, gateway, and channel tool loops, multiple independent tool calls are executed concurrently by default when the pending calls do not require approval gating; result order remains stable.
- `parallel_tools` applies to the `Agent::turn()` API surface (Web Local Control). CLI and channel handlers use their own concurrent batching when calls do not require approval gating.

## `[security]`

User-facing **profile** (VL-SEC-011) plus optional env inherit. Profiles **unfold** existing knobs; they do not add a second tool loop (GOV-007). Unset `profile` leaves sandbox/autonomy as written.

| Key | Default | Purpose |
|---|---|---|
| `profile` | unset | `isolated` — Landlock/fail-closed, scrubbed shell env, credential paths **Ask** (Once). `local` — Noop sandbox, inherit daemon env, do not treat `/home` as a file ban, credential paths **Allow** (output scrubbed; secrets can enter the model). `readonly` — `autonomy.level = read_only`. |
| `inherit_process_env` | unset | Override: `true` copies daemon env into every shell child inside `apply_shell_child_env`. Unset + `profile = local` implies true. Isolated keeps `env_clear` + functional allowlist. |

```toml
[security]
# profile = "isolated"
# inherit_process_env = false
```

`velaclaw doctor` prints `security.profile=` and whether inherit is on.

## `[security.sandbox]`

OS isolation for production `shell` (wired in `all_tools_with_runtime`). Unit tests that construct `ShellTool::new` keep Noop.

| Key | Default | Purpose |
|---|---|---|
| `enabled` | unset (auto) | `false` is YOLO: Noop sandbox. Unset/`true` follows `backend`. |
| `backend` | `auto` | Linux `auto`: Landlock when the `sandbox-landlock` feature and kernel support it, otherwise **fail-closed** (shell refused). **Non-Linux `auto` stays Noop** (no Landlock; other OS backends are not the default). `none` is YOLO Noop. Explicit `landlock` / `firejail` / `docker` / `bubblewrap`: missing backend is fail-closed, not silent Noop. |
| `firejail_args` | `[]` | Extra args when `backend = "firejail"` |
| `escape_on_approval` | `false` | **Policy B (opt-in):** after human ApprovalHub/CLI approval, that shell invocation skips OS sandbox wrap (Landlock/`no_new_privs`). Default **false** keeps beginners sandboxed (policy A). Does not widen `allowed_commands`. When enabled, `sudo`/`apt`/`apt-get`/`dpkg` (and privilege hints) require approval even under `autonomy.level = full`. |

Notes:

- **Migration:** after upgrading a Linux install, run `velaclaw doctor` and check `sandbox=… production_path=`. If you see `fail-closed`, either enable Landlock or set YOLO explicitly — otherwise **all shell is refused**, including allowlisted commands.
- Allowlisted commands still go through `Sandbox::wrap_command` unless `escape_on_approval` and the invocation is human-approved.
- Human approval does **not** enlarge `allowed_commands` (SEC-009).
- Autonomy `full` does not disable the sandbox.
- Landlock applies in the child (`pre_exec`), not the agent parent.
- Receipts: `<workspace>/.velaclaw/tool_receipts.jsonl` (truncated command; no secrets). Approved escapes record `sandbox=none(approved-escape)`.
- Shell `tool_result` errors use classified prefixes on the existing `error` string (GOV-007, no second API): `[policy_deny]`, `[needs_approval]`, `[sandbox_deny]`. Models must not retry equivalent `ls`/`find`/`cat` after `[sandbox_deny]`/`[policy_deny]`. This is the public default contract; do not document `escape_on_approval = true` as the onboard path.
- Known credential **basenames** (`github_token_list.txt`, `id_rsa`, `id_ed25519`, …): argv tokens, workspace `bash`/`sh` script bodies, and `file_read`/`file_write` paths share the same detector. Unset `profile` still hard-denies even when approved. `isolated` asks Once (Always is not persisted for those paths). `local` allows under `allowed_commands` (scrubbed). Substring false positives (`raid_rsa`) are not matched. ApprovalGate denials append `tool_receipts.jsonl` `deny` lines. `ghp_` / `github_pat_` literals are redacted in receipts and tool output.
- **Isolated GitHub CLI:** with `env_clear`, put `GH_TOKEN` or `GITHUB_TOKEN` in the daemon `EnvironmentFile` (`daemon.env`, mode `600`). Those names are restored **only** when the first policy segment is `gh` / `gh.exe`. The child also gets `GH_CONFIG_DIR` under the workspace. `profile = local` inherits daemon env and can use real `~/.config/gh` instead. Prefer `gh`; do not scan PAT/key lists. Isolated credential paths use the ApprovalHub Once modal, not `request_human_input`.

```toml
[security.sandbox]
# Default Auto. YOLO opt-out:
# enabled = false
# backend = "none"
# Power-user sudo/apt (after ApprovalHub Yes):
# escape_on_approval = true
```

## `[agents.<name>]`

Delegate sub-agent configurations. Each key under `[agents]` defines a named sub-agent that the primary agent can delegate to.

| Key | Default | Purpose |
|---|---|---|
| `provider` | _required_ | Provider name (e.g. `"ollama"`, `"openrouter"`, `"anthropic"`) |
| `model` | _required_ | Model name for the sub-agent |
| `system_prompt` | unset | Optional system prompt override for the sub-agent |
| `api_key` | unset | Optional API key override (stored encrypted when `secrets.encrypt = true`) |
| `temperature` | unset | Temperature override for the sub-agent |
| `max_depth` | `3` | Max recursion depth for nested delegation |
| `agentic` | `false` | Enable multi-turn tool-call loop mode for the sub-agent |
| `allowed_tools` | `[]` | Tool allowlist for agentic mode |
| `max_iterations` | `10` | Max tool-call iterations for agentic mode |

Notes:

- `agentic = false` preserves existing single prompt→response delegate behavior.
- `agentic = true` requires at least one matching entry in `allowed_tools`.
- Allowlisted names must exist on the parent registry. Unknown names and nested `delegate` fail closed (the child cannot escalate).
- Agentic runs use the same host tool-loop; dispatch/aggregate lines include a `run_id`.

```toml
[agents.researcher]
provider = "openrouter"
model = "anthropic/claude-sonnet-4-6"
system_prompt = "You are a research assistant."
max_depth = 2
agentic = true
allowed_tools = ["web_search", "http_request", "file_read"]
max_iterations = 8

[agents.coder]
provider = "ollama"
model = "qwen2.5-coder:32b"
temperature = 0.2
```

## `[runtime]`

| Key | Default | Purpose |
|---|---|---|
| `kind` | `native` | Shell runtime adapter: `native`, `docker`, or `wasm` (no shell). Default stays `native`. |
| `reasoning_enabled` | unset (`None`) | Global reasoning/thinking override for providers that support explicit controls |

Notes:

- `reasoning_enabled = false` explicitly disables provider-side reasoning for supported providers (currently `ollama`, via request field `think: false`).
- `reasoning_enabled = true` explicitly requests reasoning for supported providers (`think: true` on `ollama`).
- Unset keeps provider defaults.
- Do **not** set `kind = "wasm"` to load plugins: that adapter has no shell. Plugins are a sidecar under `[runtime.wasm]`.

## `[runtime.wasm]`

| Key | Default | Purpose |
|---|---|---|
| `enabled` | `false` | Register the `wasm_invoke` tool (GOV-006: off by default) |
| `tools_dir` | `tools/wasm` | Workspace-relative directory of `*.wasm` modules |
| `fuel_limit` | `1000000` | Interpreter fuel budget (`0` = unlimited; not recommended) |
| `memory_limit_mb` | `64` | Guest memory ceiling in MiB |
| `allow_workspace_read` | `false` | Reserved; WASI filesystem is not mapped in this release |
| `allow_workspace_write` | `false` | Reserved; WASI filesystem is not mapped in this release |
| `allowed_hosts` | `[]` | Reserved; guest HTTP is not wired in this release |

Notes:

- Guest ABI is `wit/velaclaw-plugin.wit`: core WebAssembly export `run() -> s32`, interpreted by **wasmi**. This is **not** a wasmtime Component Model runtime.
- Execution requires a build with `--features runtime-wasm`. Without the feature, `wasm_invoke` still registers when `enabled = true` but module execution fails closed.
- `wasm_invoke` uses the existing tool loop and Plan mutating pin (blocked in Plan). Module names are ASCII alphanumeric / `_` / `-` only.

## `[skills]`

| Key | Default | Purpose |
|---|---|---|
| `open_skills_enabled` | `false` | Opt-in loading/sync of community `open-skills` repository |
| `open_skills_dir` | unset | Optional local path for `open-skills` (defaults to `$HOME/open-skills` when enabled) |
| `prompt_injection_mode` | `full` | Skill prompt verbosity: `full` (inline instructions/tools) or `compact` (name/description/location only) |

Notes:

- Security-first default: VelaClaw does **not** clone or sync `open-skills` unless `open_skills_enabled = true`.
- Environment overrides:
  - `VELACLAW_OPEN_SKILLS_ENABLED` accepts `1/0`, `true/false`, `yes/no`, `on/off`.
  - `VELACLAW_OPEN_SKILLS_DIR` overrides the repository path when non-empty.
  - `VELACLAW_SKILLS_PROMPT_MODE` accepts `full` or `compact`.
- Precedence for enable flag: `VELACLAW_OPEN_SKILLS_ENABLED` → `skills.open_skills_enabled` in `config.toml` → default `false`.
- `prompt_injection_mode = "compact"` is recommended on low-context local models to reduce startup prompt size while keeping skill files available on demand.

## `[composio]`

| Key | Default | Purpose |
|---|---|---|
| `enabled` | `false` | Enable Composio managed OAuth tools |
| `api_key` | unset | Composio API key used by the `composio` tool |
| `entity_id` | `default` | Default `user_id` sent on connect/execute calls |

Notes:

- Backward compatibility: legacy `enable = true` is accepted as an alias for `enabled = true`.
- If `enabled = false` or `api_key` is missing, the `composio` tool is not registered.
- VelaClaw requests Composio v3 tools with `toolkit_versions=latest` and executes tools with `version="latest"` to avoid stale default tool revisions.
- Typical flow: call `connect`, complete browser OAuth, then run `execute` for the desired tool action.
- If Composio returns a missing connected-account reference error, call `list_accounts` (optionally with `app`) and pass the returned `connected_account_id` to `execute`.

## `[cost]`

| Key | Default | Purpose |
|---|---|---|
| `enabled` | `false` | Enable cost tracking |
| `daily_limit_usd` | `10.00` | Daily spending limit in USD |
| `monthly_limit_usd` | `100.00` | Monthly spending limit in USD |
| `warn_at_percent` | `80` | Warn when spending reaches this percentage of limit |
| `allow_override` | `false` | Allow requests to exceed budget with `--override` flag |

Notes:

- When `enabled = true`, the runtime tracks per-request cost estimates and enforces daily/monthly limits.
- At `warn_at_percent` threshold, a warning is emitted but requests continue.
- When a limit is reached, requests are rejected unless `allow_override = true` and the `--override` flag is passed.

## `[identity]`

| Key | Default | Purpose |
|---|---|---|
| `format` | `openclaw` | Identity format: `"openclaw"` (default) or `"aieos"` |
| `aieos_path` | unset | Path to AIEOS JSON file (relative to workspace) |
| `aieos_inline` | unset | Inline AIEOS JSON (alternative to file path) |

Notes:

- Use `format = "aieos"` with either `aieos_path` or `aieos_inline` to load an AIEOS / OpenClaw identity document.
- Only one of `aieos_path` or `aieos_inline` should be set; `aieos_path` takes precedence.

## `[multimodal]`

| Key | Default | Purpose |
|---|---|---|
| `max_images` | `4` | Maximum image markers accepted per request |
| `max_image_size_mb` | `5` | Per-image size limit before base64 encoding |
| `allow_remote_fetch` | `false` | Allow fetching `http(s)` image URLs from markers |

Notes:

- Runtime accepts image markers in user messages with syntax: ``[IMAGE:<source>]``.
- Supported sources:
  - Local file path (for example ``[IMAGE:/tmp/screenshot.png]``)
- Data URI (for example ``[IMAGE:data:image/png;base64,...]``)
- Remote URL only when `allow_remote_fetch = true`
- Allowed MIME types: `image/png`, `image/jpeg`, `image/webp`, `image/gif`, `image/bmp`.
- When the active provider does not support vision, requests fail with a structured capability error (`capability=vision`) instead of silently dropping images.

## `[browser]`

| Key | Default | Purpose |
|---|---|---|
| `enabled` | `false` | Enable `browser_open` tool (opens URLs without scraping) |
| `allowed_domains` | `[]` | Allowed domains for `browser_open` (exact or subdomain match) |
| `session_name` | unset | Browser session name (for agent-browser automation) |
| `backend` | `agent_browser` | Browser automation backend: `"agent_browser"`, `"rust_native"`, `"computer_use"`, or `"auto"` |
| `native_headless` | `true` | Headless mode for rust-native backend |
| `native_webdriver_url` | `http://127.0.0.1:9515` | WebDriver endpoint URL for rust-native backend |
| `native_chrome_path` | unset | Optional Chrome/Chromium executable path for rust-native backend |

### `[browser.computer_use]`

| Key | Default | Purpose |
|---|---|---|
| `endpoint` | `http://127.0.0.1:8787/v1/actions` | Sidecar endpoint for computer-use actions (OS-level mouse/keyboard/screenshot) |
| `api_key` | unset | Optional bearer token for computer-use sidecar (stored encrypted) |
| `timeout_ms` | `15000` | Per-action request timeout in milliseconds |
| `allow_remote_endpoint` | `false` | Allow remote/public endpoint for computer-use sidecar |
| `window_allowlist` | `[]` | Optional window title/process allowlist forwarded to sidecar policy |
| `max_coordinate_x` | unset | Optional X-axis boundary for coordinate-based actions |
| `max_coordinate_y` | unset | Optional Y-axis boundary for coordinate-based actions |

Notes:

- When `backend = "computer_use"`, the agent delegates browser actions to the sidecar at `computer_use.endpoint`.
- `allow_remote_endpoint = false` (default) rejects any non-loopback endpoint to prevent accidental public exposure.
- Use `window_allowlist` to restrict which OS windows the sidecar can interact with.

## `[http_request]`

| Key | Default | Purpose |
|---|---|---|
| `enabled` | `false` | Enable `http_request` tool for API interactions |
| `allowed_domains` | `[]` | Allowed domains for HTTP requests (exact or subdomain match) |
| `max_response_size` | `1000000` | Maximum response size in bytes (default: 1 MB) |
| `timeout_secs` | `30` | Request timeout in seconds |

Notes:

- Deny-by-default: if `allowed_domains` is empty, all HTTP requests are rejected.
- Use exact domain or subdomain matching (e.g. `"api.example.com"`, `"example.com"`).

## `[gateway]`

| Key | Default | Purpose |
|---|---|---|
| `host` | `127.0.0.1` | bind address |
| `port` | `3000` | gateway listen port |
| `require_pairing` | `true` | require pairing before bearer auth |
| `allow_public_bind` | `false` | block accidental public exposure |

## `[autonomy]`

| Key | Default | Purpose |
|---|---|---|
| `level` | `supervised` | `read_only`, `supervised`, or `full` |
| `workspace_only` | `true` | restrict writes/command paths to workspace scope |
| `allowed_commands` | _required for shell execution_ | allowlist of executable names |
| `forbidden_paths` | `[]` | explicit path denylist |
| `max_actions_per_hour` | `100` | per-policy action budget |
| `max_cost_per_day_cents` | `1000` | per-policy spend guardrail |
| `require_approval_for_medium_risk` | `true` | approval gate for medium-risk commands |
| `block_high_risk_commands` | `true` | hard block for high-risk commands |
| `auto_approve` | `[]` | tool operations always auto-approved |
| `always_ask` | `[]` | tool operations that always require approval |

Notes:

- **Recommended default for new installs**: keep `level = "supervised"` and `workspace_only = true`. Use `full` only when operators accept broader shell/filesystem scope.
- **Two approval layers**: `[autonomy]` enforces path/command guardrails (`allowed_commands`, `forbidden_paths`, `workspace_only`); `ApprovalGate` gates medium/high-risk tool operations when `level = "supervised"`. A command can fail on guardrails even when `level = "full"`.
- **Unified gate**: shell tools no longer accept model-supplied `approved`; human consent is injected only after CLI/Gateway/Channel approval. See [policy-approval-reference.md](policy-approval-reference.md).
- **Shell allowlist (VL-SEC-009)**: commands not listed in `allowed_commands` are hard-denied; human Yes/Always cannot widen the allowlist. Shell-policy Always only skips risk re-prompts for remembered executable basenames that are already allowlisted—add new binaries via config / L2 / `policy_patch` on `autonomy.allowed_commands`.
- `level = "full"` skips medium-risk approval gating for shell execution, while still enforcing configured guardrails.
- Shell separator/operator parsing is quote-aware. Characters like `;` inside quoted arguments are treated as literals, not command separators.
- Unquoted shell chaining/operators are still enforced by policy checks (`;`, `|`, `&&`, `||`, background chaining, and redirects).

## `[memory]`

| Key | Default | Purpose |
|---|---|---|
| `backend` | `sqlite` | `sqlite`, `lucid`, `markdown`, `none` |
| `auto_save` | `true` | persist user-stated inputs only (assistant outputs are excluded) |
| `embedding_provider` | `none` | `none`, `openai`, or custom endpoint |
| `embedding_model` | `text-embedding-3-small` | embedding model ID, or `hint:<name>` route |
| `embedding_dimensions` | `1536` | expected vector size for selected embedding model |
| `vector_weight` | `0.7` | hybrid ranking vector weight |
| `keyword_weight` | `0.3` | hybrid ranking keyword weight |

Notes:

- Memory context injection ignores legacy `assistant_resp*` auto-save keys to prevent old model-authored summaries from being treated as facts.

## `[[model_routes]]` and `[[embedding_routes]]`

Use route hints so integrations can keep stable names while model IDs evolve.

### `[[model_routes]]`

| Key | Default | Purpose |
|---|---|---|
| `hint` | _required_ | Task name (e.g. `"reasoning"`, `"fast"`, `"code"`, `"summarize"`) |
| `provider` | _required_ | Provider to route to (must match a known provider name) |
| `model` | _required_ | Model to use with that provider |
| `api_key` | unset | Optional API key override for this route's provider |
| `fallbacks` | `[]` | **VL-NA-021.** Same-hint peers `{ provider, model }` after micro-retry. Used only when `hint_peer_fallback` is on. |

### `[[embedding_routes]]`

| Key | Default | Purpose |
|---|---|---|
| `hint` | _required_ | Route hint name (e.g. `"semantic"`, `"archive"`, `"faq"`) |
| `provider` | _required_ | Embedding provider (`"none"`, `"openai"`, or `"custom:<url>"`) |
| `model` | _required_ | Embedding model to use with that provider |
| `dimensions` | unset | Optional embedding dimension override for this route |
| `api_key` | unset | Optional API key override for this route's provider |

```toml
[memory]
embedding_model = "hint:semantic"

[[model_routes]]
hint = "reasoning"
provider = "openrouter"
model = "provider/model-id"
# Used only when [agent].hint_peer_fallback = true (dist default off)
fallbacks = [
  { provider = "openrouter", model = "provider/peer-model" },
]

[[embedding_routes]]
hint = "semantic"
provider = "openai"
model = "text-embedding-3-small"
dimensions = 1536
```

Upgrade strategy:

1. Keep hints stable (`hint:reasoning`, `hint:semantic`).
2. Update only `model = "...new-version..."` in the route entries.
3. Validate with `velaclaw doctor` before restart/rollout.

## `[query_classification]`

Automatic model hint routing — maps user messages to `[[model_routes]]` hints based on content patterns.

| Key | Default | Purpose |
|---|---|---|
| `enabled` | `false` | Enable automatic query classification |
| `rules` | `[]` | Classification rules (evaluated in priority order) |

Each rule in `rules`:

| Key | Default | Purpose |
|---|---|---|
| `hint` | _required_ | Must match a `[[model_routes]]` hint value |
| `keywords` | `[]` | Case-insensitive substring matches |
| `patterns` | `[]` | Case-sensitive literal matches (for code fences, keywords like `"fn "`) |
| `min_length` | unset | Only match if message length ≥ N chars |
| `max_length` | unset | Only match if message length ≤ N chars |
| `priority` | `0` | Higher priority rules are checked first |

```toml
[query_classification]
enabled = true

[[query_classification.rules]]
hint = "reasoning"
keywords = ["explain", "analyze", "why"]
min_length = 200
priority = 10

[[query_classification.rules]]
hint = "fast"
keywords = ["hi", "hello", "thanks"]
max_length = 50
priority = 5
```

## `[channels_config]`

Top-level channel options are configured under `channels_config`.

| Key | Default | Purpose |
|---|---|---|
| `message_timeout_secs` | `300` | Base timeout in seconds for channel message processing; runtime scales this with tool-loop depth (up to 4x) |

Examples:

- `[channels_config.telegram]`
- `[channels_config.discord]`
- `[channels_config.whatsapp]`
- `[channels_config.nextcloud_talk]`
- `[channels_config.email]`

Notes:

- Default `300s` is optimized for on-device LLMs (Ollama) which are slower than cloud APIs.
- Runtime timeout budget is `message_timeout_secs * scale`, where `scale = min(max_tool_iterations, 4)` and a minimum of `1`.
- This scaling avoids false timeouts when the first LLM turn is slow/retried but later tool-loop turns still need to complete.
- If using cloud APIs (OpenAI, Anthropic, etc.), you can reduce this to `60` or lower.
- Values below `30` are clamped to `30` to avoid immediate timeout churn.
- When a timeout occurs, users receive: `⚠️ Request timed out while waiting for the model. Please try again.`
- Telegram-only interruption behavior is controlled with `channels_config.telegram.interrupt_on_new_message` (default `false`).
  When enabled, a newer message from the same sender in the same chat cancels the in-flight request and preserves interrupted user context.
- While `velaclaw channel start` is running, updates to `default_provider`, `default_model`, `default_temperature`, `api_key`, `api_url`, `reliability.*`, and `[agent].max_tool_iterations` are hot-applied from `config.toml` on the next inbound message. See [config-externalization.md](config-externalization.md) for the full config-vs-rebuild contract.

See detailed channel matrix and allowlist behavior in [channels-reference.md](channels-reference.md).

### `[channels_config.whatsapp]`

WhatsApp supports two backends under one config table.

Cloud API mode (Meta webhook):

| Key | Required | Purpose |
|---|---|---|
| `access_token` | Yes | Meta Cloud API bearer token |
| `phone_number_id` | Yes | Meta phone number ID |
| `verify_token` | Yes | Webhook verification token |
| `app_secret` | Optional | Enables webhook signature verification (`X-Hub-Signature-256`) |
| `allowed_numbers` | Recommended | Allowed inbound numbers (`[]` = deny all, `"*"` = allow all) |

WhatsApp Web mode (native client):

| Key | Required | Purpose |
|---|---|---|
| `session_path` | Yes | Persistent SQLite session path |
| `pair_phone` | Optional | Pair-code flow phone number (digits only) |
| `pair_code` | Optional | Custom pair code (otherwise auto-generated) |
| `allowed_numbers` | Recommended | Allowed inbound numbers (`[]` = deny all, `"*"` = allow all) |

Notes:

- WhatsApp Web requires build flag `whatsapp-web`.
- If both Cloud and Web fields are present, Cloud mode wins for backward compatibility.

### `[channels_config.nextcloud_talk]`

Native Nextcloud Talk bot integration (webhook receive + OCS send API).

| Key | Required | Purpose |
|---|---|---|
| `base_url` | Yes | Nextcloud base URL (e.g. `https://cloud.example.com`) |
| `app_token` | Yes | Bot app token used for OCS bearer auth |
| `webhook_secret` | Optional | Enables webhook signature verification |
| `allowed_users` | Recommended | Allowed Nextcloud actor IDs (`[]` = deny all, `"*"` = allow all) |

Notes:

- Webhook endpoint is `POST /nextcloud-talk`.
- `VELACLAW_NEXTCLOUD_TALK_WEBHOOK_SECRET` overrides `webhook_secret` when set.
- See [nextcloud-talk-setup.md](nextcloud-talk-setup.md) for setup and troubleshooting.

## `[hardware]`

Hardware wizard configuration for physical-world access (STM32, probe, serial).

| Key | Default | Purpose |
|---|---|---|
| `enabled` | `false` | Whether hardware access is enabled |
| `transport` | `none` | Transport mode: `"none"`, `"native"`, `"serial"`, or `"probe"` |
| `serial_port` | unset | Serial port path (e.g. `"/dev/ttyACM0"`) |
| `baud_rate` | `115200` | Serial baud rate |
| `probe_target` | unset | Probe target chip (e.g. `"STM32F401RE"`) |
| `workspace_datasheets` | `false` | Enable workspace datasheet RAG (index PDF schematics for AI pin lookups) |

Notes:

- Use `transport = "serial"` with `serial_port` for USB-serial connections.
- Use `transport = "probe"` with `probe_target` for debug-probe flashing (e.g. ST-Link).
- See [hardware-peripherals-design.md](hardware-peripherals-design.md) for protocol details.

## `[peripherals]`

Higher-level peripheral board configuration. Boards become agent tools when enabled.

| Key | Default | Purpose |
|---|---|---|
| `enabled` | `false` | Enable peripheral support (boards become agent tools) |
| `boards` | `[]` | Board configurations |
| `datasheet_dir` | unset | Path to datasheet docs (relative to workspace) for RAG retrieval |

Each entry in `boards`:

| Key | Default | Purpose |
|---|---|---|
| `board` | _required_ | Board type: `"nucleo-f401re"`, `"rpi-gpio"`, `"esp32"`, etc. |
| `transport` | `serial` | Transport: `"serial"`, `"native"`, `"websocket"` |
| `path` | unset | Path for serial: `"/dev/ttyACM0"`, `"/dev/ttyUSB0"` |
| `baud` | `115200` | Baud rate for serial |

```toml
[peripherals]
enabled = true
datasheet_dir = "docs/datasheets"

[[peripherals.boards]]
board = "nucleo-f401re"
transport = "serial"
path = "/dev/ttyACM0"
baud = 115200

[[peripherals.boards]]
board = "rpi-gpio"
transport = "native"
```

Notes:

- Place `.md`/`.txt` datasheet files named by board (e.g. `nucleo-f401re.md`, `rpi-gpio.md`) in `datasheet_dir` for RAG retrieval.
- See [hardware-peripherals-design.md](hardware-peripherals-design.md) for board protocol and firmware notes.

## Security-Relevant Defaults

- deny-by-default channel allowlists (`[]` means deny all)
- pairing required on gateway by default
- public bind disabled by default

## Validation Commands

After editing config:

```bash
velaclaw status
velaclaw doctor
velaclaw channel doctor
velaclaw service restart
```

## Related Docs

- [channels-reference.md](channels-reference.md)
- [providers-reference.md](providers-reference.md)
- [operations-runbook.md](operations-runbook.md)
- [troubleshooting.md](troubleshooting.md)
