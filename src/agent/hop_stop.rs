//! Host hop stop classes (VL-NA-043). One table for probe + DAG boundary.
//! 单跳停机分类：策略拒绝 / 封顶 / 取消，禁止再编事后图。

/// How this hop should close after a shell batch (not DAG hop count).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HopClose {
    #[default]
    None,
    /// Four executed shells; remaining DAG nodes may still run.
    Cap,
    /// Same policy class denied twice; store fail cursor, do not start later hops.
    PolicyDeny,
}

/// Stable fail_class values for [`crate::agent::bounded_dag_live::DagFailCursor`].
pub const FAIL_CLASS_CANCELLED: &str = "cancelled";
pub const FAIL_CLASS_POLICY_DENY: &str = "policy_deny";

/// Resume the stored remaining chain without a repair-planner chat.
#[must_use]
pub fn keep_remaining_without_replan(fail_class: &str) -> bool {
    matches!(
        fail_class.trim(),
        FAIL_CLASS_CANCELLED | FAIL_CLASS_POLICY_DENY
    )
}

/// Policy-deny subclass so two unlike denials do not trip the hop stop.
#[must_use]
pub fn policy_deny_class(output: &str) -> Option<&'static str> {
    let t = output.to_ascii_lowercase();
    if t.contains("[needs_approval]") || t.contains("approve once") {
        return None;
    }
    if t.contains("[once_denied]") {
        return Some("once_denied");
    }
    if t.contains("malformed invocation") {
        return Some("malformed");
    }
    if t.contains("unsafe shell construct") {
        return Some("unsafe_construct");
    }
    if t.contains("not in allowed_commands") || t.contains("not allowed by security policy") {
        return Some("allowlist");
    }
    if t.contains("wait-only") {
        return Some("wait");
    }
    if t.contains("[policy_deny]")
        || (t.contains("denied") && (t.contains("policy") || t.contains("security")))
    {
        return Some("other_policy");
    }
    None
}

/// Classes that close the hop on the first deny (not two unlike buckets).
#[must_use]
pub fn policy_deny_closes_on_first(class: &str) -> bool {
    matches!(class, "malformed" | "once_denied")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_and_policy_keep_remaining() {
        assert!(keep_remaining_without_replan(FAIL_CLASS_CANCELLED));
        assert!(keep_remaining_without_replan(FAIL_CLASS_POLICY_DENY));
        assert!(!keep_remaining_without_replan("unavailable"));
        assert!(!keep_remaining_without_replan(""));
    }

    #[test]
    fn once_prompt_is_not_policy_deny_class() {
        assert!(policy_deny_class("[needs_approval] approve Once").is_none());
    }

    #[test]
    fn malformed_and_once_denied_are_policy_classes() {
        assert_eq!(
            policy_deny_class("[policy_deny] malformed invocation: tool-call carrier in command."),
            Some("malformed")
        );
        assert_eq!(
            policy_deny_class("[once_denied] Denied by user after shell-policy approval."),
            Some("once_denied")
        );
        assert!(policy_deny_closes_on_first("malformed"));
        assert!(policy_deny_closes_on_first("once_denied"));
        assert!(!policy_deny_closes_on_first("allowlist"));
        assert_eq!(policy_deny_class("Denied by user."), None);
    }
}
