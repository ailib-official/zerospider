//! 工具执行收据（可审计 JSONL，不含密钥）。
//! Tool receipts: append-only JSONL under the workspace, with no secrets.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs::{create_dir_all, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

const MAX_COMMAND_CHARS: usize = 240;

/// Outcome recorded for a shell (or similar) tool attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptDecision {
    Allow,
    Deny,
    SandboxFail,
}

/// One JSONL record. Commands are truncated; never store env or stdin secrets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolReceipt {
    pub timestamp: DateTime<Utc>,
    pub tool: String,
    pub decision: ReceiptDecision,
    pub command: String,
    pub sandbox: String,
    pub human_approved: bool,
}

/// Append-only receipt log at `<workspace>/.velaclaw/tool_receipts.jsonl`.
#[derive(Debug, Clone)]
pub struct ToolReceiptLog {
    path: PathBuf,
}

impl ToolReceiptLog {
    pub fn in_workspace(workspace: &Path) -> Self {
        Self {
            path: workspace.join(".velaclaw").join("tool_receipts.jsonl"),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn record(
        &self,
        tool: &str,
        decision: ReceiptDecision,
        command: &str,
        sandbox: &str,
        human_approved: bool,
    ) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            create_dir_all(parent)?;
        }
        let receipt = ToolReceipt {
            timestamp: Utc::now(),
            tool: tool.to_string(),
            decision,
            command: truncate_command(&crate::security::redact_secret_literals(command)),
            sandbox: sandbox.to_string(),
            human_approved,
        };
        let line = serde_json::to_string(&receipt)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(file, "{line}")?;
        Ok(())
    }
}

fn truncate_command(command: &str) -> String {
    let mut out = String::new();
    for ch in command.chars() {
        if out.len() >= MAX_COMMAND_CHARS {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipts_write_allow_and_deny_without_secrets() {
        let tmp = tempfile::tempdir().unwrap();
        let log = ToolReceiptLog::in_workspace(tmp.path());
        log.record("shell", ReceiptDecision::Allow, "echo hello", "none", false)
            .unwrap();
        log.record("shell", ReceiptDecision::Deny, "rm -rf /", "none", true)
            .unwrap();
        let body = std::fs::read_to_string(log.path()).unwrap();
        assert!(body.contains("\"decision\":\"allow\""));
        assert!(body.contains("\"decision\":\"deny\""));
        assert!(body.contains("echo hello"));
        assert!(!body.to_lowercase().contains("api_key"));
        assert!(!body.contains("sk-"));
    }

    #[test]
    fn receipts_truncate_long_commands() {
        let long = "x".repeat(400);
        let t = truncate_command(&long);
        assert!(t.chars().count() <= MAX_COMMAND_CHARS + 1);
        assert!(t.ends_with('…'));
    }

    #[test]
    fn receipts_redact_github_pat_literals() {
        let tmp = tempfile::tempdir().unwrap();
        let log = ToolReceiptLog::in_workspace(tmp.path());
        let tok = format!("ghp_{}", "B".repeat(36));
        log.record(
            "shell",
            ReceiptDecision::Deny,
            &format!("echo {tok}"),
            "gate",
            false,
        )
        .unwrap();
        let body = std::fs::read_to_string(log.path()).unwrap();
        assert!(!body.contains(&tok), "{body}");
        assert!(body.contains("[REDACTED_TOKEN]"));
        assert!(body.contains("\"decision\":\"deny\""));
    }
}
