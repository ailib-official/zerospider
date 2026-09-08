use super::traits::{Tool, ToolExecutionContext, ToolResult};
use crate::runtime::RuntimeAdapter;
use crate::security::{
    NoopSandbox, PolicyHandle, ReceiptDecision, Sandbox, SecurityPolicy, ToolReceiptLog,
};
use async_trait::async_trait;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// Maximum shell command execution time before kill.
const SHELL_TIMEOUT_SECS: u64 = 60;
/// Maximum output size in bytes (1MB).
const MAX_OUTPUT_BYTES: usize = 1_048_576;
/// Environment variables safe to pass to shell commands.
/// Only functional variables are included — never API keys or secrets.
const SAFE_ENV_VARS: &[&str] = &[
    "PATH", "HOME", "TERM", "LANG", "LC_ALL", "LC_CTYPE", "USER", "SHELL", "TMPDIR",
];
/// Operator-supplied tokens from the daemon process env (e.g. systemd `EnvironmentFile`).
/// Kept off `SAFE_ENV_VARS` so the secret-name lint stays honest. Values are never logged.
/// Injected only when the first allowlist segment is `gh` / `gh.exe` (same split as policy).
const OPERATOR_PASSTHROUGH_ENV_VARS: &[&str] = &["GH_TOKEN", "GITHUB_TOKEN"];

/// Shell command execution tool with sandboxing
pub struct ShellTool {
    security: PolicyHandle,
    runtime: Arc<dyn RuntimeAdapter>,
    sandbox: Arc<dyn Sandbox>,
    receipts: Option<ToolReceiptLog>,
}

impl ShellTool {
    /// Test and default-tools constructor: Noop sandbox, no receipts.
    pub fn new(security: PolicyHandle, runtime: Arc<dyn RuntimeAdapter>) -> Self {
        Self {
            security,
            runtime,
            sandbox: Arc::new(NoopSandbox),
            receipts: None,
        }
    }

    /// Production constructor: OS sandbox + workspace receipts.
    pub fn with_isolation(
        security: PolicyHandle,
        runtime: Arc<dyn RuntimeAdapter>,
        sandbox: Arc<dyn Sandbox>,
        receipts: Option<ToolReceiptLog>,
    ) -> Self {
        Self {
            security,
            runtime,
            sandbox,
            receipts,
        }
    }

    fn record_receipt(
        &self,
        decision: ReceiptDecision,
        command: &str,
        human_approved: bool,
        sandbox_name: &str,
    ) {
        if let Some(log) = &self.receipts {
            if let Err(e) = log.record("shell", decision, command, sandbox_name, human_approved) {
                tracing::warn!("tool receipt write failed: {e}");
            }
        }
    }

    /// Skip OS sandbox: privilege Once, elevation retry, or credential-path Once (VL-SEC-013).
    fn skip_os_sandbox(&self, ctx: &ToolExecutionContext, command: &str) -> bool {
        if !ctx.human_shell_approved {
            return false;
        }
        if self.security.escape_on_approval() || ctx.sandbox_elevated {
            return true;
        }
        crate::security::policy::command_touches_secret_material_in(
            command,
            Some(&self.security.workspace_dir()),
        )
    }
}

/// Restore env after `env_clear`, or leave daemon env when `inherit_process_env` (GOV-007: one site).
/// Isolated (`inherit=false`): SAFE_ENV_VARS plus `GH_TOKEN`/`GITHUB_TOKEN` only when the first
/// policy segment is `gh` (Landlock `GH_CONFIG_DIR` under the workspace). Local inherits all.
fn apply_shell_child_env(
    cmd: &mut tokio::process::Command,
    inherit_process_env: bool,
    command: &str,
    workspace: &Path,
) {
    let scratch = workspace.join(".velaclaw").join("tmp");
    if let Err(e) = std::fs::create_dir_all(&scratch) {
        tracing::warn!("could not create TMPDIR scratch: {e}");
    }
    if inherit_process_env {
        cmd.env("TMPDIR", &scratch);
        return;
    }
    cmd.env_clear();
    for var in SAFE_ENV_VARS {
        if let Ok(val) = std::env::var(var) {
            cmd.env(var, val);
        }
    }
    cmd.env("TMPDIR", &scratch);
    if !first_executable_is_github_cli(command) {
        return;
    }
    for var in OPERATOR_PASSTHROUGH_ENV_VARS {
        if let Ok(val) = std::env::var(var) {
            if !val.is_empty() {
                cmd.env(var, val);
            }
        }
    }
    // Landlock does not allow ~/.config/gh; gh still opens hosts.yml and fails with EACCES
    // even when GH_TOKEN is set. Point config at the workspace (already in the sandbox).
    let gh_config = workspace.join(".velaclaw").join("gh-config");
    if let Err(e) = std::fs::create_dir_all(&gh_config) {
        tracing::warn!("could not create GH_CONFIG_DIR: {e}");
    }
    cmd.env("GH_CONFIG_DIR", &gh_config);
}

/// First executable basename via [`SecurityPolicy::base_executables`] (GOV-007: no second parser).
fn first_executable_is_github_cli(command: &str) -> bool {
    matches!(
        SecurityPolicy::base_executables(command)
            .first()
            .map(String::as_str),
        Some("gh" | "gh.exe")
    )
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }

    fn description(&self) -> &str {
        "Execute a shell command in the workspace directory"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute"
                },
                "secret_slot": {
                    "type": "string",
                    "description": "Opaque one-shot slot from request_human_input (kind=secret). \
                     Pipelines the secret to stdin (use `sudo -S ...`). Never put passwords in command."
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolExecutionContext,
    ) -> anyhow::Result<ToolResult> {
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'command' parameter"))?;
        let human_approved = ctx.human_shell_approved;

        if self.security.is_rate_limited() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Rate limit exceeded: too many actions in the last hour".into()),
            });
        }

        match self
            .security
            .validate_command_execution(command, human_approved)
        {
            Ok(_) => {}
            Err(reason) => {
                self.record_receipt(
                    ReceiptDecision::Deny,
                    command,
                    human_approved,
                    self.sandbox.name(),
                );
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(reason),
                });
            }
        }

        if !self.security.record_action() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Rate limit exceeded: action budget exhausted".into()),
            });
        }

        // Clear the environment to prevent leaking unrelated secrets (CWE-200),
        // then restore SAFE_ENV_VARS. GH_TOKEN/GITHUB_TOKEN only when the first
        // policy segment is `gh` / `gh.exe` (same `base_executables` as allowlist).
        let mut cmd = match self
            .runtime
            .build_shell_command(command, &self.security.workspace_dir())
        {
            Ok(cmd) => cmd,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Failed to build runtime command: {e}")),
                });
            }
        };
        apply_shell_child_env(
            &mut cmd,
            self.security.inherit_process_env(),
            command,
            &self.security.workspace_dir(),
        );

        let skip_sandbox = self.skip_os_sandbox(ctx, command);
        let sandbox_name = if skip_sandbox {
            "none(approved-escape)"
        } else {
            self.sandbox.name()
        };

        if !skip_sandbox {
            if let Err(e) = self.sandbox.wrap_command(cmd.as_std_mut()) {
                self.record_receipt(
                    ReceiptDecision::SandboxFail,
                    command,
                    human_approved,
                    sandbox_name,
                );
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(sandbox_deny_message(
                        &format!("Sandbox wrap failed: {e}"),
                        sandbox_name,
                    )),
                });
            }
        }

        self.record_receipt(
            ReceiptDecision::Allow,
            command,
            human_approved,
            sandbox_name,
        );

        let stdin_secret = ctx.stdin_secret.clone();
        let result = tokio::time::timeout(Duration::from_secs(SHELL_TIMEOUT_SECS), async {
            run_shell_command(cmd, stdin_secret).await
        })
        .await;

        match result {
            Ok(Ok(output)) => {
                let mut stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let mut stderr = String::from_utf8_lossy(&output.stderr).to_string();

                // Truncate output to prevent OOM
                if stdout.len() > MAX_OUTPUT_BYTES {
                    let n = crate::util::floor_char_boundary(&stdout, MAX_OUTPUT_BYTES);
                    stdout.truncate(n);
                    stdout.push_str("\n... [output truncated at 1MB]");
                }
                if stderr.len() > MAX_OUTPUT_BYTES {
                    let n = crate::util::floor_char_boundary(&stderr, MAX_OUTPUT_BYTES);
                    stderr.truncate(n);
                    stderr.push_str("\n... [stderr truncated at 1MB]");
                }

                let success = output.status.success();
                let combined = format!("{stdout}\n{stderr}");
                if !success
                    && should_label_child_sandbox_deny(
                        sandbox_name,
                        skip_sandbox,
                        command,
                        &self.security.workspace_dir(),
                        &combined,
                    )
                {
                    return Ok(ToolResult {
                        success: false,
                        output: stdout,
                        error: Some(sandbox_deny_message(stderr.trim(), sandbox_name)),
                    });
                }
                Ok(ToolResult {
                    success,
                    output: stdout,
                    error: if stderr.is_empty() {
                        None
                    } else {
                        Some(stderr)
                    },
                })
            }
            Ok(Err(e)) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Failed to execute command: {e}")),
            }),
            Err(_) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Command timed out after {SHELL_TIMEOUT_SECS}s and was killed"
                )),
            }),
        }
    }
}

async fn run_shell_command(
    mut cmd: tokio::process::Command,
    stdin_secret: Option<String>,
) -> std::io::Result<std::process::Output> {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;

    // Turn Stop drops this future; kill the child of *this* turn (not an allowlist change).
    cmd.kill_on_drop(true);

    if stdin_secret.is_some() {
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        let mut child = cmd.spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            if let Some(secret) = stdin_secret {
                // `sudo -S` reads a password line from stdin.
                let _ = stdin.write_all(secret.as_bytes()).await;
                let _ = stdin.write_all(b"\n").await;
                let _ = stdin.shutdown().await;
            }
        }
        return child.wait_with_output().await;
    }

    cmd.output().await
}

fn looks_like_eacces_or_nnp(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("permission denied")
        || lower.contains("operation not permitted")
        || lower.contains("no new privileges")
}

/// Relabel child stderr as `[sandbox_deny]` only when the OS sandbox is active and
/// the failure is NNP or a path outside typical Landlock roots — not every DAC EACCES.
fn should_label_child_sandbox_deny(
    sandbox_name: &str,
    skip_sandbox: bool,
    command: &str,
    workspace_dir: &std::path::Path,
    combined_output: &str,
) -> bool {
    if skip_sandbox || sandbox_name == "none" || sandbox_name.starts_with("none(") {
        return false;
    }
    if !looks_like_eacces_or_nnp(combined_output) {
        return false;
    }
    let lower = combined_output.to_ascii_lowercase();
    if lower.contains("no new privileges") {
        return true;
    }
    command_has_path_outside_typical_landlock_roots(command, workspace_dir)
}

/// Keep aligned with `src/security/landlock.rs` `apply_restrictions` (classification only).
fn command_has_path_outside_typical_landlock_roots(
    command: &str,
    workspace_dir: &std::path::Path,
) -> bool {
    let home = std::env::var("HOME").ok();
    for raw in command.split_whitespace() {
        let token = raw.trim_matches(|c| c == '\'' || c == '"' || c == '`');
        if token.is_empty() || token == "-" || token.starts_with('-') {
            continue;
        }
        let expanded = if let Some(rest) = token.strip_prefix("~/") {
            match &home {
                Some(h) => format!("{h}/{rest}"),
                None => continue,
            }
        } else if token == "~" {
            match &home {
                Some(h) => h.clone(),
                None => continue,
            }
        } else if let Some(rest) = token.strip_prefix("./") {
            workspace_dir.join(rest).to_string_lossy().into_owned()
        } else if token.starts_with('/') {
            token.to_string()
        } else {
            // Bare filename: shell cwd is the workspace; do not treat as host escape.
            continue;
        };
        let path = std::path::Path::new(&expanded);
        if !path_typically_allowed_under_landlock(path, workspace_dir, home.as_deref()) {
            return true;
        }
    }
    false
}

fn path_typically_allowed_under_landlock(
    path: &std::path::Path,
    workspace_dir: &std::path::Path,
    home: Option<&str>,
) -> bool {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let mut roots: Vec<std::path::PathBuf> = vec![workspace_dir.to_path_buf()];
    for p in [
        "/tmp",
        "/bin",
        "/usr",
        "/lib",
        "/lib64",
        "/etc",
        "/dev",
        "/var/lib/apt",
        "/var/lib/dpkg",
    ] {
        roots.push(std::path::PathBuf::from(p));
    }
    if let Some(h) = home {
        roots.push(std::path::Path::new(h).join(".ssh"));
    }
    roots.iter().any(|root| path.starts_with(root))
}

fn sandbox_deny_message(detail: &str, sandbox_name: &str) -> String {
    let detail = if detail.is_empty() {
        "(no stderr)".to_string()
    } else {
        detail.to_string()
    };
    format!(
        "[sandbox_deny] OS sandbox (`{sandbox_name}`) blocked this command.\n\
         Detail: {detail}\n\n\
         Next steps:\n\
         1. Copy the needed file into the workspace and use `file_read` (or `cat` there).\n\
         2. Approve Once in the same ApprovalHub modal to retry this invocation (may skip Landlock for that call).\n\
         3. Do not retry `ls`/`find`/`cat` on the same path without approval — the sandbox result will not change.\n\
         Human approval does not enlarge `allowed_commands` (VL-SEC-009)."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{NativeRuntime, RuntimeAdapter};
    use crate::security::{AutonomyLevel, PolicyHandle, SecurityPolicy};

    fn test_security(autonomy: AutonomyLevel) -> PolicyHandle {
        PolicyHandle::new(SecurityPolicy {
            autonomy,
            workspace_dir: std::env::temp_dir(),
            ..SecurityPolicy::default()
        })
    }

    fn test_runtime() -> Arc<dyn RuntimeAdapter> {
        Arc::new(NativeRuntime::new())
    }

    #[test]
    fn shell_tool_name() {
        let tool = ShellTool::new(test_security(AutonomyLevel::Supervised), test_runtime());
        assert_eq!(tool.name(), "shell");
    }

    #[test]
    fn shell_tool_description() {
        let tool = ShellTool::new(test_security(AutonomyLevel::Supervised), test_runtime());
        assert!(!tool.description().is_empty());
    }

    #[test]
    fn shell_tool_schema_has_command() {
        let tool = ShellTool::new(test_security(AutonomyLevel::Supervised), test_runtime());
        let schema = tool.parameters_schema();
        assert!(schema["properties"]["command"].is_object());
        assert!(schema["required"]
            .as_array()
            .expect("schema required field should be an array")
            .contains(&json!("command")));
        assert!(schema["properties"].get("approved").is_none());
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn shell_executes_allowed_command() {
        let tool = ShellTool::new(test_security(AutonomyLevel::Supervised), test_runtime());
        let result = tool
            .execute(
                json!({"command": "echo hello"}),
                &ToolExecutionContext::default(),
            )
            .await
            .expect("echo command execution should succeed");
        assert!(result.success);
        assert!(result.output.trim().contains("hello"));
        assert!(result.error.is_none());
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn shell_blocks_disallowed_command() {
        let tool = ShellTool::new(test_security(AutonomyLevel::Supervised), test_runtime());
        let result = tool
            .execute(
                json!({"command": "rm -rf /"}),
                &ToolExecutionContext::default(),
            )
            .await
            .expect("disallowed command execution should return a result");
        assert!(!result.success);
        let error = result.error.as_deref().unwrap_or("");
        assert!(error.contains("not allowed") || error.contains("high-risk"));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn shell_blocks_readonly() {
        let tool = ShellTool::new(test_security(AutonomyLevel::ReadOnly), test_runtime());
        let result = tool
            .execute(json!({"command": "ls"}), &ToolExecutionContext::default())
            .await
            .expect("readonly command execution should return a result");
        assert!(!result.success);
        assert!(result
            .error
            .as_ref()
            .expect("error field should be present for blocked command")
            .contains("read-only mode"));
    }

    #[tokio::test]
    async fn shell_missing_command_param() {
        let tool = ShellTool::new(test_security(AutonomyLevel::Supervised), test_runtime());
        let result = tool
            .execute(json!({}), &ToolExecutionContext::default())
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("command"));
    }

    #[tokio::test]
    async fn shell_wrong_type_param() {
        let tool = ShellTool::new(test_security(AutonomyLevel::Supervised), test_runtime());
        let result = tool
            .execute(json!({"command": 123}), &ToolExecutionContext::default())
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn shell_captures_exit_code() {
        let tool = ShellTool::new(test_security(AutonomyLevel::Supervised), test_runtime());
        let result = tool
            .execute(
                json!({"command": "ls /nonexistent_dir_xyz"}),
                &ToolExecutionContext::default(),
            )
            .await
            .expect("command with nonexistent path should return a result");
        assert!(!result.success);
    }

    fn test_security_with_env_cmd() -> PolicyHandle {
        PolicyHandle::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            workspace_dir: std::env::temp_dir(),
            allowed_commands: vec!["env".into(), "echo".into()],
            ..SecurityPolicy::default()
        })
    }

    /// RAII guard that restores an environment variable to its original state on drop,
    /// ensuring cleanup even if the test panics.
    struct EnvGuard {
        key: &'static str,
        original: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let original = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self { key, original }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.original {
                Some(val) => std::env::set_var(self.key, val),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    #[cfg(unix)]
    async fn shell_does_not_leak_api_key() {
        let _g1 = EnvGuard::set("API_KEY", "sk-test-secret-12345");
        let _g2 = EnvGuard::set("VELACLAW_API_KEY", "sk-test-secret-67890");

        let tool = ShellTool::new(test_security_with_env_cmd(), test_runtime());
        let result = tool
            .execute(json!({"command": "env"}), &ToolExecutionContext::default())
            .await
            .expect("env command execution should succeed");
        assert!(result.success);
        assert!(
            !result.output.contains("sk-test-secret-12345"),
            "API_KEY leaked to shell command output"
        );
        assert!(
            !result.output.contains("sk-test-secret-67890"),
            "VELACLAW_API_KEY leaked to shell command output"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn shell_preserves_path_and_home() {
        let tool = ShellTool::new(test_security_with_env_cmd(), test_runtime());

        let result = tool
            .execute(
                json!({"command": "echo $HOME"}),
                &ToolExecutionContext::default(),
            )
            .await
            .expect("echo HOME command should succeed");
        assert!(result.success);
        assert!(
            !result.output.trim().is_empty(),
            "HOME should be available in shell"
        );

        let result = tool
            .execute(
                json!({"command": "echo $PATH"}),
                &ToolExecutionContext::default(),
            )
            .await
            .expect("echo PATH command should succeed");
        assert!(result.success);
        assert!(
            !result.output.trim().is_empty(),
            "PATH should be available in shell"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn shell_requires_approval_for_medium_risk_command() {
        let security = PolicyHandle::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            allowed_commands: vec!["touch".into()],
            workspace_dir: std::env::temp_dir(),
            ..SecurityPolicy::default()
        });

        let tool = ShellTool::new(security.clone(), test_runtime());
        let denied = tool
            .execute(
                json!({"command": "touch velaclaw_shell_approval_test"}),
                &ToolExecutionContext::default(),
            )
            .await
            .expect("unapproved command should return a result");
        assert!(!denied.success);
        assert!(denied
            .error
            .as_deref()
            .unwrap_or("")
            .contains("requires explicit human approval"));

        let allowed = tool
            .execute(
                json!({"command": "touch velaclaw_shell_approval_test"}),
                &ToolExecutionContext::with_shell_human_approved(true),
            )
            .await
            .expect("approved command execution should succeed");
        assert!(allowed.success);

        let _ =
            tokio::fs::remove_file(std::env::temp_dir().join("velaclaw_shell_approval_test")).await;
    }

    // ── §5.2 Shell timeout enforcement tests ─────────────────

    #[test]
    fn shell_timeout_constant_is_reasonable() {
        assert_eq!(SHELL_TIMEOUT_SECS, 60, "shell timeout must be 60 seconds");
    }

    #[test]
    fn shell_output_limit_is_1mb() {
        assert_eq!(
            MAX_OUTPUT_BYTES, 1_048_576,
            "max output must be 1 MB to prevent OOM"
        );
    }

    // ── §5.3 Non-UTF8 binary output tests ────────────────────

    #[test]
    fn shell_safe_env_vars_excludes_secrets() {
        for var in SAFE_ENV_VARS {
            let lower = var.to_lowercase();
            assert!(
                !lower.contains("key") && !lower.contains("secret") && !lower.contains("token"),
                "SAFE_ENV_VARS must not include sensitive variable: {var}"
            );
        }
    }

    #[test]
    fn shell_safe_env_vars_includes_essentials() {
        assert!(
            SAFE_ENV_VARS.contains(&"PATH"),
            "PATH must be in safe env vars"
        );
        assert!(
            SAFE_ENV_VARS.contains(&"HOME"),
            "HOME must be in safe env vars"
        );
        assert!(
            SAFE_ENV_VARS.contains(&"TERM"),
            "TERM must be in safe env vars"
        );
    }

    #[test]
    fn shell_operator_passthrough_is_gh_tokens_only() {
        assert_eq!(OPERATOR_PASSTHROUGH_ENV_VARS, &["GH_TOKEN", "GITHUB_TOKEN"]);
        for var in OPERATOR_PASSTHROUGH_ENV_VARS {
            assert!(
                !SAFE_ENV_VARS.contains(var),
                "{var} must stay off SAFE_ENV_VARS"
            );
        }
    }

    #[test]
    fn gh_passthrough_uses_policy_first_executable() {
        assert!(first_executable_is_github_cli("gh pr view 1"));
        assert!(first_executable_is_github_cli("/usr/bin/gh api user"));
        assert!(first_executable_is_github_cli("FOO=1 gh pr list"));
        assert!(first_executable_is_github_cli("gh.exe pr view"));
        assert!(!first_executable_is_github_cli("echo $GH_TOKEN"));
        assert!(!first_executable_is_github_cli("env"));
        assert!(!first_executable_is_github_cli(
            "git status && gh pr create"
        ));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn shell_does_not_pass_gh_token_to_non_gh_command() {
        let prev = std::env::var("GH_TOKEN").ok();
        std::env::set_var("GH_TOKEN", "test-gh-token-fixture");
        let security = PolicyHandle::new(SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            allowed_commands: vec!["echo".into()],
            workspace_dir: std::env::temp_dir(),
            ..SecurityPolicy::default()
        });
        let tool = ShellTool::new(security, test_runtime());
        let result = tool
            .execute(
                json!({"command": "echo $GH_TOKEN"}),
                &ToolExecutionContext::default(),
            )
            .await
            .expect("echo GH_TOKEN");
        match prev {
            Some(v) => std::env::set_var("GH_TOKEN", v),
            None => std::env::remove_var("GH_TOKEN"),
        }
        assert!(result.success, "{:?}", result.error);
        assert!(
            !result.output.contains("test-gh-token-fixture"),
            "non-gh child must not inherit GH_TOKEN; got {:?}",
            result.output
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn shell_passes_gh_token_when_first_executable_is_gh() {
        let stub_dir = std::env::temp_dir().join(format!("vl-gh-stub-{}", std::process::id()));
        std::fs::create_dir_all(&stub_dir).expect("stub dir");
        let stub = stub_dir.join("gh");
        std::fs::write(
            &stub,
            "#!/bin/sh\nprintf '%s\\n' \"$GH_TOKEN\"\nprintf '%s\\n' \"$GH_CONFIG_DIR\"\n",
        )
        .expect("stub gh");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        let prev_token = std::env::var("GH_TOKEN").ok();
        let prev_path = std::env::var("PATH").ok();
        std::env::set_var("GH_TOKEN", "test-gh-token-fixture");
        std::env::set_var(
            "PATH",
            format!(
                "{}:{}",
                stub_dir.display(),
                prev_path.as_deref().unwrap_or("")
            ),
        );
        let security = PolicyHandle::new(SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            allowed_commands: vec!["gh".into()],
            workspace_dir: std::env::temp_dir(),
            ..SecurityPolicy::default()
        });
        let tool = ShellTool::new(security, test_runtime());
        let result = tool
            .execute(json!({"command": "gh"}), &ToolExecutionContext::default())
            .await
            .expect("stub gh");
        match prev_token {
            Some(v) => std::env::set_var("GH_TOKEN", v),
            None => std::env::remove_var("GH_TOKEN"),
        }
        match prev_path {
            Some(v) => std::env::set_var("PATH", v),
            None => std::env::remove_var("PATH"),
        }
        let _ = std::fs::remove_file(&stub);
        let _ = std::fs::remove_dir(&stub_dir);
        assert!(result.success, "{:?}", result.error);
        assert!(
            result.output.contains("test-gh-token-fixture"),
            "gh child must inherit GH_TOKEN; got {:?}",
            result.output
        );
        assert!(
            result.output.contains("gh-config"),
            "gh child must get workspace GH_CONFIG_DIR; got {:?}",
            result.output
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn shell_blocks_rate_limited() {
        let security = PolicyHandle::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            max_actions_per_hour: 0,
            workspace_dir: std::env::temp_dir(),
            ..SecurityPolicy::default()
        });
        let tool = ShellTool::new(security, test_runtime());
        let result = tool
            .execute(
                json!({"command": "echo test"}),
                &ToolExecutionContext::default(),
            )
            .await
            .expect("rate-limited command should return a result");
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("Rate limit"));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn shell_pipes_stdin_secret_to_command() {
        let security = PolicyHandle::new(SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            allowed_commands: vec!["cat".into()],
            workspace_dir: std::env::temp_dir(),
            ..SecurityPolicy::default()
        });
        let tool = ShellTool::new(security, test_runtime());
        let ctx = ToolExecutionContext::with_shell_human_approved(true)
            .with_stdin_secret(Some("slot-secret".into()));
        let result = tool
            .execute(json!({"command": "cat"}), &ctx)
            .await
            .expect("cat with stdin");
        assert!(result.success, "{:?}", result.error);
        assert!(result.output.contains("slot-secret"));
    }

    struct RecordingSandbox {
        wraps: std::sync::atomic::AtomicU32,
    }

    impl crate::security::Sandbox for RecordingSandbox {
        fn wrap_command(&self, _cmd: &mut std::process::Command) -> std::io::Result<()> {
            self.wraps.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }

        fn is_available(&self) -> bool {
            true
        }

        fn name(&self) -> &str {
            "recording"
        }

        fn description(&self) -> &str {
            "test double"
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn allowlisted_command_still_wraps_sandbox() {
        let recorder = Arc::new(RecordingSandbox {
            wraps: std::sync::atomic::AtomicU32::new(0),
        });
        let tool = ShellTool::with_isolation(
            test_security(AutonomyLevel::Supervised),
            test_runtime(),
            recorder.clone(),
            None,
        );
        let result = tool
            .execute(
                json!({"command": "echo hello"}),
                &ToolExecutionContext::default(),
            )
            .await
            .expect("echo");
        assert!(result.success, "{:?}", result.error);
        assert_eq!(recorder.wraps.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn approved_escape_skips_sandbox_wrap() {
        let recorder = Arc::new(RecordingSandbox {
            wraps: std::sync::atomic::AtomicU32::new(0),
        });
        let security = PolicyHandle::new(SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            workspace_dir: std::env::temp_dir(),
            allowed_commands: vec!["echo".into()],
            escape_on_approval: true,
            ..SecurityPolicy::default()
        });
        let tool = ShellTool::with_isolation(security, test_runtime(), recorder.clone(), None);
        let result = tool
            .execute(
                json!({"command": "echo hello"}),
                &ToolExecutionContext::with_shell_human_approved(true),
            )
            .await
            .expect("echo");
        assert!(result.success, "{:?}", result.error);
        assert_eq!(recorder.wraps.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn sandbox_elevated_skips_wrap_without_escape_flag() {
        let recorder = Arc::new(RecordingSandbox {
            wraps: std::sync::atomic::AtomicU32::new(0),
        });
        let security = PolicyHandle::new(SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            workspace_dir: std::env::temp_dir(),
            allowed_commands: vec!["echo".into()],
            escape_on_approval: false,
            ..SecurityPolicy::default()
        });
        let tool = ShellTool::with_isolation(security, test_runtime(), recorder.clone(), None);
        let result = tool
            .execute(
                json!({"command": "echo hello"}),
                &ToolExecutionContext::with_shell_human_approved(true).with_sandbox_elevated(true),
            )
            .await
            .expect("echo");
        assert!(result.success, "{:?}", result.error);
        assert_eq!(recorder.wraps.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn escape_on_approval_without_human_still_wraps() {
        let recorder = Arc::new(RecordingSandbox {
            wraps: std::sync::atomic::AtomicU32::new(0),
        });
        let security = PolicyHandle::new(SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            workspace_dir: std::env::temp_dir(),
            allowed_commands: vec!["echo".into()],
            escape_on_approval: true,
            ..SecurityPolicy::default()
        });
        let tool = ShellTool::with_isolation(security, test_runtime(), recorder.clone(), None);
        let result = tool
            .execute(
                json!({"command": "echo hello"}),
                &ToolExecutionContext::default(),
            )
            .await
            .expect("echo");
        assert!(result.success, "{:?}", result.error);
        assert_eq!(recorder.wraps.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn non_allowlisted_denied_when_approved_does_not_wrap() {
        let recorder = Arc::new(RecordingSandbox {
            wraps: std::sync::atomic::AtomicU32::new(0),
        });
        let security = PolicyHandle::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            workspace_dir: std::env::temp_dir(),
            allowed_commands: vec!["echo".into()],
            ..SecurityPolicy::default()
        });
        let tool = ShellTool::with_isolation(security, test_runtime(), recorder.clone(), None);
        let result = tool
            .execute(
                json!({"command": "python3 -c 'print(1)'"}),
                &ToolExecutionContext::with_shell_human_approved(true),
            )
            .await
            .expect("deny");
        assert!(!result.success);
        assert_eq!(recorder.wraps.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn fail_closed_sandbox_blocks_allowlisted_command() {
        let tool = ShellTool::with_isolation(
            test_security(AutonomyLevel::Supervised),
            test_runtime(),
            Arc::new(crate::security::FailClosedSandbox),
            None,
        );
        let result = tool
            .execute(
                json!({"command": "echo hello"}),
                &ToolExecutionContext::default(),
            )
            .await
            .expect("result");
        assert!(!result.success);
        assert!(result
            .error
            .as_deref()
            .unwrap_or("")
            .contains("[sandbox_deny]"));
    }

    #[test]
    fn child_permission_denied_in_workspace_is_not_sandbox_deny() {
        let ws = std::env::temp_dir();
        assert!(!should_label_child_sandbox_deny(
            "landlock",
            false,
            "cat ./chmod-000-file",
            &ws,
            "cat: ./chmod-000-file: Permission denied",
        ));
    }

    #[test]
    fn child_permission_denied_on_home_path_is_sandbox_deny() {
        assert!(should_label_child_sandbox_deny(
            "landlock",
            false,
            "cat /home/alex/notes.txt",
            &std::env::temp_dir(),
            "cat: /home/alex/notes.txt: Permission denied",
        ));
    }

    #[test]
    fn noop_sandbox_does_not_relabel_permission_denied() {
        assert!(!should_label_child_sandbox_deny(
            "none",
            false,
            "cat /home/alex/notes.txt",
            &std::env::temp_dir(),
            "Permission denied",
        ));
    }

    #[test]
    fn nnp_sudo_message_is_sandbox_deny() {
        assert!(should_label_child_sandbox_deny(
            "landlock",
            false,
            "sudo apt update",
            &std::env::temp_dir(),
            "sudo: The \"no new privileges\" flag is set, which prevents sudo from running as root.",
        ));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn receipts_record_allow_and_deny() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().to_path_buf();
        let receipts = crate::security::ToolReceiptLog::in_workspace(&workspace);
        let security = PolicyHandle::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            workspace_dir: workspace.clone(),
            allowed_commands: vec!["echo".into()],
            ..SecurityPolicy::default()
        });
        let tool = ShellTool::with_isolation(
            security,
            test_runtime(),
            Arc::new(crate::security::NoopSandbox),
            Some(receipts.clone()),
        );
        let allowed = tool
            .execute(
                json!({"command": "echo hello"}),
                &ToolExecutionContext::default(),
            )
            .await
            .unwrap();
        assert!(allowed.success);
        let denied = tool
            .execute(
                json!({"command": "python3 -c 'print(1)'"}),
                &ToolExecutionContext::with_shell_human_approved(true),
            )
            .await
            .unwrap();
        assert!(!denied.success);
        let body = std::fs::read_to_string(receipts.path()).unwrap();
        assert!(body.contains("\"decision\":\"allow\""));
        assert!(body.contains("\"decision\":\"deny\""));
        assert!(!body.contains("slot-secret"));
    }
}
