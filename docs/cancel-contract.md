# Cancel contract (Web, CLI, REST, Channels)

Runtime contract for **stopping an in-flight agent turn**. Implementation lives in
[`src/agent/turn_cancel.rs`](../src/agent/turn_cancel.rs) and the single tool loop
[`run_tool_call_loop`](../src/agent/loop_/tool_loop.rs) (GOV-007).

## Shared outcomes

| Outcome | Persist assistant text | User copy |
|---|---|---|
| **Completed** | Yes | Normal reply |
| **Cancelled** (`ToolLoopCancelled`) | User at turn start; assistant = `Stopped.` | `Stopped.` |
| **Failed** | No | Error (CLI stderr / WS `error`) |

Web Chat **Stop** and CLI **Esc Esc** both classify with `classify_turn_result`. Cancel also stores a DAG fail cursor (`fail_class=cancelled`) so the next message resumes remaining nodes instead of planning a postmortem graph.

## Surfaces

| Surface | How to stop | Token | Notes |
|---|---|---|---|
| Web Chat (`GET /ws`) | `{ "type": "cancel" }` then optional Close | Yes | Close or recv error also cancels. Frame: `cancelled`. |
| CLI interactive | Esc twice within 500ms (TTY) | Yes (`CliTurnCancel`) | Returns to the prompt; not a process exit. |
| CLI `y/n` approval | Type **N** | Token still set | Stdin is owned by the approval prompt; double-Esc may not fire until that prompt returns. |
| `POST /api/chat` | **None** | `None` | Fire-and-forget until the HTTP call returns. Not equivalent to WS Stop. Do not add a second REST cancel protocol. |
| Channels (e.g. Telegram interrupt) | Channel-specific | Same loop + token | Interrupted turns must not persist a successful assistant reply (existing tests). |
| Eos Web AbortController | Browser abort of SSE | Different host | Eos is not VelaClaw’s tool-loop; do not copy AbortController into this agent. |
| Ctrl+C | Process exit | N/A | Not a turn cancel. |

## Mid-tool and HITL

1. **LLM wait:** `select!` on `CancellationToken` vs `provider.chat`. Dropping the chat future is the stop; explicit ai-lib `CancelHandle` is **out of scope** unless a leak is measured.
2. **Tool execute:** cancel **must** return `ToolLoopCancelled`, not a failed tool string that the model would continue on.
3. **Shell:** `tokio::process::Command` uses `kill_on_drop(true)` so dropping the tool future ends **this turn’s** child. That is turn-stop, not a change to `[autonomy].allowed_commands` (SEC-009).
4. **Other tools** (browser, git helpers): future drop only; a child may run until its own timeout.
5. **ApprovalHub / HumanInputHub:** Stop aborts pending waiters so the UI modal does not sit until the 5–10 minute timeout, and so a dropped waiter is **not** treated as a user **Denied** that continues the loop.

## Non-goals

- Second tool-loop or REST cancel API
- Widening shell allowlists
- Channel-wide 003 rewrite
