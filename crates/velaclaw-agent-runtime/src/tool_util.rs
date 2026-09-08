//! Credential scrubbing + tool arg alias normalization (VL-ARCH-010).
//! 凭证擦除与工具参数别名规范化（从主 crate tool_batch 迁入）。

use regex::{Regex, RegexSet};
use std::sync::LazyLock;

static SENSITIVE_KEY_PATTERNS: LazyLock<RegexSet> = LazyLock::new(|| {
    RegexSet::new([
        r"(?i)token",
        r"(?i)api[_-]?key",
        r"(?i)password",
        r"(?i)secret",
        r"(?i)user[_-]?key",
        r"(?i)bearer",
        r"(?i)credential",
    ])
    .unwrap()
});

static SENSITIVE_KV_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)(token|api[_-]?key|password|secret|user[_-]?key|bearer|credential)["']?\s*[:=]\s*(?:"([^"]{8,})"|'([^']{8,})'|([a-zA-Z0-9_\-\.]{8,}))"#,
    )
    .unwrap()
});

/// Scrub credentials from tool output to prevent accidental exfiltration.
pub fn scrub_credentials(input: &str) -> String {
    let _ = &*SENSITIVE_KEY_PATTERNS;
    let after_kv = SENSITIVE_KV_REGEX
        .replace_all(input, |caps: &regex::Captures| {
            let full_match = &caps[0];
            let key = &caps[1];
            let val = caps
                .get(2)
                .or(caps.get(3))
                .or(caps.get(4))
                .map(|m| m.as_str())
                .unwrap_or("");

            let prefix = if val.len() > 4 { &val[..4] } else { "" };

            if full_match.contains(':') {
                if full_match.contains('"') {
                    format!("\"{key}\": \"{prefix}*[REDACTED]\"")
                } else {
                    format!("{key}: {prefix}*[REDACTED]")
                }
            } else if full_match.contains('=') {
                if full_match.contains('"') {
                    format!("{key}=\"{prefix}*[REDACTED]\"")
                } else {
                    format!("{key}={prefix}*[REDACTED]")
                }
            } else {
                format!("{key}: {prefix}*[REDACTED]")
            }
        })
        .to_string();
    scrub_token_literals(&after_kv)
}

fn scrub_token_literals(input: &str) -> String {
    static PAT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}")
            .expect("token literal regex")
    });
    PAT.replace_all(input, "[REDACTED_TOKEN]").into_owned()
}

/// Map common DSML / model parameter aliases to tool schema keys.
pub fn normalize_tool_arguments(tool_name: &str, mut args: serde_json::Value) -> serde_json::Value {
    let Some(obj) = args.as_object_mut() else {
        return args;
    };
    match tool_name {
        "file_read" | "file_write" if !obj.contains_key("path") => {
            if let Some(path) = obj.remove("file_path") {
                obj.insert("path".to_string(), path);
            }
        }
        "shell" if !obj.contains_key("command") => {
            if let Some(cmd) = obj.remove("cmd") {
                obj.insert("command".to_string(), cmd);
            }
        }
        _ => {}
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrub_credentials_redacts_api_key() {
        let input = r#"api_key: "sk-abcdefghijklmnopqrstuvwxyz""#;
        let scrubbed = scrub_credentials(input);
        assert!(scrubbed.contains("*[REDACTED]"), "{scrubbed}");
        assert!(!scrubbed.contains("sk-abcdefghijklmnopqrstuvwxyz"));
    }

    #[test]
    fn scrub_credentials_redacts_ghp_literal() {
        let tok = format!("ghp_{}", "A".repeat(36));
        let out = scrub_credentials(&format!("printed {tok}"));
        assert!(!out.contains(&tok), "{out}");
        assert!(out.contains("[REDACTED_TOKEN]"));
    }

    #[test]
    fn normalize_file_read_alias() {
        let args = serde_json::json!({"file_path": "/tmp/x"});
        let out = normalize_tool_arguments("file_read", args);
        assert_eq!(out["path"], "/tmp/x");
        assert!(out.get("file_path").is_none());
    }
}
