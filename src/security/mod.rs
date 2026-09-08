//! 安全子系统，负责策略管理、密钥存储与配对逻辑。
//! Security subsystem for policy enforcement, sandboxing, and secret management.
//!
//! This module provides the security infrastructure for VelaClaw. The core type
//! [`SecurityPolicy`] defines autonomy levels, workspace boundaries, and
//! access-control rules that are enforced across the tool and runtime subsystems.
//! [`PairingGuard`] implements device pairing for channel authentication, and
//! [`SecretStore`] handles encrypted credential storage.
//!
//! OS-level isolation is provided through the [`Sandbox`] trait defined in
//! [`traits`], with pluggable backends including Docker, Firejail, Bubblewrap,
//! and Landlock. [`create_sandbox`] selects Linux Auto as Landlock or
//! fail-closed. [`ToolReceiptLog`] records allow/deny/sandbox_fail lines.
//!
//! # Extension
//!
//! To add a new sandbox backend, implement [`Sandbox`] in a new submodule and
//! register it in [`detect::create_sandbox`]. See `AGENTS.md` §7.5 for security
//! change guidelines.

pub mod audit;
#[cfg(feature = "sandbox-bubblewrap")]
pub mod bubblewrap;
pub mod detect;
pub mod docker;
#[cfg(target_os = "linux")]
pub mod firejail;
#[cfg(feature = "sandbox-landlock")]
pub mod landlock;
pub mod pairing;
pub mod policy;
pub mod policy_handle;
pub mod receipts;
pub mod secrets;
pub mod traits;

#[allow(unused_imports)]
pub use audit::{AuditEvent, AuditEventType, AuditLogger};
#[allow(unused_imports)]
pub use detect::{
    create_sandbox, describe_effective_sandbox, effective_sandbox_config, EffectiveSandbox,
};
#[allow(unused_imports)]
pub use pairing::PairingGuard;
pub use policy::{
    normalize_autonomy_config, AutonomyLevel, PolicyPromptExtras, SecretPathMode, SecurityPolicy,
    ToolOperation,
};
pub use policy_handle::PolicyHandle;
#[allow(unused_imports)]
pub use receipts::{ReceiptDecision, ToolReceipt, ToolReceiptLog};
#[allow(unused_imports)]
pub use secrets::SecretStore;
#[allow(unused_imports)]
pub use traits::{FailClosedSandbox, NoopSandbox, Sandbox};

/// Mask GitHub PAT / common token literals so they never land in receipts or policy errors.
pub fn redact_secret_literals(input: &str) -> String {
    static PAT: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = PAT.get_or_init(|| {
        regex::Regex::new(
            r"(?x)
            (?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{20,}
            | github_pat_[A-Za-z0-9_]{20,}
            ",
        )
        .expect("token literal regex")
    });
    re.replace_all(input, "[REDACTED_TOKEN]").into_owned()
}

/// Redact sensitive values for safe logging. Shows first 4 chars + "***" suffix.
/// This function intentionally breaks the data-flow taint chain for static analysis.
pub fn redact(value: &str) -> String {
    if value.len() <= 4 {
        "***".to_string()
    } else {
        format!("{}***", &value[..4])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reexported_policy_and_pairing_types_are_usable() {
        let policy = SecurityPolicy::default();
        assert_eq!(policy.autonomy, AutonomyLevel::Supervised);

        let guard = PairingGuard::new(false, &[]);
        assert!(!guard.require_pairing());
    }

    #[test]
    fn reexported_secret_store_encrypt_decrypt_roundtrip() {
        let temp = tempfile::tempdir().unwrap();
        let store = SecretStore::new(temp.path(), false);

        let encrypted = store.encrypt("top-secret").unwrap();
        let decrypted = store.decrypt(&encrypted).unwrap();

        assert_eq!(decrypted, "top-secret");
    }

    #[test]
    fn redact_hides_most_of_value() {
        assert_eq!(redact("abcdefgh"), "abcd***");
        assert_eq!(redact("ab"), "***");
        assert_eq!(redact(""), "***");
        assert_eq!(redact("12345"), "1234***");
    }

    #[test]
    fn redact_secret_literals_masks_github_pats() {
        let raw = "token ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAABB and github_pat_11AAAAAAA0BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";
        let out = redact_secret_literals(raw);
        assert!(!out.contains("ghp_A"));
        assert!(!out.contains("github_pat_11"));
        assert!(out.contains("[REDACTED_TOKEN]"));
    }
}
