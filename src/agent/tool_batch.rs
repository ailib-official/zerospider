//! Unified tool batch execution — approval gate + parallel/sequential dispatch (VL-UR-003).
//! 统一工具批执行：批准门 + 并行/串行调度。

use crate::agent::dispatcher::ParsedToolCall as GateToolCall;
use crate::agent::host_phase::HostPhase;
use crate::approval::{
    ApprovalGate, ApprovalHub, ApprovalManager, ChannelApprovalSession, GateDecision, HumanInputHub,
};
use crate::observability::{Observer, ObserverEvent};
use crate::security::{PolicyHandle, ReceiptDecision, ToolReceiptLog};
use crate::tools::{Tool, ToolExecutionContext};
use anyhow::Result;
use std::sync::Arc;
use std::time::Instant;
use tokio_util::sync::CancellationToken;
use velaclaw_agent_runtime::{
    is_shell_policy_tool, normalize_tool_arguments, shell_command_from_args, ToolLoopCancelled,
};

pub(crate) use velaclaw_agent_runtime::scrub_credentials;

/// Parsed tool call from LLM output (loop-local shape without provider tool_call_id).
#[derive(Debug, Clone)]
pub(crate) struct ParsedToolCall {
    pub name: String,
    pub arguments: serde_json::Value,
}

/// Output of one tool invocation in a batch.
#[derive(Debug, Clone)]
pub struct ToolBatchResult {
    pub output: String,
    pub success: bool,
}

/// Optional Web/gateway gate extras (VL-CTX-002): ApprovalHub + secret_slot hub.
#[derive(Clone, Default)]
pub(crate) struct ToolBatchGateExtras {
    pub approval_hub: Option<Arc<ApprovalHub>>,
    pub human_input_hub: Option<Arc<HumanInputHub>>,
    pub host_phase: HostPhase,
}

fn abort_hitl(extras: Option<&ToolBatchGateExtras>) {
    if let Some(extras) = extras {
        if let Some(hub) = &extras.approval_hub {
            hub.abort_all_pending();
        }
        if let Some(hub) = &extras.human_input_hub {
            hub.abort_all_pending();
        }
    }
}

fn cancelled_err() -> anyhow::Error {
    ToolLoopCancelled.into()
}

fn find_tool<'a>(tools: &'a [Box<dyn Tool>], name: &str) -> Option<&'a dyn Tool> {
    tools.iter().find(|t| t.name() == name).map(|t| t.as_ref())
}

/// Resolve shell `secret_slot` into stdin secret (same semantics as prior Agent path).
///
/// Gate approval still sees the original args (including `secret_slot`) via
/// `call.arguments.clone()`; this helper strips the slot from the execution
/// copy and consumes the secret from [`HumanInputHub`] so the shell never
/// receives the opaque slot id as a literal argument.
fn build_tool_execution_context(
    call_name: &str,
    args: &mut serde_json::Value,
    shell_human_approved: bool,
    human_input_hub: Option<&HumanInputHub>,
) -> Result<ToolExecutionContext, ToolBatchResult> {
    let mut stdin_secret = None;
    if call_name == "shell" {
        if let Some(slot_id) = args
            .as_object_mut()
            .and_then(|m| m.remove("secret_slot"))
            .and_then(|v| v.as_str().map(|s| s.to_string()))
        {
            let Some(hub) = human_input_hub else {
                return Err(ToolBatchResult {
                    output: "Error: secret_slot requires interactive gateway human input".into(),
                    success: false,
                });
            };
            match hub.secret_slots().take(&slot_id) {
                Some(secret) => stdin_secret = Some(secret),
                None => {
                    return Err(ToolBatchResult {
                        output: format!(
                            "Error: secret_slot '{slot_id}' is missing or already consumed. \
                             Call request_human_input(kind=secret) again."
                        ),
                        success: false,
                    });
                }
            }
        }
    }
    Ok(
        ToolExecutionContext::with_shell_human_approved(shell_human_approved)
            .with_stdin_secret(stdin_secret),
    )
}

fn plan_blocked(phase: HostPhase, tool_name: &str) -> Option<ToolBatchResult> {
    phase
        .blocked_output(tool_name)
        .map(|output| ToolBatchResult {
            output,
            success: false,
        })
}

async fn execute_one_tool(
    call_name: &str,
    call_arguments: serde_json::Value,
    tools_registry: &[Box<dyn Tool>],
    observer: &dyn Observer,
    cancellation_token: Option<&CancellationToken>,
    ctx: &ToolExecutionContext,
    extras: Option<&ToolBatchGateExtras>,
) -> Result<ToolBatchResult> {
    let call_arguments = normalize_tool_arguments(call_name, call_arguments);
    let caption = crate::agent::turn_progress::progress_caption(call_name, &call_arguments);
    let Some(tool) = find_tool(tools_registry, call_name) else {
        return Ok(ToolBatchResult {
            output: format!("Unknown tool: {call_name}"),
            success: false,
        });
    };

    observer.record_event(&ObserverEvent::ToolCallStart {
        tool: call_name.to_string(),
        caption: Some(caption.clone()),
    });
    let start = Instant::now();

    let tool_future = tool.execute(call_arguments, ctx);
    let tool_result = if let Some(token) = cancellation_token {
        tokio::select! {
            () = token.cancelled() => {
                abort_hitl(extras);
                return Err(cancelled_err());
            }
            result = tool_future => result,
        }
    } else {
        tool_future.await
    };

    if cancellation_token.is_some_and(CancellationToken::is_cancelled) {
        abort_hitl(extras);
        return Err(cancelled_err());
    }

    match tool_result {
        Ok(r) => {
            let output = if r.success {
                scrub_credentials(&r.output)
            } else {
                let raw = r.error.unwrap_or_else(|| r.output);
                format!("Error: {}", scrub_credentials(&raw))
            };
            let expand = crate::agent::turn_progress::progress_expand_body(&output);
            observer.record_event(&ObserverEvent::ToolCall {
                tool: call_name.to_string(),
                duration: start.elapsed(),
                success: r.success,
                summary: Some(caption.clone()),
                detail: expand,
            });
            Ok(ToolBatchResult {
                output,
                success: r.success,
            })
        }
        Err(e) => {
            let output = scrub_credentials(&format!("Error executing {call_name}: {e}"));
            let expand = crate::agent::turn_progress::progress_expand_body(&output);
            observer.record_event(&ObserverEvent::ToolCall {
                tool: call_name.to_string(),
                duration: start.elapsed(),
                success: false,
                summary: Some(caption),
                detail: expand,
            });
            Ok(ToolBatchResult {
                output,
                success: false,
            })
        }
    }
}

/// Whether multiple tool calls may run concurrently (gate-aware).
pub(crate) fn should_execute_tools_in_parallel(
    tool_calls: &[ParsedToolCall],
    gate: Option<&ApprovalGate<'_>>,
) -> bool {
    if tool_calls.len() <= 1 {
        return false;
    }

    if let Some(gate) = gate {
        if tool_calls
            .iter()
            .any(|call| is_shell_policy_tool(&call.name) || gate.needs_approval(&call.name))
        {
            return false;
        }
    }

    true
}

async fn execute_tools_parallel(
    tool_calls: &[ParsedToolCall],
    tools_registry: &[Box<dyn Tool>],
    observer: &dyn Observer,
    cancellation_token: Option<&CancellationToken>,
    host_phase: HostPhase,
    extras: Option<&ToolBatchGateExtras>,
) -> Result<Vec<ToolBatchResult>> {
    if cancellation_token.is_some_and(CancellationToken::is_cancelled) {
        abort_hitl(extras);
        return Err(cancelled_err());
    }
    let mut blocked: Vec<(usize, ToolBatchResult)> = Vec::new();
    let mut runnable: Vec<(usize, ParsedToolCall)> = Vec::new();
    for (i, call) in tool_calls.iter().enumerate() {
        if let Some(b) = plan_blocked(host_phase, &call.name) {
            blocked.push((i, b));
        } else {
            runnable.push((i, call.clone()));
        }
    }
    let futures: Vec<_> = runnable
        .into_iter()
        .map(|(i, call)| async move {
            let ctx = ToolExecutionContext::default();
            (
                i,
                execute_one_tool(
                    &call.name,
                    call.arguments,
                    tools_registry,
                    observer,
                    cancellation_token,
                    &ctx,
                    extras,
                )
                .await,
            )
        })
        .collect();
    let mut out = vec![
        ToolBatchResult {
            output: String::new(),
            success: false,
        };
        tool_calls.len()
    ];
    for (i, b) in blocked {
        out[i] = b;
    }
    for (i, r) in futures_util::future::join_all(futures).await {
        match r {
            Ok(batch) => out[i] = batch,
            Err(e) => {
                abort_hitl(extras);
                return Err(e);
            }
        }
    }
    if cancellation_token.is_some_and(CancellationToken::is_cancelled) {
        abort_hitl(extras);
        return Err(cancelled_err());
    }
    Ok(out)
}

async fn execute_tools_sequential_no_gate(
    tool_calls: &[ParsedToolCall],
    tools_registry: &[Box<dyn Tool>],
    observer: &dyn Observer,
    cancellation_token: Option<&CancellationToken>,
    extras: Option<&ToolBatchGateExtras>,
    host_phase: HostPhase,
) -> Result<Vec<ToolBatchResult>> {
    let human_input_hub = extras
        .and_then(|e| e.human_input_hub.as_ref())
        .map(std::convert::AsRef::as_ref);
    let mut results = Vec::with_capacity(tool_calls.len());

    for call in tool_calls {
        if cancellation_token.is_some_and(CancellationToken::is_cancelled) {
            abort_hitl(extras);
            return Err(cancelled_err());
        }
        if let Some(blocked) = plan_blocked(host_phase, &call.name) {
            results.push(blocked);
            continue;
        }
        let mut args = normalize_tool_arguments(&call.name, call.arguments.clone());
        let ctx = match build_tool_execution_context(&call.name, &mut args, false, human_input_hub)
        {
            Ok(ctx) => ctx,
            Err(err) => {
                results.push(err);
                continue;
            }
        };
        results.push(
            execute_one_tool(
                &call.name,
                args,
                tools_registry,
                observer,
                cancellation_token,
                &ctx,
                extras,
            )
            .await?,
        );
    }

    Ok(results)
}

async fn execute_tools_sequential_with_gate(
    tool_calls: &[ParsedToolCall],
    tools_registry: &[Box<dyn Tool>],
    observer: &dyn Observer,
    gate: &ApprovalGate<'_>,
    cancellation_token: Option<&CancellationToken>,
    extras: Option<&ToolBatchGateExtras>,
    host_phase: HostPhase,
    policy: Option<&PolicyHandle>,
) -> Result<Vec<ToolBatchResult>> {
    let human_input_hub = extras
        .and_then(|e| e.human_input_hub.as_ref())
        .map(std::convert::AsRef::as_ref);
    let mut results = Vec::with_capacity(tool_calls.len());

    for call in tool_calls {
        if cancellation_token.is_some_and(CancellationToken::is_cancelled) {
            abort_hitl(extras);
            return Err(cancelled_err());
        }
        if let Some(blocked) = plan_blocked(host_phase, &call.name) {
            results.push(blocked);
            continue;
        }
        let mut args = normalize_tool_arguments(&call.name, call.arguments.clone());
        let gate_call = GateToolCall {
            name: call.name.clone(),
            arguments: call.arguments.clone(),
            tool_call_id: None,
        };

        let decision = if let Some(token) = cancellation_token {
            tokio::select! {
                () = token.cancelled() => {
                    abort_hitl(extras);
                    return Err(cancelled_err());
                }
                decision = gate.decide_async(&gate_call) => decision,
            }
        } else {
            gate.decide_async(&gate_call).await
        };

        if cancellation_token.is_some_and(CancellationToken::is_cancelled) {
            abort_hitl(extras);
            return Err(cancelled_err());
        }

        let (shell_human_approved, proceed) = match decision {
            GateDecision::Denied { message } => {
                record_gate_denied(policy, &call.name, &call.arguments);
                results.push(ToolBatchResult {
                    output: scrub_credentials(&message),
                    success: false,
                });
                (false, false)
            }
            GateDecision::Proceed {
                shell_human_approved,
            } => (shell_human_approved, true),
        };

        if !proceed {
            continue;
        }

        let ctx = match build_tool_execution_context(
            &call.name,
            &mut args,
            shell_human_approved,
            human_input_hub,
        ) {
            Ok(ctx) => ctx,
            Err(err) => {
                results.push(err);
                continue;
            }
        };
        results.push(
            execute_one_tool(
                &call.name,
                args,
                tools_registry,
                observer,
                cancellation_token,
                &ctx,
                extras,
            )
            .await?,
        );
        if let Some(first) = results.last().cloned() {
            if should_retry_shell_elevation(&call.name, &first) {
                let elevation = if let Some(token) = cancellation_token {
                    tokio::select! {
                        () = token.cancelled() => {
                            abort_hitl(extras);
                            return Err(cancelled_err());
                        }
                        decision = gate.decide_elevation_async(&gate_call) => decision,
                    }
                } else {
                    gate.decide_elevation_async(&gate_call).await
                };
                match elevation {
                    GateDecision::Denied { message } => {
                        record_gate_denied(policy, &call.name, &call.arguments);
                        results.pop();
                        results.push(ToolBatchResult {
                            output: scrub_credentials(&message),
                            success: false,
                        });
                    }
                    GateDecision::Proceed {
                        shell_human_approved,
                    } => {
                        let mut retry_args =
                            normalize_tool_arguments(&call.name, call.arguments.clone());
                        let retry_ctx = match build_tool_execution_context(
                            &call.name,
                            &mut retry_args,
                            shell_human_approved,
                            human_input_hub,
                        ) {
                            Ok(ctx) => ctx.with_sandbox_elevated(true),
                            Err(err) => {
                                results.pop();
                                results.push(err);
                                continue;
                            }
                        };
                        results.pop();
                        results.push(
                            execute_one_tool(
                                &call.name,
                                retry_args,
                                tools_registry,
                                observer,
                                cancellation_token,
                                &retry_ctx,
                                extras,
                            )
                            .await?,
                        );
                    }
                }
            }
        }
    }

    Ok(results)
}

fn record_gate_denied(policy: Option<&PolicyHandle>, tool_name: &str, args: &serde_json::Value) {
    let Some(policy) = policy else {
        return;
    };
    let summary = shell_command_from_args(tool_name, args).unwrap_or(tool_name);
    let log = ToolReceiptLog::in_workspace(&policy.workspace_dir());
    if let Err(e) = log.record(tool_name, ReceiptDecision::Deny, summary, "gate", false) {
        tracing::warn!("tool receipt write failed: {e}");
    }
}

fn should_retry_shell_elevation(call_name: &str, result: &ToolBatchResult) -> bool {
    call_name == "shell"
        && !result.success
        && (result.output.contains("[sandbox_deny]") || result.output.contains("[needs_approval]"))
}

/// Execute a batch of tool calls with optional approval manager and security policy gate.
pub(crate) async fn execute_tool_batch(
    tool_calls: &[ParsedToolCall],
    tools_registry: &[Box<dyn Tool>],
    observer: &dyn Observer,
    approval: Option<&ApprovalManager>,
    security: Option<&PolicyHandle>,
    channel_name: &str,
    channel_approval: Option<ChannelApprovalSession>,
    cancellation_token: Option<&CancellationToken>,
    gate_extras: Option<&ToolBatchGateExtras>,
) -> Result<Vec<ToolBatchResult>> {
    if let (Some(mgr), Some(policy)) = (approval, security) {
        mgr.sync_security_profile(policy.profile());
    }
    let policy = security.cloned();
    let managed_gate = approval.map(|mgr| {
        let mut gate = ApprovalGate::new(mgr, channel_name, policy.clone());
        if let Some(session) = channel_approval {
            gate = gate.with_channel_session(session);
        }
        if let Some(hub) = gate_extras.and_then(|e| e.approval_hub.clone()) {
            gate = gate.with_hub(hub);
        }
        gate
    });

    let gate_ref: Option<&ApprovalGate<'_>> = managed_gate.as_ref();
    let host_phase = gate_extras.map(|e| e.host_phase).unwrap_or_default();

    // secret_slot resolution requires sequential execution (HITL store is not
    // safe to consume concurrently across a parallel batch).
    let should_parallel = should_execute_tools_in_parallel(tool_calls, gate_ref)
        && gate_extras
            .and_then(|e| e.human_input_hub.as_ref())
            .is_none();

    if should_parallel {
        return execute_tools_parallel(
            tool_calls,
            tools_registry,
            observer,
            cancellation_token,
            host_phase,
            gate_extras,
        )
        .await;
    }

    if let Some(gate) = gate_ref {
        execute_tools_sequential_with_gate(
            tool_calls,
            tools_registry,
            observer,
            gate,
            cancellation_token,
            gate_extras,
            host_phase,
            policy.as_ref(),
        )
        .await
    } else {
        execute_tools_sequential_no_gate(
            tool_calls,
            tools_registry,
            observer,
            cancellation_token,
            gate_extras,
            host_phase,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AutonomyConfig;

    #[test]
    fn should_execute_tools_in_parallel_returns_false_for_single_call() {
        let calls = vec![ParsedToolCall {
            name: "file_read".to_string(),
            arguments: serde_json::json!({"path": "a.txt"}),
        }];

        assert!(!should_execute_tools_in_parallel(&calls, None));
    }

    #[test]
    fn should_execute_tools_in_parallel_returns_false_when_gate_needs_approval() {
        let calls = vec![
            ParsedToolCall {
                name: "shell".to_string(),
                arguments: serde_json::json!({"command": "pwd"}),
            },
            ParsedToolCall {
                name: "http_request".to_string(),
                arguments: serde_json::json!({"url": "https://example.com"}),
            },
        ];
        let approval_cfg = AutonomyConfig::default();
        let approval_mgr = ApprovalManager::from_config(&approval_cfg);
        let gate = ApprovalGate::new(&approval_mgr, "cli", None);

        assert!(!should_execute_tools_in_parallel(&calls, Some(&gate)));
    }

    #[test]
    fn should_execute_tools_in_parallel_returns_false_for_shell_under_full() {
        let calls = vec![
            ParsedToolCall {
                name: "shell".to_string(),
                arguments: serde_json::json!({"command": "pwd"}),
            },
            ParsedToolCall {
                name: "file_read".to_string(),
                arguments: serde_json::json!({"path": "a.txt"}),
            },
        ];
        let mut approval_cfg = AutonomyConfig::default();
        approval_cfg.level = crate::security::AutonomyLevel::Full;
        approval_cfg.always_ask.clear();
        let approval_mgr = ApprovalManager::from_config(&approval_cfg);
        let gate = ApprovalGate::new(&approval_mgr, "cli", None);
        assert!(!approval_mgr.needs_approval("shell"));
        assert!(!should_execute_tools_in_parallel(&calls, Some(&gate)));
    }

    #[test]
    fn elevation_retry_detects_sandbox_deny() {
        let result = ToolBatchResult {
            output: "Error: [sandbox_deny] blocked".into(),
            success: false,
        };
        assert!(should_retry_shell_elevation("shell", &result));
        assert!(!should_retry_shell_elevation("file_read", &result));
        let deny = ToolBatchResult {
            output: "Error: [policy_deny] not allowed".into(),
            success: false,
        };
        assert!(
            !should_retry_shell_elevation("shell", &deny),
            "allowlist policy_deny is not elevation"
        );
    }

    #[tokio::test]
    async fn plan_phase_blocks_shell_without_executing() {
        let calls = vec![ParsedToolCall {
            name: "shell".to_string(),
            arguments: serde_json::json!({"command": "true"}),
        }];
        let extras = ToolBatchGateExtras {
            host_phase: HostPhase::Plan,
            ..Default::default()
        };
        let observer = crate::observability::NoopObserver;
        let results = execute_tool_batch(
            &calls,
            &[],
            &observer,
            None,
            None,
            "cli",
            None,
            None,
            Some(&extras),
        )
        .await
        .unwrap();
        assert_eq!(results.len(), 1);
        assert!(!results[0].success);
        assert!(results[0].output.contains("Plan phase"));
    }

    struct HangTool;

    #[async_trait::async_trait]
    impl Tool for HangTool {
        fn name(&self) -> &str {
            "hang"
        }

        fn description(&self) -> &str {
            "blocks until cancelled"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolExecutionContext,
        ) -> anyhow::Result<crate::tools::ToolResult> {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            Ok(crate::tools::ToolResult {
                success: true,
                output: "should not finish".into(),
                error: None,
            })
        }
    }

    #[tokio::test]
    async fn mid_tool_cancel_promotes_to_tool_loop_cancelled() {
        let calls = vec![ParsedToolCall {
            name: "hang".to_string(),
            arguments: serde_json::json!({}),
        }];
        let token = CancellationToken::new();
        token.cancel();
        let observer = crate::observability::NoopObserver;
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(HangTool)];
        let err = execute_tool_batch(
            &calls,
            &tools,
            &observer,
            None,
            None,
            "cli",
            None,
            Some(&token),
            None,
        )
        .await
        .expect_err("cancel must not return tool results");
        assert!(crate::agent::loop_::is_tool_loop_cancelled(&err));
    }

    #[tokio::test]
    async fn gate_denied_secret_path_writes_receipt() {
        let tmp = tempfile::tempdir().unwrap();
        let mut approval_cfg = AutonomyConfig::default();
        approval_cfg.level = crate::security::AutonomyLevel::Full;
        approval_cfg.always_ask.clear();
        let approval_mgr = ApprovalManager::from_config(&approval_cfg);
        let policy = PolicyHandle::new(crate::security::SecurityPolicy {
            autonomy: crate::security::AutonomyLevel::Full,
            workspace_dir: tmp.path().to_path_buf(),
            allowed_commands: vec!["cat".into()],
            secret_path_mode: crate::security::SecretPathMode::Deny,
            ..crate::security::SecurityPolicy::default()
        });
        let calls = vec![ParsedToolCall {
            name: "shell".to_string(),
            arguments: serde_json::json!({"command": "cat github_token_list.txt"}),
        }];
        let observer = crate::observability::NoopObserver;
        let results = execute_tool_batch(
            &calls,
            &[],
            &observer,
            Some(&approval_mgr),
            Some(&policy),
            "cli",
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(results.len(), 1);
        assert!(!results[0].success);
        assert!(
            results[0].output.contains("[policy_deny]"),
            "{}",
            results[0].output
        );
        let body =
            std::fs::read_to_string(tmp.path().join(".velaclaw/tool_receipts.jsonl")).unwrap();
        assert!(body.contains("\"decision\":\"deny\""));
        assert!(body.contains("github_token_list.txt"));
        let tok = format!("ghp_{}", "C".repeat(36));
        assert!(!body.contains(&tok));
    }

    #[test]
    fn scrub_credentials_redacts_api_key() {
        let input = "api_key: sk-1234567890abcdef";
        let scrubbed = scrub_credentials(input);
        assert!(scrubbed.contains("[REDACTED]"));
        assert!(!scrubbed.contains("1234567890abcdef"));
    }
}
