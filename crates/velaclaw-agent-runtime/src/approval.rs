//! Human approval gate contracts (VL-UR-002).
//! 人类批准门契约：工具级 backend + 可选 shell 策略 hook（实现在 app）。

use crate::dispatcher::ParsedToolCall;
use async_trait::async_trait;
use serde_json::Value;

/// Outcome of an approval gate check before tool execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateDecision {
    Proceed { shell_human_approved: bool },
    Denied { message: String },
}

/// Channel/backend adapter for interactive tool and shell approval (CLI, gateway, Telegram).
#[async_trait]
pub trait HumanApprovalBackend: Send + Sync {
    fn needs_tool_approval(&self, tool_name: &str) -> bool;

    /// Prompt for tool-level approval (sync path — CLI).
    fn approve_tool_sync(&self, tool_name: &str, arguments: &Value) -> bool;

    /// Prompt for tool-level approval (async path — gateway hub).
    async fn approve_tool_async(&self, tool_name: &str, arguments: &Value) -> bool;

    /// Whether interactive shell-policy prompts are available on this channel.
    fn interactive_shell_approval(&self) -> bool;

    /// Prompt for medium/high-risk shell command approval (sync path — CLI).
    fn approve_shell_command_sync(&self, command: &str) -> bool;

    /// Prompt for medium/high-risk shell command approval (async path — channel/gateway).
    async fn approve_shell_command_async(&self, command: &str) -> bool {
        self.approve_shell_command_sync(command)
    }

    /// Same stdin/hub as shell approval, used after sandbox/policy miss (VL-SEC-011).
    async fn approve_shell_elevation_async(&self, command: &str) -> bool {
        self.approve_shell_command_async(command).await
    }

    /// Session-scoped Always for risk prompts covering this command's executable basenames.
    fn shell_session_always_allowed(&self, command: &str) -> bool {
        let _ = command;
        false
    }

    /// Persistent Never for this tool name (no prompt; deny).
    fn never_tool(&self, tool_name: &str) -> bool {
        let _ = tool_name;
        false
    }

    /// Persistent Never for this shell command's executable basenames.
    fn shell_session_never(&self, command: &str) -> bool {
        let _ = command;
        false
    }
}

/// App-layer shell policy enforcement (`SecurityPolicy`); runtime only holds the slot.
pub trait ShellPolicyHook: Send + Sync {
    fn validate_shell_command(
        &self,
        tool_name: &str,
        arguments: &Value,
        human_approved: bool,
    ) -> Result<(), String>;
}

/// Unified gate: tool-level backend + optional shell hook.
pub struct ApprovalGate<'a, B: HumanApprovalBackend + ?Sized> {
    backend: &'a B,
    shell_hook: Option<&'a dyn ShellPolicyHook>,
}

impl<'a, B: HumanApprovalBackend + ?Sized> ApprovalGate<'a, B> {
    pub fn new(backend: &'a B, shell_hook: Option<&'a dyn ShellPolicyHook>) -> Self {
        Self {
            backend,
            shell_hook,
        }
    }

    pub fn needs_approval(&self, tool_name: &str) -> bool {
        self.backend.needs_tool_approval(tool_name)
    }

    pub fn decide_sync(&self, call: &ParsedToolCall) -> GateDecision {
        let prior_tool_level =
            is_shell_policy_tool(&call.name) && self.backend.needs_tool_approval(&call.name);
        if let Some(denied) = self.tool_level_decision_sync(call) {
            return denied;
        }
        self.shell_policy_decision(&call.name, &call.arguments, prior_tool_level)
    }

    pub async fn decide_async(&self, call: &ParsedToolCall) -> GateDecision {
        let prior_tool_level =
            is_shell_policy_tool(&call.name) && self.backend.needs_tool_approval(&call.name);
        if let Some(denied) = self.tool_level_decision_async(call).await {
            return denied;
        }
        self.shell_policy_decision_async(&call.name, &call.arguments, prior_tool_level)
            .await
    }

    /// Same gate, after `[sandbox_deny]` / `[needs_approval]` at execute (VL-SEC-011).
    /// Always prompts when interactive — including Full autonomy (routine skip does not apply).
    pub async fn decide_elevation_async(&self, call: &ParsedToolCall) -> GateDecision {
        if !is_shell_policy_tool(&call.name) {
            return GateDecision::Denied {
                message: "Elevation applies only to shell-policy tools.".into(),
            };
        }
        let Some(command) = shell_command_from_args(&call.name, &call.arguments) else {
            return GateDecision::Denied {
                message: "Elevation requires a shell command.".into(),
            };
        };
        if self.backend.shell_session_never(command) {
            return GateDecision::Denied {
                message: "Denied by policy (Never).".into(),
            };
        }
        if !self.backend.interactive_shell_approval() {
            return GateDecision::Denied {
                message: format!(
                    "Command requires explicit human approval: {command}. \
                     Interactive approval is not available on this channel."
                ),
            };
        }
        if self.backend.approve_shell_elevation_async(command).await {
            GateDecision::Proceed {
                shell_human_approved: true,
            }
        } else {
            GateDecision::Denied {
                message: "Denied by user.".into(),
            }
        }
    }

    fn tool_level_decision_sync(&self, call: &ParsedToolCall) -> Option<GateDecision> {
        if self.backend.never_tool(&call.name) {
            return Some(GateDecision::Denied {
                message: "Denied by policy (Never).".into(),
            });
        }
        if !self.backend.needs_tool_approval(&call.name) {
            return None;
        }
        if self.backend.approve_tool_sync(&call.name, &call.arguments) {
            None
        } else {
            Some(GateDecision::Denied {
                message: "Denied by user.".into(),
            })
        }
    }

    async fn tool_level_decision_async(&self, call: &ParsedToolCall) -> Option<GateDecision> {
        if self.backend.never_tool(&call.name) {
            return Some(GateDecision::Denied {
                message: "Denied by policy (Never).".into(),
            });
        }
        if !self.backend.needs_tool_approval(&call.name) {
            return None;
        }
        if self
            .backend
            .approve_tool_async(&call.name, &call.arguments)
            .await
        {
            None
        } else {
            Some(GateDecision::Denied {
                message: "Denied by user.".into(),
            })
        }
    }

    fn shell_policy_decision(
        &self,
        tool_name: &str,
        args: &Value,
        prior_tool_level_approval: bool,
    ) -> GateDecision {
        if !is_shell_policy_tool(tool_name) {
            return GateDecision::Proceed {
                shell_human_approved: false,
            };
        }
        let Some(hook) = self.shell_hook else {
            return GateDecision::Proceed {
                shell_human_approved: false,
            };
        };
        let Some(command) = shell_command_from_args(tool_name, args) else {
            return GateDecision::Proceed {
                shell_human_approved: false,
            };
        };
        if self.backend.shell_session_never(command) {
            return GateDecision::Denied {
                message: "Denied by policy (Never).".into(),
            };
        }
        let pre_approved =
            prior_tool_level_approval || self.backend.shell_session_always_allowed(command);
        if pre_approved && hook.validate_shell_command(tool_name, args, true).is_ok() {
            return GateDecision::Proceed {
                shell_human_approved: true,
            };
        }
        if hook.validate_shell_command(tool_name, args, false).is_ok() {
            return GateDecision::Proceed {
                shell_human_approved: false,
            };
        }
        // Allowlisted but needs risk approval vs hard deny (allowlist/safety).
        // Do not prompt when approve=true still fails — that would Yes then deny at tool.
        match hook.validate_shell_command(tool_name, args, true) {
            Ok(()) => {
                if self.backend.interactive_shell_approval() {
                    if self.backend.approve_shell_command_sync(command) {
                        GateDecision::Proceed {
                            shell_human_approved: true,
                        }
                    } else {
                        GateDecision::Denied {
                            message: "Denied by user.".into(),
                        }
                    }
                } else {
                    GateDecision::Denied {
                        message: format!(
                            "Command requires explicit human approval: {command}. \
                             Interactive approval is not available on this channel."
                        ),
                    }
                }
            }
            Err(message) => GateDecision::Denied { message },
        }
    }

    async fn shell_policy_decision_async(
        &self,
        tool_name: &str,
        args: &Value,
        prior_tool_level_approval: bool,
    ) -> GateDecision {
        if !is_shell_policy_tool(tool_name) {
            return GateDecision::Proceed {
                shell_human_approved: false,
            };
        }
        let Some(hook) = self.shell_hook else {
            return GateDecision::Proceed {
                shell_human_approved: false,
            };
        };
        let Some(command) = shell_command_from_args(tool_name, args) else {
            return GateDecision::Proceed {
                shell_human_approved: false,
            };
        };
        if self.backend.shell_session_never(command) {
            return GateDecision::Denied {
                message: "Denied by policy (Never).".into(),
            };
        }
        let pre_approved =
            prior_tool_level_approval || self.backend.shell_session_always_allowed(command);
        if pre_approved && hook.validate_shell_command(tool_name, args, true).is_ok() {
            return GateDecision::Proceed {
                shell_human_approved: true,
            };
        }
        if hook.validate_shell_command(tool_name, args, false).is_ok() {
            return GateDecision::Proceed {
                shell_human_approved: false,
            };
        }
        match hook.validate_shell_command(tool_name, args, true) {
            Ok(()) => {
                if self.backend.interactive_shell_approval() {
                    if self.backend.approve_shell_command_async(command).await {
                        GateDecision::Proceed {
                            shell_human_approved: true,
                        }
                    } else {
                        GateDecision::Denied {
                            message: "Denied by user.".into(),
                        }
                    }
                } else {
                    GateDecision::Denied {
                        message: format!(
                            "Command requires explicit human approval: {command}. \
                             Interactive approval is not available on this channel."
                        ),
                    }
                }
            }
            Err(message) => GateDecision::Denied { message },
        }
    }
}

pub fn is_shell_policy_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "shell" | "cron_add" | "cron_update" | "cron_run" | "schedule" | "file_read" | "file_write"
    )
}

pub fn shell_command_from_args<'a>(tool_name: &str, args: &'a Value) -> Option<&'a str> {
    match tool_name {
        "file_read" | "file_write" => args.get("path").and_then(|v| v.as_str()),
        "shell" | "cron_add" | "cron_update" | "cron_run" | "schedule" => {
            args.get("command").and_then(|v| v.as_str())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct AllowAll;

    #[async_trait]
    impl HumanApprovalBackend for AllowAll {
        fn needs_tool_approval(&self, _tool_name: &str) -> bool {
            false
        }
        fn approve_tool_sync(&self, _: &str, _: &Value) -> bool {
            true
        }
        async fn approve_tool_async(&self, _: &str, _: &Value) -> bool {
            true
        }
        fn interactive_shell_approval(&self) -> bool {
            true
        }
        fn approve_shell_command_sync(&self, _: &str) -> bool {
            true
        }
        async fn approve_shell_command_async(&self, _: &str) -> bool {
            true
        }
    }

    struct DenyShellHook;

    impl ShellPolicyHook for DenyShellHook {
        fn validate_shell_command(
            &self,
            _tool_name: &str,
            _arguments: &Value,
            human_approved: bool,
        ) -> Result<(), String> {
            if human_approved {
                Ok(())
            } else {
                Err("needs approval".into())
            }
        }
    }

    #[test]
    fn non_shell_tool_proceeds_without_approval() {
        let backend = AllowAll;
        let gate = ApprovalGate::new(&backend, Some(&DenyShellHook));
        let call = ParsedToolCall {
            name: "glob_search".into(),
            arguments: json!({"pattern": "*"}),
            tool_call_id: None,
        };
        assert_eq!(
            gate.decide_sync(&call),
            GateDecision::Proceed {
                shell_human_approved: false
            }
        );
    }

    #[test]
    fn shell_command_extracted_from_args() {
        let args = json!({"command": "echo hi"});
        assert_eq!(shell_command_from_args("shell", &args), Some("echo hi"));
        let path_args = json!({"path": "notes.md"});
        assert_eq!(
            shell_command_from_args("file_read", &path_args),
            Some("notes.md")
        );
    }
}
