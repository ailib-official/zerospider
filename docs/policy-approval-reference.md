# Policy & Approval Runtime Reference

Canonical runtime contract for VelaClaw **0.7.0+** unified policy and approval layers (VL-SEC-001..006).

Related:

- Config keys: [config-reference.md](config-reference.md) — `[autonomy]`, L2/L2.5 policy files, channel `approval_mode`
- Migration from pre-0.7 behavior: [migration-policy-v0.7.0.md](migration-policy-v0.7.0.md)
- Channel setup: [channels-reference.md](channels-reference.md)

## Policy layers

| Layer | Location | Purpose |
|---|---|---|
| **L0** | ai-protocol manifest | `tool_calling` (parser, native strategy) |
| **L1** | `~/.velaclaw/config.toml` | `[autonomy]`, `[agent]`, `[gateway]`, `[security.*]` |
| **L2** | `<workspace>/agent-policy.yaml` | Workspace overrides: `tool_calling`, `autonomy`, `approval`, `self_adjust` (v1 or v2) |
| **L2.5** | `<workspace>/.velaclaw/policy-overrides.yaml` | Persistent operator/agent patches (session allowlist, autonomy tweaks) |
| **L3** | Session / channel profile | Per-sender dispatcher override, channel `approval_mode`, gateway pairing |

Merge order for autonomy/approval effective values:

**L1 config.toml** → **L2 agent-policy.yaml** → **L2.5 policy-overrides.yaml** → runtime session state.

## ApprovalGate (single human gate)

All supervised tool execution paths use `ApprovalGate`:

1. **Policy check** — `SecurityPolicy` / `PolicyHandle` (paths, shell risk, rate limits).
2. **Human check** — channel-specific backend (`CLI` stdin, `Gateway` Web UI hub, `Channel` inline prompt).

The shell tool schema **does not** expose an `approved` parameter. Human consent is injected internally after gate approval; models cannot self-approve.

Credential files (`github_token_list.txt`, SSH key basenames, …) use this same gate (VL-SEC-013): argv tokens, workspace `bash`/`sh` script bodies, and `file_read`/`file_write` paths. Isolated profile **Ask** → Once; unset profile **Deny** has no modal. Gate `Denied` writes `ReceiptDecision::Deny`. Do not use `request_human_input` for PAT files.

### Three entry approval matrix

| Entry | Supervised tool approval | Shell medium-risk confirmation | Notes |
|---|---|---|---|
| **CLI** (`velaclaw agent`, one-shot) | Interactive stdin: `🔒 Security policy requires approval...` then `[Y]es / [N]o / [A]lways` | Same prompt path when policy requires human approval; shell shows the command | Tool `Always` → L2.5 `approval.session_allowlist`. Shell-policy `Always` → executable basename in `approval.session_shell_binaries` (does **not** widen `allowed_commands`) |
| **Gateway** (Web UI) | `ApprovalHub` modal / async request | Gateway hub prompt when shell policy requires it | Requires pairing when `require_pairing = true` |
| **Channel** (Telegram, Discord, …) | Controlled by `approval_mode` (see below) | Inline mode only; `deny` blocks interactive approval | Default: `inline` with timeout (300s) |

### Web `request_human_input` (short credentials / choices)

Separate from tool/shell **approval**: the agent may call `request_human_input` when it needs a **short** operator value so the **same turn** can continue.

| Kind | Intended use | Not for |
|---|---|---|
| `choice` | Abort vs short option buttons | Long free-form answers |
| `secret` | Password / token → one-shot `secret_slot` for `shell` | Asking the human to run sudo themselves |
| `text` | Short codes only (≤128 chars) | Pasting command output / logs |
| `handoff` | Rare off-machine confirm only | “Run this in your terminal and paste results” |

Normative UX: machine work uses **`shell` + ApprovalHub** (Deny / Allow once / Always). Collecting terminal results via a modal is not an agent workflow.

### Channel `approval_mode`

Set on each channel table, for example `[channels_config.telegram]`:

| Value | Behavior |
|---|---|
| `inline` (default) | Prompt in-channel (inline keyboard / Y-N-A). Supervised tools wait for human response. |
| `deny` | Deny any tool call that would require interactive approval. |
| `gateway_redirect` | Reserved; defer to gateway Web UI (not wired for all channels). |

## Tool batch execution

`run_tool_call_loop` and channel/gateway handlers call `execute_tool_batch()` (VL-UR-003):

- Multiple independent tool calls run **in parallel** when no pending call needs approval gating.
- When any call in the batch needs approval, the batch runs **sequentially** through the gate.
- Result order remains stable (matches call order).

`Agent::turn()` uses a separate path for the embedded web agent API; CLI/channel/gateway share the unified batch helper.

## L2 — `agent-policy.yaml` v2

Supported `version`: `1` or `2`. Version `2` adds autonomy and approval override sections:

```yaml
version: 2
tool_calling:
  dispatcher: auto
autonomy:
  level: supervised
  allowed_commands: [git, cargo, rg]
approval:
  auto_approve: [file_read, memory_recall]
  always_ask: [shell]
self_adjust:
  allowed_writes:
    - autonomy.allowed_commands
    - approval.session_allowlist
  denied_writes:
    - security.*
    - channels.*.credentials
    - gateway.paired_tokens
```

Discovery: project root or `workspace/agent-policy.yaml`, walking up from CWD; honors `VELACLAW_WORKSPACE`.

## L2.5 — `.velaclaw/policy-overrides.yaml`

User-facing persistent layer under the workspace:

```
<workspace>/.velaclaw/policy-overrides.yaml
```

Written by:

- Operator **Always** on tools (appends to `approval.session_allowlist`)
- Operator **Always** on shell-policy prompts (appends executable basename to `approval.session_shell_binaries`)
- `policy_patch` tool when `self_adjust` allows the dot-path (requires `ai-protocol` feature)

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

**VL-SEC-009 (scheme H):** `allowed_commands` is a hard gate — interactive Yes/Always cannot add executables. Non-allowlisted shell commands are denied without a risk prompt. Shell-policy Always only skips **risk** re-prompts for remembered basenames that are already allowlisted.

### Ops-readonly profile {#ops-readonly-profile}

VelaClaw does **not** enable ops diagnostics (`df` / `du` / `free` / `uname` / …) in global schema defaults. Operators who want those basenames can **manually merge** the fragment:

- Example: [`examples/profiles/ops-readonly.toml`](../examples/profiles/ops-readonly.toml)
- Fresh onboard also seeds [`agent-policy.yaml`](../examples/profiles/agent-policy.self-adjust.yaml) so `policy_patch` may extend `autonomy.allowed_commands` when L2 self_adjust allows it.
- Existing `config.toml` / `daemon.env` are never silently rewritten.

Deny messages (CLI shell tool + Web tool result) share the same next-step semantics: edit `[autonomy].allowed_commands`, merge ops-readonly, or use `policy_patch` when seeded — approval cannot widen the allowlist.

## `policy_patch` tool

Available with `--features ai-protocol`. Applies validated dot-path patches to L2.5. Paths must match `self_adjust.allowed_writes` globs and must not match `denied_writes`.

Supported paths include:

- `approval.session_allowlist`
- `approval.session_shell_binaries`
- `autonomy.level`, `autonomy.workspace_only`, `autonomy.allowed_commands`, `autonomy.forbidden_paths`
- `autonomy.auto_approve`, `autonomy.always_ask`

Denied by default: `security.*`, credential fields, `gateway.paired_tokens`.

## Defaults vs schema

Documented defaults match `AutonomyConfig` / schema defaults in `src/config/schema/`:

- `level = supervised`
- `workspace_only = true`
- `max_actions_per_hour = 100`
- `require_approval_for_medium_risk = true`
- `block_high_risk_commands = true`
- Channel `approval_mode = inline`
- Channel `approval_timeout_secs = 300`

## Audit

When `[security.audit]` is enabled, tool approval decisions are appended to the security audit log in addition to the in-memory `ApprovalManager` audit trail.
