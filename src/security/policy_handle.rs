//! Shared, hot-reloadable [`SecurityPolicy`] handle for tool registries (VL-SEC-005).
//! 工具注册表共用的可热刷新安全策略句柄。

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use super::policy::{
    AutonomyLevel, CommandRiskLevel, PolicyPromptExtras, SecurityPolicy, ToolOperation,
};
use crate::config::Config;

/// Cloneable handle to a workspace [`SecurityPolicy`] that may be refreshed in place.
#[derive(Clone)]
pub struct PolicyHandle(Arc<RwLock<SecurityPolicy>>);

impl PolicyHandle {
    pub fn new(policy: SecurityPolicy) -> Self {
        Self(Arc::new(RwLock::new(policy)))
    }

    pub fn from_config(autonomy: &crate::config::AutonomyConfig, workspace_dir: &Path) -> Self {
        Self::new(SecurityPolicy::from_config(autonomy, workspace_dir))
    }

    pub fn from_workspace_config(config: &Config) -> anyhow::Result<Self> {
        Ok(Self::new(SecurityPolicy::from_workspace_config(config)?))
    }

    /// Reload merged L1+L2+L2.5 autonomy into the in-memory policy (preserves action tracker).
    pub fn refresh_from_config(&self, config: &Config) -> anyhow::Result<()> {
        let mut fresh = SecurityPolicy::from_workspace_config(config)?;
        let mut guard = self.0.write().expect("security policy lock poisoned");
        fresh.tracker = guard.tracker.clone();
        *guard = fresh;
        Ok(())
    }

    pub fn read(&self) -> std::sync::RwLockReadGuard<'_, SecurityPolicy> {
        self.0.read().expect("security policy lock poisoned")
    }

    pub fn snapshot(&self) -> SecurityPolicy {
        self.read().clone()
    }

    pub fn can_act(&self) -> bool {
        self.read().can_act()
    }

    pub fn record_action(&self) -> bool {
        self.read().record_action()
    }

    pub fn is_rate_limited(&self) -> bool {
        self.read().is_rate_limited()
    }

    pub fn is_path_allowed(&self, path: &str) -> bool {
        self.read().is_path_allowed(path)
    }

    pub fn rewrite_temp_tool_path(&self, path: &str) -> String {
        self.read().rewrite_temp_tool_path(path)
    }

    pub fn set_graph_scratch_rel(&self, rel: Option<String>) {
        self.0
            .write()
            .expect("security policy lock poisoned")
            .graph_scratch_rel = rel;
    }

    pub fn tool_fs_path(&self, path: &str) -> PathBuf {
        self.read().tool_fs_path(path)
    }

    pub fn is_resolved_path_allowed(&self, resolved: &Path) -> bool {
        self.read().is_resolved_path_allowed(resolved)
    }

    pub fn allows_workspace_symlink_read(&self, logical_full: &Path, resolved: &Path) -> bool {
        self.read()
            .allows_workspace_symlink_read(logical_full, resolved)
    }

    pub fn validate_command_execution(
        &self,
        command: &str,
        human_approved: bool,
    ) -> Result<CommandRiskLevel, String> {
        self.read()
            .validate_command_execution(command, human_approved)
    }

    pub fn enforce_tool_operation(
        &self,
        operation: ToolOperation,
        tool_name: &str,
    ) -> Result<(), String> {
        self.read().enforce_tool_operation(operation, tool_name)
    }

    pub fn validate_secret_path_access(&self, path: &str, approved: bool) -> Result<(), String> {
        self.read().validate_secret_path_access(path, approved)
    }

    pub fn workspace_dir(&self) -> PathBuf {
        self.read().workspace_dir.clone()
    }

    pub fn autonomy(&self) -> AutonomyLevel {
        self.read().autonomy
    }

    pub fn command_risk_level(&self, command: &str) -> CommandRiskLevel {
        self.read().command_risk_level(command)
    }

    pub fn is_command_allowed(&self, command: &str) -> bool {
        self.read().is_command_allowed(command)
    }

    pub fn append_execution_policy_prompt(&self, prompt: &mut String, extras: &PolicyPromptExtras) {
        self.read().append_execution_policy_prompt(prompt, extras);
    }

    /// Whether human-approved shell may skip OS sandbox wrap (`[security.sandbox].escape_on_approval`).
    pub fn escape_on_approval(&self) -> bool {
        self.read().escape_on_approval
    }

    pub fn inherit_process_env(&self) -> bool {
        self.read().inherit_process_env
    }

    pub fn profile(&self) -> Option<crate::config::SecurityProfile> {
        self.read().profile
    }
}
