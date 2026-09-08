//! Interactive approval workflow for supervised mode.
//!
//! Provides a pre-execution hook that prompts the user before tool calls,
//! with session-scoped "Always" allowlists and audit logging.

mod backend;
mod channel_hub;
mod gate;
mod hub;
mod human_input;
mod secret_slots;

pub use backend::{
    ChannelApprovalSession, DenyApprovalBackend, ManagerApprovalBackend, PolicyHandleShellHook,
    SecurityPolicyShellHook,
};
pub use channel_hub::ChannelApprovalHub;
pub use gate::{ApprovalGate, GateDecision};
pub use hub::ApprovalHub;
pub use human_input::{
    HumanInputHub, HumanInputKind, HumanInputOutcome, HumanInputRequest, HumanInputRequiredEvent,
    HumanInputRespondBody,
};
pub use secret_slots::SecretSlotStore;

use crate::config::AutonomyConfig;
use crate::config::PolicyOverridesStore;
use crate::security::audit::AuditLogger;
use crate::security::AutonomyLevel;
use chrono::Utc;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::{self, BufRead, Write};
use std::sync::Arc;

// ── Types ────────────────────────────────────────────────────────

/// A request to approve a tool call before execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub tool_name: String,
    pub arguments: serde_json::Value,
    /// True when this prompt is post-execute elevation (sandbox/policy miss).
    #[serde(default)]
    pub elevation: bool,
}

/// The user's response to an approval request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalResponse {
    /// Execute this one call.
    Yes,
    /// Deny this call.
    No,
    /// Execute and add tool to session-scoped allowlist (shell: also remember executable basename).
    Always,
    /// Persist denylist; do not prompt again for this tool/basename until cleared.
    Never,
}

/// A single audit log entry for an approval decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalLogEntry {
    pub timestamp: String,
    pub tool_name: String,
    pub arguments_summary: String,
    pub decision: ApprovalResponse,
    pub channel: String,
}

// ── ApprovalManager ──────────────────────────────────────────────

/// Manages the interactive approval workflow.
///
/// - Checks config-level `auto_approve` / `always_ask` lists
/// - Maintains a session-scoped "always" allowlist (tools) and shell binary set
/// - Records an audit trail of all decisions
pub struct ApprovalManager {
    /// Tools that never need approval (from config).
    auto_approve: HashSet<String>,
    /// Tools that always need approval, ignoring session allowlist.
    always_ask: HashSet<String>,
    /// Autonomy level from config.
    autonomy_level: AutonomyLevel,
    /// Session-scoped allowlist built from "Always" responses (tool names).
    session_allowlist: Mutex<HashSet<String>>,
    /// Session-scoped shell executable basenames from shell-policy "Always" (VL-SEC-009).
    session_shell_binaries: Mutex<HashSet<String>>,
    /// Persistent Never (tools).
    session_denylist: Mutex<HashSet<String>>,
    /// Persistent Never (shell basenames).
    session_shell_denylist: Mutex<HashSet<String>>,
    /// Last seen `[security.profile]` — change drops in-memory Always grants.
    last_profile: Mutex<Option<crate::config::SecurityProfile>>,
    profile_seen: Mutex<bool>,
    /// Audit trail of approval decisions.
    audit_log: Mutex<Vec<ApprovalLogEntry>>,
    /// L2.5 persistence for "Always" decisions (VL-SEC-004).
    overrides_store: Option<Arc<PolicyOverridesStore>>,
    /// Optional `security.audit` bridge (VL-SEC-004).
    security_audit: Option<Arc<AuditLogger>>,
}

impl ApprovalManager {
    /// Create from autonomy config.
    pub fn from_config(config: &AutonomyConfig) -> Self {
        Self {
            auto_approve: config.auto_approve.iter().cloned().collect(),
            always_ask: config.always_ask.iter().cloned().collect(),
            autonomy_level: config.level,
            session_allowlist: Mutex::new(HashSet::new()),
            session_shell_binaries: Mutex::new(HashSet::new()),
            session_denylist: Mutex::new(HashSet::new()),
            session_shell_denylist: Mutex::new(HashSet::new()),
            last_profile: Mutex::new(None),
            profile_seen: Mutex::new(false),
            audit_log: Mutex::new(Vec::new()),
            overrides_store: None,
            security_audit: None,
        }
    }

    pub fn with_overrides_store(mut self, store: Arc<PolicyOverridesStore>) -> Self {
        self.overrides_store = Some(store);
        self
    }

    pub fn with_security_audit(mut self, audit: Arc<AuditLogger>) -> Self {
        self.security_audit = Some(audit);
        self
    }

    /// Seed shell-policy Always basenames from L2.5 on manager spawn.
    pub fn seed_session_shell_binaries<I>(&self, binaries: I)
    where
        I: IntoIterator<Item = String>,
    {
        let mut set = self.session_shell_binaries.lock();
        for b in binaries {
            let trimmed = b.trim();
            if !trimmed.is_empty() {
                set.insert(trimmed.to_string());
            }
        }
    }

    pub fn seed_session_denylist<I>(&self, tools: I)
    where
        I: IntoIterator<Item = String>,
    {
        let mut set = self.session_denylist.lock();
        for t in tools {
            let trimmed = t.trim();
            if !trimmed.is_empty() {
                set.insert(trimmed.to_string());
            }
        }
    }

    pub fn seed_session_shell_denylist<I>(&self, binaries: I)
    where
        I: IntoIterator<Item = String>,
    {
        let mut set = self.session_shell_denylist.lock();
        for b in binaries {
            let trimmed = b.trim();
            if !trimmed.is_empty() {
                set.insert(trimmed.to_string());
            }
        }
    }

    /// Drop in-memory Always grants when `[security.profile]` changes.
    pub fn sync_security_profile(&self, profile: Option<crate::config::SecurityProfile>) {
        let mut seen = self.profile_seen.lock();
        let mut last = self.last_profile.lock();
        if !*seen {
            *last = profile;
            *seen = true;
            return;
        }
        if *last != profile {
            self.session_allowlist.lock().clear();
            self.session_shell_binaries.lock().clear();
            *last = profile;
        }
    }

    pub fn is_never_tool(&self, tool_name: &str) -> bool {
        self.session_denylist.lock().contains(tool_name)
    }

    pub fn shell_session_never_covers(&self, command: &str) -> bool {
        let bases = crate::security::SecurityPolicy::base_executables(command);
        if bases.is_empty() {
            return false;
        }
        let denied = self.session_shell_denylist.lock();
        bases.iter().any(|b| denied.contains(b))
    }

    /// Check whether a tool call requires interactive approval.
    ///
    /// Returns `true` if the call needs a prompt, `false` if it can proceed.
    pub fn needs_approval(&self, tool_name: &str) -> bool {
        // Full autonomy never prompts.
        if self.autonomy_level == AutonomyLevel::Full {
            return false;
        }

        // ReadOnly blocks everything — handled elsewhere; no prompt needed.
        if self.autonomy_level == AutonomyLevel::ReadOnly {
            return false;
        }

        // always_ask overrides everything.
        if self.always_ask.contains(tool_name) {
            return true;
        }

        // auto_approve skips the prompt.
        if self.auto_approve.contains(tool_name) {
            return false;
        }

        // Session allowlist (from prior "Always" responses).
        let allowlist = self.session_allowlist.lock();
        if allowlist.contains(tool_name) {
            return false;
        }

        // Default: supervised mode requires approval.
        true
    }

    /// Record an approval decision and update session state.
    pub fn record_decision(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        decision: ApprovalResponse,
        channel: &str,
    ) {
        if decision == ApprovalResponse::Never {
            let mut denylist = self.session_denylist.lock();
            denylist.insert(tool_name.to_string());
            drop(denylist);
            if let Some(store) = &self.overrides_store {
                if let Err(err) = store.persist_session_denylist_add(tool_name) {
                    tracing::warn!(
                        tool = tool_name,
                        error = %err,
                        "failed to persist session denylist to policy-overrides.yaml"
                    );
                }
            }
            if velaclaw_agent_runtime::is_shell_policy_tool(tool_name) {
                if let Some(command) = args.get("command").and_then(|v| v.as_str()) {
                    let bases = crate::security::SecurityPolicy::base_executables(command);
                    {
                        let mut bins = self.session_shell_denylist.lock();
                        for b in &bases {
                            bins.insert(b.clone());
                        }
                    }
                    if let Some(store) = &self.overrides_store {
                        for b in &bases {
                            if let Err(err) = store.persist_session_shell_denylist_add(b) {
                                tracing::warn!(
                                    binary = %b,
                                    error = %err,
                                    "failed to persist session shell denylist"
                                );
                            }
                        }
                    }
                }
            }
        }

        if decision == ApprovalResponse::Always {
            let persist_secrets = velaclaw_agent_runtime::shell_command_from_args(tool_name, args)
                .map_or(true, |c| {
                    !crate::security::policy::command_touches_secret_material(c)
                        && !crate::security::policy::command_invokes_posix_script(c)
                });
            if persist_secrets {
                let mut allowlist = self.session_allowlist.lock();
                allowlist.insert(tool_name.to_string());
                if let Some(store) = &self.overrides_store {
                    if let Err(err) = store.persist_session_allowlist_add(tool_name) {
                        tracing::warn!(
                            tool = tool_name,
                            error = %err,
                            "failed to persist session allowlist to policy-overrides.yaml"
                        );
                    }
                }
            }

            // Shell-policy Always: remember executable basenames only (VL-SEC-009 / H).
            if persist_secrets && velaclaw_agent_runtime::is_shell_policy_tool(tool_name) {
                if let Some(command) = args.get("command").and_then(|v| v.as_str()) {
                    let bases = crate::security::SecurityPolicy::base_executables(command);
                    if !bases.is_empty() {
                        {
                            let mut bins = self.session_shell_binaries.lock();
                            for b in &bases {
                                bins.insert(b.clone());
                            }
                        }
                        if let Some(store) = &self.overrides_store {
                            for b in &bases {
                                if let Err(err) = store.persist_session_shell_binary_add(b) {
                                    tracing::warn!(
                                        binary = %b,
                                        error = %err,
                                        "failed to persist session shell binary to policy-overrides.yaml"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }

        // Append to audit log.
        let summary = summarize_args(args);
        let entry = ApprovalLogEntry {
            timestamp: Utc::now().to_rfc3339(),
            tool_name: tool_name.to_string(),
            arguments_summary: summary.clone(),
            decision,
            channel: channel.to_string(),
        };
        let mut log = self.audit_log.lock();
        log.push(entry);

        if let Some(audit) = &self.security_audit {
            let approved = !matches!(decision, ApprovalResponse::No | ApprovalResponse::Never);
            if let Err(err) = audit.log_tool_approval(
                channel,
                tool_name,
                approval_decision_label(decision),
                &summary,
                approved,
            ) {
                tracing::warn!(
                    tool = tool_name,
                    error = %err,
                    "failed to write tool approval to security audit log"
                );
            }
        }
    }

    /// Get a snapshot of the audit log.
    pub fn audit_log(&self) -> Vec<ApprovalLogEntry> {
        self.audit_log.lock().clone()
    }

    /// Get the current session allowlist.
    pub fn session_allowlist(&self) -> HashSet<String> {
        self.session_allowlist.lock().clone()
    }

    /// Whether session Always covers risk prompts for every basename in `command`.
    pub fn shell_session_always_covers(&self, command: &str) -> bool {
        let bases = crate::security::SecurityPolicy::base_executables(command);
        if bases.is_empty() {
            return false;
        }
        let remembered = self.session_shell_binaries.lock();
        bases.iter().all(|b| remembered.contains(b))
    }

    /// Snapshot of session shell binary Always set.
    pub fn session_shell_binaries(&self) -> HashSet<String> {
        self.session_shell_binaries.lock().clone()
    }

    /// Prompt the operator on a TTY (product CLI). Non-TTY fails closed (no auto-Yes).
    pub fn prompt_cli(&self, request: &ApprovalRequest) -> ApprovalResponse {
        prompt_cli_interactive(request)
    }

    /// Wait for a gateway/Web UI approval via [`ApprovalHub`].
    pub async fn prompt_gateway(
        &self,
        hub: &ApprovalHub,
        request: &ApprovalRequest,
    ) -> ApprovalResponse {
        let summary = summarize_args(&request.arguments);
        hub.request(request, &summary).await
    }
}

// ── CLI prompt ───────────────────────────────────────────────────

/// Display the approval prompt and read user input from stdin.
fn prompt_cli_interactive(request: &ApprovalRequest) -> ApprovalResponse {
    use std::io::IsTerminal;

    if !std::io::stdin().is_terminal() {
        eprintln!(
            "🔒 Approval required for {} but stdin is not a TTY; denying. \
             Use an interactive `velaclaw` session or the Web ApprovalHub.",
            request.tool_name
        );
        return ApprovalResponse::No;
    }

    let summary = summarize_args(&request.arguments);
    eprintln!();
    if request.tool_name == "shell" {
        if request.elevation {
            eprintln!("🔒 Sandbox or policy blocked this command; elevate this invocation?");
        } else {
            eprintln!("🔒 Security policy requires approval for shell command:");
        }
        if let Some(cmd) = request.arguments.get("command").and_then(|v| v.as_str()) {
            eprintln!("   {cmd}");
            if crate::security::policy::command_requires_privilege_hint(cmd) {
                eprintln!(
                    "   Privilege note: sudo/su needs the binary in [autonomy].allowed_commands and your approval."
                );
            }
        } else {
            eprintln!("   {summary}");
        }
        eprintln!("   [Y]es = once, [A]lways = remember this executable, [N]o = deny this call, [!] Never = persist deny");
    } else {
        eprintln!(
            "🔒 Security policy requires approval for tool: {}",
            request.tool_name
        );
        eprintln!("   {summary}");
    }
    eprint!(
        "   [Y]es / [N]o / [A]lways / [!]Never for {}: ",
        request.tool_name
    );
    let _ = io::stderr().flush();

    let stdin = io::stdin();
    let mut line = String::new();
    if stdin.lock().read_line(&mut line).is_err() {
        return ApprovalResponse::No;
    }

    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => ApprovalResponse::Yes,
        "a" | "always" => ApprovalResponse::Always,
        "!" | "never" => ApprovalResponse::Never,
        _ => ApprovalResponse::No,
    }
}

fn approval_decision_label(decision: ApprovalResponse) -> &'static str {
    match decision {
        ApprovalResponse::Yes => "yes",
        ApprovalResponse::No => "no",
        ApprovalResponse::Always => "always",
        ApprovalResponse::Never => "never",
    }
}

/// Produce a short human-readable summary of tool arguments.
fn summarize_args(args: &serde_json::Value) -> String {
    match args {
        serde_json::Value::Object(map) => {
            let parts: Vec<String> = map
                .iter()
                .map(|(k, v)| {
                    let val = match v {
                        serde_json::Value::String(s) => truncate_for_summary(s, 80),
                        other => {
                            let s = other.to_string();
                            truncate_for_summary(&s, 80)
                        }
                    };
                    format!("{k}: {val}")
                })
                .collect();
            parts.join(", ")
        }
        other => {
            let s = other.to_string();
            truncate_for_summary(&s, 120)
        }
    }
}

fn truncate_for_summary(input: &str, max_chars: usize) -> String {
    let mut chars = input.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        input.to_string()
    }
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AutonomyConfig;

    fn supervised_config() -> AutonomyConfig {
        AutonomyConfig {
            level: AutonomyLevel::Supervised,
            auto_approve: vec!["file_read".into(), "memory_recall".into()],
            always_ask: vec!["shell".into()],
            ..AutonomyConfig::default()
        }
    }

    fn full_config() -> AutonomyConfig {
        AutonomyConfig {
            level: AutonomyLevel::Full,
            ..AutonomyConfig::default()
        }
    }

    // ── needs_approval ───────────────────────────────────────

    #[test]
    fn auto_approve_tools_skip_prompt() {
        let mgr = ApprovalManager::from_config(&supervised_config());
        assert!(!mgr.needs_approval("file_read"));
        assert!(!mgr.needs_approval("memory_recall"));
    }

    #[test]
    fn always_ask_tools_always_prompt() {
        let mgr = ApprovalManager::from_config(&supervised_config());
        assert!(mgr.needs_approval("shell"));
    }

    #[test]
    fn unknown_tool_needs_approval_in_supervised() {
        let mgr = ApprovalManager::from_config(&supervised_config());
        assert!(mgr.needs_approval("file_write"));
        assert!(mgr.needs_approval("http_request"));
    }

    #[test]
    fn full_autonomy_never_prompts() {
        let mgr = ApprovalManager::from_config(&full_config());
        assert!(!mgr.needs_approval("shell"));
        assert!(!mgr.needs_approval("file_write"));
        assert!(!mgr.needs_approval("anything"));
    }

    #[test]
    fn readonly_never_prompts() {
        let config = AutonomyConfig {
            level: AutonomyLevel::ReadOnly,
            ..AutonomyConfig::default()
        };
        let mgr = ApprovalManager::from_config(&config);
        assert!(!mgr.needs_approval("shell"));
    }

    // ── session allowlist ────────────────────────────────────

    #[test]
    fn always_response_adds_to_session_allowlist() {
        let mgr = ApprovalManager::from_config(&supervised_config());
        assert!(mgr.needs_approval("file_write"));

        mgr.record_decision(
            "file_write",
            &serde_json::json!({"path": "test.txt"}),
            ApprovalResponse::Always,
            "cli",
        );

        // Now file_write should be in session allowlist.
        assert!(!mgr.needs_approval("file_write"));
    }

    #[test]
    fn never_response_denies_tool_without_prompt_path() {
        let mgr = ApprovalManager::from_config(&supervised_config());
        mgr.record_decision(
            "file_write",
            &serde_json::json!({"path": "x"}),
            ApprovalResponse::Never,
            "cli",
        );
        assert!(mgr.is_never_tool("file_write"));
    }

    #[test]
    fn profile_change_clears_session_allowlist() {
        let mgr = ApprovalManager::from_config(&supervised_config());
        mgr.sync_security_profile(Some(crate::config::SecurityProfile::Isolated));
        mgr.record_decision(
            "file_write",
            &serde_json::json!({"path": "test.txt"}),
            ApprovalResponse::Always,
            "cli",
        );
        assert!(!mgr.needs_approval("file_write"));
        mgr.sync_security_profile(Some(crate::config::SecurityProfile::Local));
        assert!(mgr.needs_approval("file_write"));
    }

    #[test]
    fn always_ask_overrides_session_allowlist() {
        let mgr = ApprovalManager::from_config(&supervised_config());

        // Even after "Always" for shell, it should still prompt.
        mgr.record_decision(
            "shell",
            &serde_json::json!({"command": "ls"}),
            ApprovalResponse::Always,
            "cli",
        );

        // shell is in always_ask, so it still needs approval.
        assert!(mgr.needs_approval("shell"));
    }

    #[test]
    fn yes_response_does_not_add_to_allowlist() {
        let mgr = ApprovalManager::from_config(&supervised_config());
        mgr.record_decision(
            "file_write",
            &serde_json::json!({}),
            ApprovalResponse::Yes,
            "cli",
        );
        assert!(mgr.needs_approval("file_write"));
    }

    #[test]
    fn always_response_persists_to_policy_overrides() {
        use crate::config::PolicyOverridesStore;
        use std::fs;
        use tempfile::TempDir;
        use velaclaw_config::POLICY_OVERRIDES_DIR;

        let dir = TempDir::new().unwrap();
        let mut config = crate::config::Config::default();
        config.workspace_dir = dir.path().to_path_buf();
        let store = Arc::new(PolicyOverridesStore::new(&config, None));
        let mgr = ApprovalManager::from_config(&supervised_config()).with_overrides_store(store);

        mgr.record_decision(
            "file_write",
            &serde_json::json!({"path": "out.txt"}),
            ApprovalResponse::Always,
            "cli",
        );

        let path = dir
            .path()
            .join(POLICY_OVERRIDES_DIR)
            .join("policy-overrides.yaml");
        assert!(path.is_file());
        let raw = fs::read_to_string(path).unwrap();
        assert!(raw.contains("file_write"));
        assert!(!raw.contains("api_key"));
    }

    // ── audit log ────────────────────────────────────────────

    #[test]
    fn audit_log_records_decisions() {
        let mgr = ApprovalManager::from_config(&supervised_config());

        mgr.record_decision(
            "shell",
            &serde_json::json!({"command": "rm -rf ./build/"}),
            ApprovalResponse::No,
            "cli",
        );
        mgr.record_decision(
            "file_write",
            &serde_json::json!({"path": "out.txt", "content": "hello"}),
            ApprovalResponse::Yes,
            "cli",
        );

        let log = mgr.audit_log();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].tool_name, "shell");
        assert_eq!(log[0].decision, ApprovalResponse::No);
        assert_eq!(log[1].tool_name, "file_write");
        assert_eq!(log[1].decision, ApprovalResponse::Yes);
    }

    #[test]
    fn audit_log_contains_timestamp_and_channel() {
        let mgr = ApprovalManager::from_config(&supervised_config());
        mgr.record_decision(
            "shell",
            &serde_json::json!({"command": "ls"}),
            ApprovalResponse::Yes,
            "telegram",
        );

        let log = mgr.audit_log();
        assert_eq!(log.len(), 1);
        assert!(!log[0].timestamp.is_empty());
        assert_eq!(log[0].channel, "telegram");
    }

    // ── summarize_args ───────────────────────────────────────

    #[test]
    fn summarize_args_object() {
        let args = serde_json::json!({"command": "ls -la", "cwd": "/tmp"});
        let summary = summarize_args(&args);
        assert!(summary.contains("command: ls -la"));
        assert!(summary.contains("cwd: /tmp"));
    }

    #[test]
    fn summarize_args_truncates_long_values() {
        let long_val = "x".repeat(200);
        let args = serde_json::json!({"content": long_val});
        let summary = summarize_args(&args);
        assert!(summary.contains('…'));
        assert!(summary.len() < 200);
    }

    #[test]
    fn summarize_args_unicode_safe_truncation() {
        let long_val = "🦀".repeat(120);
        let args = serde_json::json!({"content": long_val});
        let summary = summarize_args(&args);
        assert!(summary.contains("content:"));
        assert!(summary.contains('…'));
    }

    #[test]
    fn summarize_args_non_object() {
        let args = serde_json::json!("just a string");
        let summary = summarize_args(&args);
        assert!(summary.contains("just a string"));
    }

    // ── ApprovalResponse serde ───────────────────────────────

    #[test]
    fn approval_response_serde_roundtrip() {
        let json = serde_json::to_string(&ApprovalResponse::Always).unwrap();
        assert_eq!(json, "\"always\"");
        let parsed: ApprovalResponse = serde_json::from_str("\"no\"").unwrap();
        assert_eq!(parsed, ApprovalResponse::No);
        let never: ApprovalResponse = serde_json::from_str("\"never\"").unwrap();
        assert_eq!(never, ApprovalResponse::Never);
    }

    // ── ApprovalRequest ──────────────────────────────────────

    #[test]
    fn approval_request_serde() {
        let req = ApprovalRequest {
            tool_name: "shell".into(),
            arguments: serde_json::json!({"command": "echo hi"}),
            elevation: false,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: ApprovalRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.tool_name, "shell");
        assert!(!parsed.elevation);
        let legacy: ApprovalRequest =
            serde_json::from_str(r#"{"tool_name":"shell","arguments":{}}"#).unwrap();
        assert!(!legacy.elevation);
    }

    #[test]
    fn cron_and_heartbeat_channels_are_not_interactive() {
        use super::backend::ManagerApprovalBackend;
        use velaclaw_agent_runtime::HumanApprovalBackend;

        let mgr = ApprovalManager::from_config(&supervised_config());
        assert!(ManagerApprovalBackend::new(&mgr, "cli").interactive_shell_approval());
        assert!(!ManagerApprovalBackend::new(&mgr, "cron").interactive_shell_approval());
        assert!(!ManagerApprovalBackend::new(&mgr, "heartbeat").interactive_shell_approval());
    }

    #[tokio::test]
    async fn approval_channel_supervised_denies_without_human() {
        use super::channel_hub::ChannelApprovalHub;
        use super::gate::ApprovalGate;
        use crate::agent::dispatcher::ParsedToolCall;
        use crate::channels::traits::{Channel, ChannelMessage, SendMessage};
        use crate::config::{AutonomyConfig, ChannelApprovalMode};
        use crate::security::{AutonomyLevel, PolicyHandle, SecurityPolicy};
        use async_trait::async_trait;
        use std::sync::Arc;
        use std::time::Duration;

        struct SilentChannel;

        #[async_trait]
        impl Channel for SilentChannel {
            fn name(&self) -> &str {
                "telegram"
            }

            async fn send(&self, _message: &SendMessage) -> anyhow::Result<()> {
                Ok(())
            }

            async fn listen(
                &self,
                _tx: tokio::sync::mpsc::Sender<ChannelMessage>,
            ) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let autonomy = AutonomyConfig {
            level: AutonomyLevel::Supervised,
            always_ask: vec!["shell".into()],
            ..AutonomyConfig::default()
        };
        let mgr = ApprovalManager::from_config(&autonomy);
        let security = PolicyHandle::new(SecurityPolicy::default());
        let session = super::backend::ChannelApprovalSession {
            hub: Arc::new(ChannelApprovalHub::new()),
            channel: Arc::new(SilentChannel),
            reply_target: "chat-1".into(),
            sender: "velaclaw_user".into(),
            mode: ChannelApprovalMode::Inline,
            timeout: Duration::from_millis(50),
        };
        let gate =
            ApprovalGate::new(&mgr, "telegram", Some(security)).with_channel_session(session);

        let call = ParsedToolCall {
            name: "shell".into(),
            arguments: serde_json::json!({"command": "curl https://example.com"}),
            tool_call_id: None,
        };

        match gate.decide_async(&call).await {
            GateDecision::Denied { message } => {
                assert!(
                    message.contains("Denied"),
                    "expected denial message, got: {message}"
                );
            }
            GateDecision::Proceed { .. } => {
                panic!("expected denial without human approval, got proceed")
            }
        }
    }

    #[tokio::test]
    async fn web_shell_policy_approval_uses_gateway_hub() {
        use super::gate::ApprovalGate;
        use super::hub::ApprovalHub;
        use crate::agent::dispatcher::ParsedToolCall;
        use crate::config::AutonomyConfig;
        use crate::security::{AutonomyLevel, PolicyHandle, SecurityPolicy};
        use std::sync::Arc;
        use std::time::Duration;

        // Allowlisted high-risk command under supervised → shell-policy interactive path.
        // Web + ApprovalHub must prompt (not sync-deny). Non-allowlisted never prompts (VL-SEC-009).
        let autonomy = AutonomyConfig {
            level: AutonomyLevel::Supervised,
            always_ask: vec![],
            auto_approve: vec!["shell".into()],
            ..AutonomyConfig::default()
        };
        let mgr = ApprovalManager::from_config(&autonomy);
        let mut policy = SecurityPolicy::default();
        policy.autonomy = AutonomyLevel::Supervised;
        policy.allowed_commands = vec!["curl".into()];
        let security = PolicyHandle::new(policy);
        let hub = Arc::new(ApprovalHub::new());
        let mut sub = hub.subscribe();
        let gate = ApprovalGate::new(&mgr, "web", Some(security)).with_hub(Arc::clone(&hub));

        let call = ParsedToolCall {
            name: "shell".into(),
            arguments: serde_json::json!({"command": "curl https://example.com"}),
            tool_call_id: None,
        };

        let hub_respond = Arc::clone(&hub);
        let respond = tokio::spawn(async move {
            let ev = tokio::time::timeout(Duration::from_secs(2), sub.recv())
                .await
                .expect("approval event timeout")
                .expect("approval broadcast");
            assert_eq!(ev.tool_name, "shell");
            assert!(hub_respond.respond(&ev.id, true, false, false));
        });

        let decision = gate.decide_async(&call).await;
        respond.await.expect("respond join");

        match decision {
            GateDecision::Proceed {
                shell_human_approved: true,
            } => {}
            GateDecision::Proceed {
                shell_human_approved: false,
            } => panic!("expected Proceed with human approval, got shell_human_approved=false"),
            GateDecision::Denied { message } => {
                panic!("expected Proceed with human approval, got Denied: {message}")
            }
        }
    }

    #[tokio::test]
    async fn shell_session_always_skips_risk_prompt_for_same_binary_only() {
        use super::gate::ApprovalGate;
        use super::hub::ApprovalHub;
        use crate::agent::dispatcher::ParsedToolCall;
        use crate::config::AutonomyConfig;
        use crate::security::{AutonomyLevel, PolicyHandle, SecurityPolicy};
        use std::sync::Arc;

        let autonomy = AutonomyConfig {
            level: AutonomyLevel::Full,
            always_ask: vec![],
            ..AutonomyConfig::default()
        };
        let mgr = ApprovalManager::from_config(&autonomy);
        mgr.record_decision(
            "shell",
            &serde_json::json!({"command": "echo ok"}),
            ApprovalResponse::Always,
            "web",
        );
        assert!(mgr.session_shell_binaries().contains("echo"));
        assert!(!mgr.shell_session_always_covers("apt remove -y samba"));

        let mut policy = SecurityPolicy::default();
        policy.autonomy = AutonomyLevel::Full;
        policy.allowed_commands = vec!["echo".into()];
        let security = PolicyHandle::new(policy);
        let hub = Arc::new(ApprovalHub::new());
        let gate = ApprovalGate::new(&mgr, "web", Some(security)).with_hub(Arc::clone(&hub));

        let foreign = ParsedToolCall {
            name: "shell".into(),
            arguments: serde_json::json!({"command": "apt remove -y samba"}),
            tool_call_id: None,
        };
        match gate.decide_async(&foreign).await {
            GateDecision::Denied { message } => {
                assert!(
                    message.contains("not in allowed_commands"),
                    "expected hard allowlist deny, got {message}"
                );
            }
            other @ GateDecision::Proceed { .. } => {
                panic!("expected Denied for non-allowlisted apt, got {other:?}")
            }
        }

        let mut policy2 = SecurityPolicy::default();
        policy2.autonomy = AutonomyLevel::Supervised;
        policy2.allowed_commands = vec!["echo".into()];
        policy2.require_approval_for_medium_risk = true;
        // echo alone is low risk — proceed without prompt even under supervised.
        let security2 = PolicyHandle::new(policy2);
        let gate2 = ApprovalGate::new(&mgr, "web", Some(security2)).with_hub(Arc::clone(&hub));
        let echo_again = ParsedToolCall {
            name: "shell".into(),
            arguments: serde_json::json!({"command": "echo again"}),
            tool_call_id: None,
        };
        match gate2.decide_async(&echo_again).await {
            GateDecision::Proceed { .. } => {}
            other @ GateDecision::Denied { .. } => {
                panic!("expected Proceed for allowlisted echo, got {other:?}")
            }
        }
    }

    #[tokio::test]
    async fn shell_session_always_covers_risk_for_remembered_binary() {
        use super::gate::ApprovalGate;
        use super::hub::ApprovalHub;
        use crate::agent::dispatcher::ParsedToolCall;
        use crate::config::AutonomyConfig;
        use crate::security::{AutonomyLevel, PolicyHandle, SecurityPolicy};
        use std::sync::Arc;

        let autonomy = AutonomyConfig {
            level: AutonomyLevel::Full,
            always_ask: vec![],
            ..AutonomyConfig::default()
        };
        let mgr = ApprovalManager::from_config(&autonomy);
        mgr.record_decision(
            "shell",
            &serde_json::json!({"command": "curl https://example.com"}),
            ApprovalResponse::Always,
            "web",
        );

        let mut policy = SecurityPolicy::default();
        policy.autonomy = AutonomyLevel::Supervised;
        policy.allowed_commands = vec!["curl".into()];
        policy.require_approval_for_medium_risk = true;
        let security = PolicyHandle::new(policy);
        let hub = Arc::new(ApprovalHub::new());
        let gate = ApprovalGate::new(&mgr, "web", Some(security)).with_hub(hub);

        let call = ParsedToolCall {
            name: "shell".into(),
            arguments: serde_json::json!({"command": "curl https://example.com/other"}),
            tool_call_id: None,
        };
        match gate.decide_async(&call).await {
            GateDecision::Proceed {
                shell_human_approved: true,
            } => {}
            GateDecision::Proceed {
                shell_human_approved: false,
            } => panic!("expected session-binary Always Proceed, got shell_human_approved=false"),
            GateDecision::Denied { message } => {
                panic!("expected session-binary Always Proceed, got Denied: {message}")
            }
        }
    }

    #[tokio::test]
    async fn supervised_shell_with_always_ask_needs_single_prompt() {
        use super::gate::ApprovalGate;
        use super::hub::ApprovalHub;
        use crate::agent::dispatcher::ParsedToolCall;
        use crate::config::AutonomyConfig;
        use crate::security::{AutonomyLevel, PolicyHandle, SecurityPolicy};
        use std::sync::Arc;
        use std::time::Duration;

        let autonomy = AutonomyConfig {
            level: AutonomyLevel::Supervised,
            always_ask: vec!["shell".into()],
            ..AutonomyConfig::default()
        };
        let mgr = ApprovalManager::from_config(&autonomy);
        let mut policy = SecurityPolicy::default();
        policy.autonomy = AutonomyLevel::Supervised;
        policy.allowed_commands = vec!["curl".into()];
        let security = PolicyHandle::new(policy);
        let hub = Arc::new(ApprovalHub::new());
        let mut sub = hub.subscribe();
        let gate = ApprovalGate::new(&mgr, "web", Some(security)).with_hub(Arc::clone(&hub));

        let call = ParsedToolCall {
            name: "shell".into(),
            arguments: serde_json::json!({"command": "curl https://example.com"}),
            tool_call_id: None,
        };

        let hub_respond = Arc::clone(&hub);
        let respond = tokio::spawn(async move {
            let ev = tokio::time::timeout(Duration::from_secs(2), sub.recv())
                .await
                .expect("approval event timeout")
                .expect("approval broadcast");
            assert_eq!(ev.tool_name, "shell");
            assert!(hub_respond.respond(&ev.id, true, false, false));
            assert!(
                tokio::time::timeout(Duration::from_millis(200), sub.recv())
                    .await
                    .is_err(),
                "expected only one approval prompt for shell (tool-level + shell-policy collapsed)"
            );
        });

        let decision = gate.decide_async(&call).await;
        respond.await.expect("respond join");

        match decision {
            GateDecision::Proceed {
                shell_human_approved: true,
            } => {}
            GateDecision::Proceed {
                shell_human_approved: false,
            } => panic!("expected Proceed with human approval, got shell_human_approved=false"),
            GateDecision::Denied { message } => {
                panic!("expected Proceed with human approval, got Denied: {message}")
            }
        }
    }

    #[tokio::test]
    async fn non_allowlisted_shell_denied_without_hub_prompt() {
        use super::gate::ApprovalGate;
        use super::hub::ApprovalHub;
        use crate::agent::dispatcher::ParsedToolCall;
        use crate::config::AutonomyConfig;
        use crate::security::{AutonomyLevel, PolicyHandle, SecurityPolicy};
        use std::sync::Arc;
        use std::time::Duration;

        let autonomy = AutonomyConfig {
            level: AutonomyLevel::Full,
            always_ask: vec![],
            ..AutonomyConfig::default()
        };
        let mgr = ApprovalManager::from_config(&autonomy);
        let mut policy = SecurityPolicy::default();
        policy.autonomy = AutonomyLevel::Full;
        policy.allowed_commands = vec!["echo".into()];
        let security = PolicyHandle::new(policy);
        let hub = Arc::new(ApprovalHub::new());
        let mut sub = hub.subscribe();
        let gate = ApprovalGate::new(&mgr, "web", Some(security)).with_hub(hub);

        let call = ParsedToolCall {
            name: "shell".into(),
            arguments: serde_json::json!({"command": "apt remove -y samba"}),
            tool_call_id: None,
        };

        let decision = gate.decide_async(&call).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(100), sub.recv())
                .await
                .is_err(),
            "non-allowlisted shell must not open ApprovalHub"
        );
        match decision {
            GateDecision::Denied { message } => {
                assert!(message.contains("not in allowed_commands"));
            }
            other @ GateDecision::Proceed { .. } => panic!("expected hard deny, got {other:?}"),
        }
    }
}
