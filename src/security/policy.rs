use parking_lot::Mutex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// How much autonomy the agent has
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AutonomyLevel {
    /// Read-only: can observe but not act
    ReadOnly,
    /// Supervised: acts but requires approval for risky operations
    #[default]
    Supervised,
    /// Full: autonomous execution within policy bounds
    Full,
}

/// How shell/file commands that name credential basenames are treated (VL-SEC-011).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SecretPathMode {
    /// Hard `[policy_deny]` even when approved (legacy default when profile is unset).
    #[default]
    Deny,
    /// `[needs_approval]` until Once/Always this invocation; isolated profile.
    Ask,
    /// Allow when otherwise policy-ok (local profile). Output still scrubbed.
    Allow,
}

/// Risk score for shell command execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandRiskLevel {
    Low,
    Medium,
    High,
}

/// Extra shell commands merged into `[autonomy].allowed_commands` when `level = full`.
/// Operators can still override by omitting names from their explicit allowlist only if they
/// replace the entire default list; merged entries are additive for home-lab usability.
pub const FULL_AUTONOMY_EXTRA_COMMANDS: &[&str] = &[
    "apt", "apt-get", "awk", "bash", "chmod", "cp", "curl", "dig", "docker", "dpkg", "host",
    "make", "mkdir", "mv", "nc", "node", "ping", "python", "python3", "rm", "rsync", "scp", "sed",
    "sh", "sort", "ssh", "tar", "touch", "tr", "uniq", "unzip", "wget", "zip",
];

/// Path prefixes dropped from `forbidden_paths` when `level = full` (too broad for owned machines).
const FULL_AUTONOMY_RELAXED_FORBIDDEN_PREFIXES: &[&str] = &["/home", "/tmp"];

/// Apply level-aware defaults so `full` autonomy is usable on home-lab installs without
/// hand-editing dozens of config keys.
pub fn normalize_autonomy_config(
    autonomy_config: &crate::config::AutonomyConfig,
) -> crate::config::AutonomyConfig {
    if autonomy_config.level != AutonomyLevel::Full {
        return autonomy_config.clone();
    }

    let mut effective = autonomy_config.clone();
    for cmd in FULL_AUTONOMY_EXTRA_COMMANDS {
        let name = (*cmd).to_string();
        if !effective.allowed_commands.iter().any(|c| c == &name) {
            effective.allowed_commands.push(name);
        }
    }
    effective
        .forbidden_paths
        .retain(|p| !FULL_AUTONOMY_RELAXED_FORBIDDEN_PREFIXES.contains(&p.as_str()));
    effective
}

/// Optional runtime surfaces shown in the execution-policy system prompt.
#[derive(Debug, Clone, Default)]
pub struct PolicyPromptExtras {
    pub http_request_enabled: bool,
    pub proxy_enabled: bool,
    pub proxy_http: Option<String>,
    /// L2 `self_adjust.allowed_writes` patterns (empty = default session-allowlist only).
    pub self_adjust_allowed_writes: Vec<String>,
    /// L2 `self_adjust.denied_writes` patterns.
    pub self_adjust_denied_writes: Vec<String>,
    /// Whether the `policy_patch` tool is registered for this session.
    pub policy_patch_enabled: bool,
    /// Configured `[runtime].kind` (native / docker / wasm).
    pub runtime_kind: String,
    /// Effective OS sandbox name (landlock / none / fail-closed / …).
    pub sandbox_name: String,
}

/// Classifies whether a tool operation is read-only or side-effecting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolOperation {
    Read,
    Act,
}

/// Sliding-window action tracker for rate limiting.
#[derive(Debug)]
pub struct ActionTracker {
    /// Timestamps of recent actions (kept within the last hour).
    actions: Mutex<Vec<Instant>>,
}

impl ActionTracker {
    pub fn new() -> Self {
        Self {
            actions: Mutex::new(Vec::new()),
        }
    }

    /// Record an action and return the current count within the window.
    pub fn record(&self) -> usize {
        let mut actions = self.actions.lock();
        let cutoff = Instant::now()
            .checked_sub(std::time::Duration::from_secs(3600))
            .unwrap_or_else(Instant::now);
        actions.retain(|t| *t > cutoff);
        actions.push(Instant::now());
        actions.len()
    }

    /// Count of actions in the current window without recording.
    pub fn count(&self) -> usize {
        let mut actions = self.actions.lock();
        let cutoff = Instant::now()
            .checked_sub(std::time::Duration::from_secs(3600))
            .unwrap_or_else(Instant::now);
        actions.retain(|t| *t > cutoff);
        actions.len()
    }
}

impl Clone for ActionTracker {
    fn clone(&self) -> Self {
        let actions = self.actions.lock();
        Self {
            actions: Mutex::new(actions.clone()),
        }
    }
}

/// Security policy enforced on all tool executions
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone)]
pub struct SecurityPolicy {
    pub autonomy: AutonomyLevel,
    pub workspace_dir: PathBuf,
    pub workspace_only: bool,
    pub allowed_commands: Vec<String>,
    pub forbidden_paths: Vec<String>,
    pub max_actions_per_hour: u32,
    pub max_cost_per_day_cents: u32,
    pub require_approval_for_medium_risk: bool,
    pub block_high_risk_commands: bool,
    /// When true, human-approved shell skips OS sandbox wrap (see `[security.sandbox]`).
    pub escape_on_approval: bool,
    /// Inherit daemon env in `apply_shell_child_env` (local profile).
    pub inherit_process_env: bool,
    /// Credential-path handling.
    pub secret_path_mode: SecretPathMode,
    /// Unfolded `[security.profile]` if set.
    pub profile: Option<crate::config::SecurityProfile>,
    pub tracker: ActionTracker,
    /// When set, `/tmp` rewrites into this workspace-relative graph scratch (VL-NA-040).
    pub graph_scratch_rel: Option<String>,
}

impl Default for SecurityPolicy {
    fn default() -> Self {
        Self {
            autonomy: AutonomyLevel::Supervised,
            workspace_dir: PathBuf::from("."),
            workspace_only: true,
            allowed_commands: vec![
                "git".into(),
                "npm".into(),
                "cargo".into(),
                "ls".into(),
                "cat".into(),
                "grep".into(),
                "find".into(),
                "echo".into(),
                "pwd".into(),
                "wc".into(),
                "head".into(),
                "tail".into(),
                "date".into(),
            ],
            forbidden_paths: vec![
                // System directories (blocked even when workspace_only=false)
                "/etc".into(),
                "/root".into(),
                "/home".into(),
                "/usr".into(),
                "/bin".into(),
                "/sbin".into(),
                "/lib".into(),
                "/opt".into(),
                "/boot".into(),
                "/dev".into(),
                "/proc".into(),
                "/sys".into(),
                "/var".into(),
                "/tmp".into(),
                // Sensitive dotfiles
                "~/.ssh".into(),
                "~/.gnupg".into(),
                "~/.aws".into(),
                "~/.config".into(),
            ],
            max_actions_per_hour: 20,
            max_cost_per_day_cents: 500,
            require_approval_for_medium_risk: true,
            block_high_risk_commands: true,
            escape_on_approval: false,
            inherit_process_env: false,
            secret_path_mode: SecretPathMode::Deny,
            profile: None,
            tracker: ActionTracker::new(),
            graph_scratch_rel: None,
        }
    }
}

/// Workspace-relative scratch for host temp roots (`/tmp`, `/var/tmp`).
pub const SCRATCH_REL: &str = ".velaclaw/tmp";

/// Map `/tmp/foo` → `.velaclaw/tmp/foo` so file tools stay workspace-only.
#[must_use]
pub fn rewrite_temp_tool_path(path: &str) -> String {
    let trimmed = path.trim();
    for prefix in ["/var/tmp/", "/tmp/", "/var/tmp", "/tmp"] {
        if trimmed == prefix.trim_end_matches('/') {
            return format!("{SCRATCH_REL}/scratch");
        }
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            if prefix.ends_with('/') || rest.starts_with('/') {
                let name = rest.trim_start_matches('/');
                if name.is_empty() {
                    return format!("{SCRATCH_REL}/scratch");
                }
                return format!("{SCRATCH_REL}/{name}");
            }
        }
    }
    trimmed.to_string()
}

// ── Shell Command Parsing Utilities ───────────────────────────────────────
// These helpers implement a minimal quote-aware shell lexer. They exist
// because security validation must reason about the *structure* of a
// command (separators, operators, quoting) rather than treating it as a
// flat string — otherwise an attacker could hide dangerous sub-commands
// inside quoted arguments or chained operators.
/// Skip leading environment variable assignments (e.g. `FOO=bar cmd args`).
/// Returns the remainder starting at the first non-assignment word.
fn skip_env_assignments(s: &str) -> &str {
    let mut rest = s;
    loop {
        let Some(word) = rest.split_whitespace().next() else {
            return rest;
        };
        // Environment assignment: contains '=' and starts with a letter or underscore
        if word.contains('=')
            && word
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        {
            // Advance past this word
            rest = rest[word.len()..].trim_start();
        } else {
            return rest;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuoteState {
    None,
    Single,
    Double,
}

/// Wait-only binaries stall the turn; never allow, even under Full / approval.
fn command_is_wait_only_executable(command: &str) -> bool {
    SecurityPolicy::base_executables(command).iter().any(|b| {
        let b = b.to_ascii_lowercase();
        b == "sleep" || b == "usleep"
    })
}

/// Split a shell command into sub-commands by unquoted separators.
///
/// Separators:
/// - `;` and newline
/// - `|`
/// - `&&`, `||`
///
/// Characters inside single or double quotes are treated as literals, so
/// `sqlite3 db "SELECT 1; SELECT 2;"` remains a single segment.
fn split_unquoted_segments(command: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut quote = QuoteState::None;
    let mut escaped = false;
    let mut chars = command.chars().peekable();

    let push_segment = |segments: &mut Vec<String>, current: &mut String| {
        let trimmed = current.trim();
        if !trimmed.is_empty() {
            segments.push(trimmed.to_string());
        }
        current.clear();
    };

    while let Some(ch) = chars.next() {
        match quote {
            QuoteState::Single => {
                if ch == '\'' {
                    quote = QuoteState::None;
                }
                current.push(ch);
            }
            QuoteState::Double => {
                if escaped {
                    escaped = false;
                    current.push(ch);
                    continue;
                }
                if ch == '\\' {
                    escaped = true;
                    current.push(ch);
                    continue;
                }
                if ch == '"' {
                    quote = QuoteState::None;
                }
                current.push(ch);
            }
            QuoteState::None => {
                if escaped {
                    escaped = false;
                    current.push(ch);
                    continue;
                }
                if ch == '\\' {
                    escaped = true;
                    current.push(ch);
                    continue;
                }

                match ch {
                    '\'' => {
                        quote = QuoteState::Single;
                        current.push(ch);
                    }
                    '"' => {
                        quote = QuoteState::Double;
                        current.push(ch);
                    }
                    ';' | '\n' => push_segment(&mut segments, &mut current),
                    '|' => {
                        if chars.next_if_eq(&'|').is_some() {
                            // Consume full `||`; both characters are separators.
                        }
                        push_segment(&mut segments, &mut current);
                    }
                    '&' => {
                        if chars.next_if_eq(&'&').is_some() {
                            // `&&` is a separator; single `&` is handled separately.
                            push_segment(&mut segments, &mut current);
                        } else {
                            current.push(ch);
                        }
                    }
                    _ => current.push(ch),
                }
            }
        }
    }

    let trimmed = current.trim();
    if !trimmed.is_empty() {
        segments.push(trimmed.to_string());
    }

    segments
}

/// Detect a single unquoted `&` operator (background/chain). `&&` is allowed.
///
/// We treat any standalone `&` as unsafe in policy validation because it can
/// chain hidden sub-commands and escape foreground timeout expectations.
fn contains_unquoted_single_ampersand(command: &str) -> bool {
    let mut quote = QuoteState::None;
    let mut escaped = false;
    let mut chars = command.chars().peekable();
    let mut prev_significant = None;

    while let Some(ch) = chars.next() {
        match quote {
            QuoteState::Single => {
                if ch == '\'' {
                    quote = QuoteState::None;
                }
            }
            QuoteState::Double => {
                if escaped {
                    escaped = false;
                    continue;
                }
                if ch == '\\' {
                    escaped = true;
                    continue;
                }
                if ch == '"' {
                    quote = QuoteState::None;
                }
            }
            QuoteState::None => {
                if escaped {
                    escaped = false;
                    continue;
                }
                if ch == '\\' {
                    escaped = true;
                    continue;
                }
                match ch {
                    '\'' => quote = QuoteState::Single,
                    '"' => quote = QuoteState::Double,
                    '&' => {
                        if chars.next_if_eq(&'&').is_some() {
                            prev_significant = Some('&');
                            continue;
                        }
                        if prev_significant == Some('>') {
                            if let Some(next) = chars.peek() {
                                if next.is_ascii_digit() || *next == '-' {
                                    chars.next();
                                }
                            }
                            prev_significant = Some('&');
                            continue;
                        }
                        return true;
                    }
                    ch if !ch.is_whitespace() => prev_significant = Some(ch),
                    _ => {}
                }
            }
        }
    }

    false
}

/// Detect an unquoted character in a shell command.
fn contains_unquoted_char(command: &str, target: char) -> bool {
    let mut quote = QuoteState::None;
    let mut escaped = false;

    for ch in command.chars() {
        match quote {
            QuoteState::Single => {
                if ch == '\'' {
                    quote = QuoteState::None;
                }
            }
            QuoteState::Double => {
                if escaped {
                    escaped = false;
                    continue;
                }
                if ch == '\\' {
                    escaped = true;
                    continue;
                }
                if ch == '"' {
                    quote = QuoteState::None;
                }
            }
            QuoteState::None => {
                if escaped {
                    escaped = false;
                    continue;
                }
                if ch == '\\' {
                    escaped = true;
                    continue;
                }
                match ch {
                    '\'' => quote = QuoteState::Single,
                    '"' => quote = QuoteState::Double,
                    _ if ch == target => return true,
                    _ => {}
                }
            }
        }
    }

    false
}

/// Detect output redirects that can write to arbitrary paths.
/// Permits `2>/dev/null`, `1>/dev/null`, and `2>&1` style fd duplication.
fn contains_unquoted_unsafe_redirect(command: &str) -> bool {
    let mut quote = QuoteState::None;
    let mut escaped = false;
    let bytes = command.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        let ch = command[i..].chars().next().unwrap();
        let ch_len = ch.len_utf8();

        match quote {
            QuoteState::Single => {
                if ch == '\'' {
                    quote = QuoteState::None;
                }
            }
            QuoteState::Double => {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    quote = QuoteState::None;
                }
            }
            QuoteState::None => {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '\'' {
                    quote = QuoteState::Single;
                } else if ch == '"' {
                    quote = QuoteState::Double;
                } else if ch == '>' {
                    if i + 1 < bytes.len() && bytes[i + 1] == b'>' {
                        return true;
                    }

                    let mut k = i + 1;
                    while k < bytes.len() && bytes[k].is_ascii_whitespace() {
                        k += 1;
                    }

                    if k < bytes.len() && bytes[k] == b'&' {
                        k += 1;
                        if k < bytes.len() && (bytes[k].is_ascii_digit() || bytes[k] == b'-') {
                            i = k + 1;
                            continue;
                        }
                        return true;
                    }

                    let tail = &command[k..];
                    if tail.starts_with("/dev/null") {
                        i = k + "/dev/null".len();
                        continue;
                    }

                    return true;
                }
            }
        }

        i += ch_len;
    }

    false
}

pub(crate) fn command_requires_privilege_hint(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    lower.contains("sudo")
        || lower.contains(" su ")
        || lower.starts_with("su ")
        || lower.contains("runuser")
        || lower.contains("/root/")
        || lower.contains(" pkexec")
}

const MAX_SECRET_SCRIPT_BYTES: u64 = 64 * 1024;
const MAX_ADMITTED_SHELL_BYTES: usize = 8192;
const MAX_ADMITTED_PATH_BYTES: usize = 4096;

/// Gate + execute share this deny token (VL-NA-044). Never Prompt Once.
pub const MALFORMED_INVOCATION_MARK: &str = "[policy_deny] malformed invocation";

/// Reject tool args that are still a model envelope, not a command or path.
pub fn admit_tool_invocation(tool_name: &str, args: &Value) -> Result<(), String> {
    match tool_name {
        "file_read" | "file_write" => {
            let path = args.get("path").and_then(Value::as_str).unwrap_or("");
            admit_file_path(path)
        }
        "shell" | "cron_add" | "cron_update" | "cron_run" | "schedule" => {
            let command = args.get("command").and_then(Value::as_str).unwrap_or("");
            admit_shell_command(command)
        }
        _ => Ok(()),
    }
}

/// Admit a shell `command` argument (GOV-007 with [`admit_tool_invocation`]).
pub fn admit_shell_command(command: &str) -> Result<(), String> {
    if command.trim().is_empty() {
        return Err(malformed_invocation_error("empty command"));
    }
    if command.contains('\0') {
        return Err(malformed_invocation_error("NUL in command"));
    }
    if crate::util::invocation_contains_carrier(command) {
        return Err(malformed_invocation_error("tool-call carrier in command"));
    }
    if command.len() > MAX_ADMITTED_SHELL_BYTES {
        return Err(malformed_invocation_error("command exceeds size cap"));
    }
    Ok(())
}

/// Admit a file-tool `path` only. `file_write` content is never a path or Once display.
pub fn admit_file_path(path: &str) -> Result<(), String> {
    if path.trim().is_empty() {
        return Err(malformed_invocation_error("empty path"));
    }
    if path.contains('\0') || path.contains('\n') || path.contains('\r') {
        return Err(malformed_invocation_error("path is not a single line"));
    }
    if crate::util::invocation_contains_carrier(path) {
        return Err(malformed_invocation_error("tool-call carrier in path"));
    }
    if path.len() > MAX_ADMITTED_PATH_BYTES {
        return Err(malformed_invocation_error("path exceeds size cap"));
    }
    Ok(())
}

fn malformed_invocation_error(detail: &str) -> String {
    format!("{MALFORMED_INVOCATION_MARK}: {detail}.")
}

/// Credential / PAT / key files the agent must never dump into tool results.
/// Matches path **basenames** (and a few well-known relative paths), not raw substrings
/// (`raid_rsa` must not trip `id_rsa`).
pub(crate) fn command_touches_secret_material(command: &str) -> bool {
    command_touches_secret_material_in(command, None)
}

pub(crate) fn command_touches_secret_material_in(command: &str, workspace: Option<&Path>) -> bool {
    if text_touches_secret_material(command) {
        return true;
    }
    let Some(workspace) = workspace else {
        return false;
    };
    workspace_scripts_touch_secrets(command, workspace)
}

pub(crate) fn path_touches_secret_material(path: &str) -> bool {
    text_touches_secret_material(path)
}

/// `bash|sh` wrappers whose body is not on argv — do not persist Always.
pub(crate) fn command_invokes_posix_script(command: &str) -> bool {
    !invoked_posix_script_paths(command).is_empty()
}

fn text_touches_secret_material(text: &str) -> bool {
    for raw in text.split_whitespace() {
        let token = raw.trim_matches(|c| c == '\'' || c == '"' || c == '`');
        if token.is_empty() {
            continue;
        }
        let lower = token.to_ascii_lowercase();
        if lower.ends_with("/.aws/credentials") || lower == ".aws/credentials" {
            return true;
        }
        if lower.contains("/.gnupg/")
            || lower.ends_with("/.gnupg")
            || lower == ".gnupg"
            || lower.starts_with(".gnupg/")
        {
            return true;
        }
        let base = lower.rsplit('/').next().unwrap_or(lower.as_str());
        if secret_basename(base) {
            return true;
        }
    }
    false
}

fn posix_shell_interpreter(base: &str) -> bool {
    matches!(
        base,
        "bash" | "sh" | "dash" | "bash.exe" | "sh.exe" | "dash.exe"
    )
}

fn invoked_posix_script_paths(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    for segment in split_unquoted_segments(command) {
        let cmd_part = skip_env_assignments(&segment);
        let mut words = cmd_part.split_whitespace();
        let Some(base_raw) = words.next() else {
            continue;
        };
        let token0 = base_raw.trim_matches(|c| c == '\'' || c == '"' || c == '`');
        let base = token0
            .rsplit('/')
            .next()
            .unwrap_or(token0)
            .to_ascii_lowercase();
        let source_dot = token0 == "." || base == "source";
        if !posix_shell_interpreter(&base) && !source_dot {
            continue;
        }
        for w in words {
            let t = w.trim_matches(|c| c == '\'' || c == '"' || c == '`');
            if t.is_empty() {
                continue;
            }
            if t.starts_with('-') {
                if posix_shell_interpreter(&base) && t.contains('c') {
                    break;
                }
                continue;
            }
            out.push(t.to_string());
            break;
        }
    }
    out
}

fn workspace_scripts_touch_secrets(command: &str, workspace: &Path) -> bool {
    let Ok(canon_ws) = workspace.canonicalize() else {
        return false;
    };
    for rel in invoked_posix_script_paths(command) {
        let candidate = Path::new(&rel);
        let joined = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            workspace.join(candidate)
        };
        let resolved = std::fs::canonicalize(&joined).unwrap_or(joined);
        if !resolved.starts_with(&canon_ws) {
            continue;
        }
        let Ok(meta) = std::fs::metadata(&resolved) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        if meta.len() > MAX_SECRET_SCRIPT_BYTES {
            return true;
        }
        let Ok(body) = std::fs::read_to_string(&resolved) else {
            continue;
        };
        if text_touches_secret_material(&body) {
            return true;
        }
    }
    false
}

fn secret_basename(base: &str) -> bool {
    matches!(
        base,
        "github_token_list.txt"
            | "github-tokens.md"
            | ".netrc"
            | "_netrc"
            | "id_rsa"
            | "id_rsa.pub"
            | "id_ecdsa"
            | "id_ecdsa.pub"
            | "id_ed25519"
            | "id_ed25519.pub"
            | "id_ed25519_sk"
            | "id_ed25519_sk.pub"
            | "id_ecdsa_sk"
            | "id_ecdsa_sk.pub"
    ) || base.starts_with("id_rsa.")
        || base.starts_with("id_ed25519.")
        || base.starts_with("id_ed25519_")
        || base.starts_with("id_ecdsa.")
        || base.starts_with("id_ecdsa_")
}

/// Bases that need human approval (even under Full) when `escape_on_approval` is on,
/// so the approved path can skip Landlock/NNP for real package/privilege work.
fn command_requires_sandbox_escape_approval(command: &str) -> bool {
    if command_requires_privilege_hint(command) {
        return true;
    }
    for segment in split_unquoted_segments(command) {
        let cmd_part = skip_env_assignments(&segment);
        let Some(base_raw) = cmd_part.split_whitespace().next() else {
            continue;
        };
        let base = base_raw
            .rsplit('/')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        if matches!(
            base.as_str(),
            "sudo" | "su" | "pkexec" | "runuser" | "apt" | "apt-get" | "dpkg"
        ) {
            return true;
        }
    }
    false
}

impl SecurityPolicy {
    // ── Risk Classification ──────────────────────────────────────────────
    // Risk is assessed per-segment (split on shell operators), and the
    // highest risk across all segments wins. This prevents bypasses like
    // `ls && rm -rf /` from being classified as Low just because `ls` is safe.

    /// Classify command risk. Any high-risk segment marks the whole command high.
    pub fn command_risk_level(&self, command: &str) -> CommandRiskLevel {
        let mut saw_medium = false;

        for segment in split_unquoted_segments(command) {
            let cmd_part = skip_env_assignments(&segment);
            let mut words = cmd_part.split_whitespace();
            let Some(base_raw) = words.next() else {
                continue;
            };

            let base = base_raw
                .rsplit('/')
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();

            let args: Vec<String> = words.map(|w| w.to_ascii_lowercase()).collect();
            let joined_segment = cmd_part.to_ascii_lowercase();

            // High-risk commands
            if matches!(
                base.as_str(),
                "rm" | "mkfs"
                    | "dd"
                    | "shutdown"
                    | "reboot"
                    | "halt"
                    | "poweroff"
                    | "sudo"
                    | "su"
                    | "chown"
                    | "chmod"
                    | "useradd"
                    | "userdel"
                    | "usermod"
                    | "passwd"
                    | "mount"
                    | "umount"
                    | "iptables"
                    | "ufw"
                    | "firewall-cmd"
                    | "curl"
                    | "wget"
                    | "nc"
                    | "ncat"
                    | "netcat"
                    | "scp"
                    | "ssh"
                    | "ftp"
                    | "telnet"
            ) {
                return CommandRiskLevel::High;
            }

            if joined_segment.contains("rm -rf /")
                || joined_segment.contains("rm -fr /")
                || joined_segment.contains(":(){:|:&};:")
            {
                return CommandRiskLevel::High;
            }

            // Medium-risk commands (state-changing, but not inherently destructive)
            let medium = match base.as_str() {
                "git" => args.first().is_some_and(|verb| {
                    matches!(
                        verb.as_str(),
                        "commit"
                            | "push"
                            | "reset"
                            | "clean"
                            | "rebase"
                            | "merge"
                            | "cherry-pick"
                            | "revert"
                            | "branch"
                            | "checkout"
                            | "switch"
                            | "tag"
                    )
                }),
                "npm" | "pnpm" | "yarn" => args.first().is_some_and(|verb| {
                    matches!(
                        verb.as_str(),
                        "install" | "add" | "remove" | "uninstall" | "update" | "publish"
                    )
                }),
                "cargo" => args.first().is_some_and(|verb| {
                    matches!(
                        verb.as_str(),
                        "add" | "remove" | "install" | "clean" | "publish"
                    )
                }),
                "touch" | "mkdir" | "mv" | "cp" | "ln" => true,
                _ => false,
            };

            saw_medium |= medium;
        }

        if saw_medium {
            CommandRiskLevel::Medium
        } else {
            CommandRiskLevel::Low
        }
    }

    // ── Command Execution Policy Gate ──────────────────────────────────────
    // Validation follows a strict precedence order:
    //   1. Read-only
    //   2. Admit (carrier / shape / size) — never ApprovalHub
    //   3. Unsafe shell constructs
    //   4. Wait-only executables
    //   5. Allowlist (human approval cannot widen)
    //   6. Secret Ask / privilege Once (only if the invoke could still run)

    fn command_or_path_touches_secrets(&self, command: &str) -> bool {
        command_touches_secret_material_in(command, Some(self.workspace_dir.as_path()))
    }

    /// File-tool secret gate (same `secret_path_mode` as shell; no allowlist).
    pub fn validate_secret_path_access(&self, path: &str, approved: bool) -> Result<(), String> {
        admit_file_path(path)?;
        if !path_touches_secret_material(path) {
            return Ok(());
        }
        match self.secret_path_mode {
            SecretPathMode::Allow => Ok(()),
            SecretPathMode::Ask if approved => Ok(()),
            SecretPathMode::Ask => Err(self.format_command_policy_error(
                "Path names a credential file; approve Once to allow this invocation.",
                path,
                true,
            )),
            SecretPathMode::Deny => Err(self.format_command_policy_error(
                "Path blocked: secret or credential file is not readable by the agent.",
                path,
                false,
            )),
        }
    }

    /// Validate full command execution policy (allowlist + risk gate).
    pub fn validate_command_execution(
        &self,
        command: &str,
        approved: bool,
    ) -> Result<CommandRiskLevel, String> {
        if self.autonomy == AutonomyLevel::ReadOnly {
            return Err(self.format_command_policy_error(
                "Security policy: read-only mode blocks shell execution.",
                command,
                false,
            ));
        }

        admit_shell_command(command)?;

        if !self.passes_shell_safety_gates(command) {
            return Err(self.format_command_policy_error(
                "Command blocked: unsafe shell construct (injection, redirect, or dangerous args).",
                command,
                false,
            ));
        }

        if command_is_wait_only_executable(command) {
            return Err(self.format_command_policy_error(
                "Command blocked: wait-only executables (sleep/usleep) are not allowed.",
                command,
                false,
            ));
        }

        // Hard allowlist: human approval cannot widen allowed_commands (VL-SEC-009 / H).
        if !self.segments_are_allowlisted(command) {
            return Err(self.format_command_policy_error(
                "Command not allowed by security policy (not in allowed_commands).",
                command,
                false,
            ));
        }

        // Secret material: argv tokens, workspace bash/sh bodies (after admission).
        if self.command_or_path_touches_secrets(command) {
            match self.secret_path_mode {
                SecretPathMode::Allow => {}
                SecretPathMode::Ask if approved => {}
                SecretPathMode::Ask => {
                    return Err(self.format_command_policy_error(
                        "Command reads a credential path; approve Once to allow this invocation.",
                        command,
                        true,
                    ));
                }
                SecretPathMode::Deny => {
                    return Err(self.format_command_policy_error(
                        "Command blocked: secret or credential path is not readable by the agent.",
                        command,
                        false,
                    ));
                }
            }
        }

        let risk = self.command_risk_level(command);

        // Policy B: privilege/package commands always need ApprovalHub even under Full,
        // so the approved invocation can skip Landlock/NNP.
        if self.escape_on_approval && command_requires_sandbox_escape_approval(command) && !approved
        {
            return Err(self.format_command_policy_error(
                "Command requires explicit human approval: privileged/package operation \
                 (sandbox escape_on_approval).",
                command,
                true,
            ));
        }

        if risk == CommandRiskLevel::High {
            let needs_prompt = self.autonomy == AutonomyLevel::Supervised
                && (!approved || self.block_high_risk_commands);
            if needs_prompt && !approved {
                return Err(self.format_command_policy_error(
                    "Command requires explicit human approval: high-risk operation.",
                    command,
                    true,
                ));
            }
        }

        if risk == CommandRiskLevel::Medium
            && self.autonomy == AutonomyLevel::Supervised
            && self.require_approval_for_medium_risk
            && !approved
        {
            return Err(self.format_command_policy_error(
                "Command requires explicit human approval: medium-risk operation.",
                command,
                true,
            ));
        }

        Ok(risk)
    }

    // ── Layered Command Allowlist ──────────────────────────────────────────
    // Defence-in-depth: five independent gates run in order before the
    // per-segment allowlist check. Each gate targets a specific bypass
    // technique. If any gate rejects, the whole command is blocked.

    /// Check if a shell command is allowed.
    ///
    /// Validates the **entire** command string, not just the first word:
    /// - Blocks subshell operators (`` ` ``, `$(`) that hide arbitrary execution
    /// - Splits on command separators (`|`, `&&`, `||`, `;`, newlines) and
    ///   validates each sub-command against the allowlist
    /// - Blocks single `&` background chaining (`&&` remains supported)
    /// - Blocks output redirections (`>`, `>>`) that could write outside workspace
    /// - Blocks dangerous arguments (e.g. `find -exec`, `git config`)
    pub fn is_command_allowed(&self, command: &str) -> bool {
        self.passes_shell_safety_gates(command) && self.segments_are_allowlisted(command)
    }

    /// Hard shell gates that human approval cannot override (injection, redirects, etc.).
    pub fn passes_shell_safety_gates(&self, command: &str) -> bool {
        if self.autonomy == AutonomyLevel::ReadOnly {
            return false;
        }

        if command.contains('`')
            || command.contains("$(")
            || command.contains("${")
            || command.contains("<(")
            || command.contains(">(")
        {
            return false;
        }

        if contains_unquoted_unsafe_redirect(command) {
            return false;
        }

        if command
            .split_whitespace()
            .any(|w| w == "tee" || w.ends_with("/tee"))
        {
            return false;
        }

        if contains_unquoted_single_ampersand(command) {
            return false;
        }

        let segments = split_unquoted_segments(command);
        for segment in &segments {
            let cmd_part = skip_env_assignments(segment);
            let mut words = cmd_part.split_whitespace();
            let base_raw = words.next().unwrap_or("");
            let base_cmd = base_raw.rsplit('/').next().unwrap_or("");

            if base_cmd.is_empty() {
                continue;
            }

            let args: Vec<String> = words.map(|w| w.to_ascii_lowercase()).collect();
            if !self.is_args_safe(base_cmd, &args) {
                return false;
            }
        }

        segments.iter().any(|s| {
            let s = skip_env_assignments(s.trim());
            s.split_whitespace().next().is_some_and(|w| !w.is_empty())
        })
    }

    /// Executable basenames for each shell segment (same split rules as allowlist).
    pub fn base_executables(command: &str) -> Vec<String> {
        let mut out = Vec::new();
        for segment in split_unquoted_segments(command) {
            let cmd_part = skip_env_assignments(&segment);
            let mut words = cmd_part.split_whitespace();
            let base_raw = words.next().unwrap_or("");
            let base_cmd = base_raw.rsplit('/').next().unwrap_or("");
            if !base_cmd.is_empty() {
                out.push(base_cmd.to_string());
            }
        }
        out
    }

    fn segments_are_allowlisted(&self, command: &str) -> bool {
        let bases = Self::base_executables(command);
        if bases.is_empty() {
            return false;
        }
        bases.iter().all(|base_cmd| {
            self.allowed_commands
                .iter()
                .any(|allowed| allowed == base_cmd)
        })
    }

    fn format_command_policy_error(
        &self,
        headline: &str,
        command: &str,
        approval_eligible: bool,
    ) -> String {
        let kind = if approval_eligible {
            "[needs_approval]"
        } else {
            "[policy_deny]"
        };
        let displayed = crate::security::redact_secret_literals(command);
        let mut msg = format!("{kind} {headline}\n   Command: {displayed}");
        if approval_eligible {
            msg.push_str(
                "\n\n   Interactive approval: [Y]es = run once, [A]lways = skip risk prompts for this \
                 executable basename this session, [N]o = deny this call, [!] Never = persist deny.",
            );
            use std::fmt::Write as _;
            let _ = write!(
                msg,
                "\n   Config: add executable names to [autonomy].allowed_commands (current: {}).",
                self.allowed_commands.join(", ")
            );
        } else if !self.segments_are_allowlisted(command) {
            use std::fmt::Write as _;
            let _ = write!(
                msg,
                "\n\n   Next steps (CLI + Web):\n\
                   1. Add the executable basename to [autonomy].allowed_commands in config.toml \
(current: {}).\n\
                   2. For common ops reads (df/du/free/uname/…), merge \
`examples/profiles/ops-readonly.toml` — do not overwrite an existing config.\n\
                   3. If workspace `agent-policy.yaml` allows `autonomy.allowed_commands` via \
self_adjust, use the `policy_patch` tool; otherwise edit config.toml (no silent rewrite).\n\
                   4. Interactive approval cannot widen the allowlist (VL-SEC-009).\n\
                   Docs: docs/policy-approval-reference.md#ops-readonly-profile",
                self.allowed_commands.join(", ")
            );
        }
        if command_requires_privilege_hint(command) {
            msg.push_str(
                "\n\n   Privilege note: sudo/su/run-as-root needs the binary in allowed_commands plus your approval, \
                 or run the host action yourself and retry a non-privileged command.",
            );
        }
        msg
    }

    /// Check for dangerous arguments that allow sub-command execution.
    fn is_args_safe(&self, base: &str, args: &[String]) -> bool {
        let base = base.to_ascii_lowercase();
        match base.as_str() {
            "find" => {
                // find -exec and find -ok allow arbitrary command execution
                !args.iter().any(|arg| arg == "-exec" || arg == "-ok")
            }
            "git" => {
                // git config, alias, and -c can be used to set dangerous options
                // (e.g. git config core.editor "rm -rf /")
                !args.iter().any(|arg| {
                    arg == "config"
                        || arg.starts_with("config.")
                        || arg == "alias"
                        || arg.starts_with("alias.")
                        || arg == "-c"
                })
            }
            _ => true,
        }
    }

    // ── Path Validation ────────────────────────────────────────────────
    // Layered checks: null-byte injection → component-level traversal →
    // URL-encoded traversal → tilde expansion → absolute-path block →
    // forbidden-prefix match. Each layer addresses a distinct escape
    // technique; together they enforce workspace confinement.

    /// Check if a file path is allowed (no path traversal, within workspace)
    pub fn is_path_allowed(&self, path: &str) -> bool {
        // Block null bytes (can truncate paths in C-backed syscalls)
        if path.contains('\0') {
            return false;
        }

        // Block path traversal: check for ".." as a path component
        if Path::new(path)
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return false;
        }

        // Block URL-encoded traversal attempts (e.g. ..%2f)
        let lower = path.to_lowercase();
        if lower.contains("..%2f") || lower.contains("%2f..") {
            return false;
        }

        // VL-SEC-012 / VL-NA-040: host temp roots rewrite into workspace or graph scratch.
        let path = self.rewrite_temp_tool_path(path);

        // Expand tilde for comparison
        let expanded = if let Some(stripped) = path.strip_prefix("~/") {
            if let Some(home) = std::env::var("HOME").ok().map(PathBuf::from) {
                home.join(stripped).to_string_lossy().to_string()
            } else {
                path.to_string()
            }
        } else {
            path.to_string()
        };

        // Block absolute paths when workspace_only is set
        if self.workspace_only && Path::new(&expanded).is_absolute() {
            return false;
        }

        // Block forbidden paths using path-component-aware matching
        let expanded_path = Path::new(&expanded);
        for forbidden in &self.forbidden_paths {
            let forbidden_expanded = if let Some(stripped) = forbidden.strip_prefix("~/") {
                if let Some(home) = std::env::var("HOME").ok().map(PathBuf::from) {
                    home.join(stripped).to_string_lossy().to_string()
                } else {
                    forbidden.clone()
                }
            } else {
                forbidden.clone()
            };
            let forbidden_path = Path::new(&forbidden_expanded);
            if expanded_path.starts_with(forbidden_path) {
                return false;
            }
        }

        true
    }

    /// Map `/tmp` / `/var/tmp` onto workspace `.velaclaw/tmp` (VL-SEC-012).
    #[must_use]
    pub fn rewrite_temp_tool_path(&self, path: &str) -> String {
        let rewritten = rewrite_temp_tool_path(path);
        let Some(root) = self.graph_scratch_rel.as_deref() else {
            return rewritten;
        };
        if rewritten.starts_with(root) {
            return rewritten;
        }
        if rewritten == SCRATCH_REL || rewritten.starts_with(&format!("{SCRATCH_REL}/")) {
            let rest = rewritten
                .strip_prefix(SCRATCH_REL)
                .unwrap_or("")
                .trim_start_matches('/');
            if rest.starts_with("graphs/") {
                return rewritten;
            }
            if rest.is_empty() {
                return root.to_string();
            }
            return format!("{root}/{rest}");
        }
        rewritten
    }

    /// Workspace join after temp rewrite (file tools must not `join("/tmp/...")`).
    #[must_use]
    pub fn tool_fs_path(&self, path: &str) -> PathBuf {
        let rewritten = self.rewrite_temp_tool_path(path);
        let p = Path::new(&rewritten);
        if p.is_absolute() {
            PathBuf::from(rewritten)
        } else {
            self.workspace_dir.join(rewritten)
        }
    }

    /// Validate that a resolved path is still inside the workspace.
    /// Call this AFTER joining `workspace_dir` + relative path and canonicalizing.
    pub fn is_resolved_path_allowed(&self, resolved: &Path) -> bool {
        // Must be under workspace_dir (prevents symlink escapes).
        // Prefer canonical workspace root so `/a/../b` style config paths don't
        // cause false positives or negatives.
        let workspace_root = self
            .workspace_dir
            .canonicalize()
            .unwrap_or_else(|_| self.workspace_dir.clone());
        resolved.starts_with(workspace_root)
    }

    /// Allow reads via workspace-relative paths that traverse symlinked trees (home-lab).
    pub fn allows_workspace_symlink_read(&self, logical_full: &Path, resolved: &Path) -> bool {
        if self.is_resolved_path_allowed(resolved) {
            return true;
        }
        self.autonomy == AutonomyLevel::Full && logical_full.starts_with(&self.workspace_dir)
    }

    /// Check if autonomy level permits any action at all
    pub fn can_act(&self) -> bool {
        self.autonomy != AutonomyLevel::ReadOnly
    }

    // ── Tool Operation Gating ──────────────────────────────────────────────
    // Read operations bypass autonomy and rate checks because they have
    // no side effects. Act operations must pass both the autonomy gate
    // (not read-only) and the sliding-window rate limiter.

    /// Enforce policy for a tool operation.
    ///
    /// Read operations are always allowed by autonomy/rate gates.
    /// Act operations require non-readonly autonomy and available action budget.
    pub fn enforce_tool_operation(
        &self,
        operation: ToolOperation,
        operation_name: &str,
    ) -> Result<(), String> {
        match operation {
            ToolOperation::Read => Ok(()),
            ToolOperation::Act => {
                if !self.can_act() {
                    return Err(format!(
                        "Security policy: read-only mode, cannot perform '{operation_name}'"
                    ));
                }

                if !self.record_action() {
                    return Err("Rate limit exceeded: action budget exhausted".to_string());
                }

                Ok(())
            }
        }
    }

    /// Record an action and check if the rate limit has been exceeded.
    /// Returns `true` if the action is allowed, `false` if rate-limited.
    pub fn record_action(&self) -> bool {
        let count = self.tracker.record();
        count <= self.max_actions_per_hour as usize
    }

    /// Check if the rate limit would be exceeded without recording.
    pub fn is_rate_limited(&self) -> bool {
        self.tracker.count() >= self.max_actions_per_hour as usize
    }

    /// Append configured execution boundaries to the system prompt so the model does not
    /// invent restrictions. Operators still tune policy via config — this only surfaces it.
    pub fn append_execution_policy_prompt(&self, prompt: &mut String, extras: &PolicyPromptExtras) {
        use std::fmt::Write;

        prompt.push_str("## Execution Policy (configured — do not guess)\n\n");
        let _ = writeln!(
            prompt,
            "- Autonomy level: `{}`",
            serde_json::to_string(&self.autonomy)
                .unwrap_or_else(|_| "\"unknown\"".into())
                .trim_matches('"')
        );
        let _ = writeln!(prompt, "- Workspace: `{}`", self.workspace_dir.display());
        if !extras.runtime_kind.is_empty() {
            let docker_note = if extras.runtime_kind == "docker" {
                "shell runs inside the configured Docker runtime"
            } else {
                "[runtime.docker] is unused; shell runs on the host process"
            };
            let _ = writeln!(
                prompt,
                "- Runtime kind: `{}` — {docker_note}. Do **not** describe the host as a container unless kind is `docker`.",
                extras.runtime_kind
            );
        }
        if !extras.sandbox_name.is_empty() {
            let _ = writeln!(
                prompt,
                "- OS sandbox: `{}` — Permission denied on paths outside the sandbox allowlist is Landlock (or the configured backend), not Docker.",
                extras.sandbox_name
            );
        }
        if self.escape_on_approval {
            prompt.push_str(
                "- Sandbox escape_on_approval: **enabled** — after human approval, shell skips \
                 OS Landlock/NNP for that invocation (sudo/apt can work). Unapproved shell stays sandboxed.\n",
            );
        } else if !extras.sandbox_name.is_empty() {
            prompt.push_str(
                "- Sandbox escape_on_approval: disabled (default) — human approval does **not** \
                 remove Landlock; sudo may fail with no-new-privileges.\n",
            );
        }
        let _ = writeln!(
            prompt,
            "- `workspace_only`: {} — file tools (`file_read`, `glob_search`, etc.) only accept paths relative to the workspace unless you use the `shell` tool.",
            self.workspace_only
        );
        let _ = writeln!(
            prompt,
            "- Allowed shell commands: {}",
            self.allowed_commands.join(", ")
        );
        if self.forbidden_paths.is_empty() {
            prompt.push_str("- Forbidden path prefixes: (none)\n");
        } else {
            let _ = writeln!(
                prompt,
                "- Forbidden path prefixes: {}",
                self.forbidden_paths.join(", ")
            );
        }
        let _ = writeln!(
            prompt,
            "- HTTP request tool: {}",
            if extras.http_request_enabled {
                "enabled"
            } else {
                "disabled — use `shell` with `curl` when allowed, or enable [http_request] in config.toml"
            }
        );
        if extras.proxy_enabled {
            if let Some(ref proxy) = extras.proxy_http {
                let _ = writeln!(
                    prompt,
                    "- Proxy: enabled (`{proxy}`). Use the `proxy_config` tool to inspect or adjust proxy env."
                );
            } else {
                prompt.push_str("- Proxy: enabled (see config [proxy] section).\n");
            }
        } else {
            prompt.push_str("- Proxy: disabled.\n");
        }
        if extras.policy_patch_enabled {
            prompt.push_str(
                "- Policy self-adjust: `policy_patch` tool is available for L2.5 overrides.\n",
            );
        } else {
            prompt.push_str(
                "- Policy self-adjust: `policy_patch` is not registered in this session.\n",
            );
        }
        if extras.self_adjust_allowed_writes.is_empty()
            && extras.self_adjust_denied_writes.is_empty()
        {
            prompt.push_str(
                "- self_adjust writes: default — only `approval.session_allowlist` / `approval.*`; \
                 `security.*`, `gateway.*`, and `channels.*` are denied.\n",
            );
        } else {
            let _ = writeln!(
                prompt,
                "- self_adjust allowed_writes: {}",
                extras.self_adjust_allowed_writes.join(", ")
            );
            let _ = writeln!(
                prompt,
                "- self_adjust denied_writes: {}",
                extras.self_adjust_denied_writes.join(", ")
            );
        }
        prompt.push_str(
            "\n**CRITICAL tool-use rules:**\n\
             - When the user asks you to run a command, read a file, or check connectivity, ALWAYS invoke the matching tool first.\n\
             - NEVER claim a command or path is blocked without a real `<tool_result>` error from an attempted tool call.\n\
             - If a tool fails, quote the exact error to the user in plain language and say what to change in `config.toml` — do not ask the user to edit source code.\n\
             - If a `<tool_result>` starts with `[sandbox_deny]` or `[needs_approval]`, wait for the human approval modal (Once/Always/No/Never). Do not invent a second tool.\n\
             - If a `<tool_result>` starts with `[policy_deny]`, do **not** retry equivalent `ls`/`find`/`cat` on the same path unless the operator changed allowlist/config.\n\
             - Isolated GitHub CLI: invoke `gh` as the **first** executable (allowlisted). Auth is `GH_TOKEN`/`GITHUB_TOKEN` from the daemon environment when set. Do not prefix with `echo`/`env`/`git &&`. Prefer `gh`; do not scan PAT/key lists. Credential paths require the ApprovalHub Once modal (`[needs_approval]`), not `request_human_input`. Local profile inherits daemon env instead.\n\
             - Optional local tool catalog: a workspace `tools/` directory (or symlink) when present.\n\n",
        );
    }

    /// Build from config sections
    pub fn from_config(
        autonomy_config: &crate::config::AutonomyConfig,
        workspace_dir: &Path,
    ) -> Self {
        let effective = normalize_autonomy_config(autonomy_config);
        Self::from_normalized(&effective, workspace_dir)
    }

    /// Build from L1 `config.toml` merged with L2 `agent-policy.yaml` when present.
    pub fn from_workspace_config(config: &crate::config::Config) -> anyhow::Result<Self> {
        #[cfg(feature = "ai-protocol")]
        let mut policy = {
            let autonomy = crate::config::resolve_effective_autonomy(config)?;
            Self::from_config(&autonomy, &config.workspace_dir)
        };
        #[cfg(not(feature = "ai-protocol"))]
        let mut policy = Self::from_config(&config.autonomy, &config.workspace_dir);
        policy.escape_on_approval = config.security.sandbox.escape_on_approval;
        apply_security_profile(&mut policy, config);
        Ok(policy)
    }

    fn from_normalized(effective: &crate::config::AutonomyConfig, workspace_dir: &Path) -> Self {
        Self {
            autonomy: effective.level,
            workspace_dir: workspace_dir.to_path_buf(),
            workspace_only: effective.workspace_only,
            allowed_commands: effective.allowed_commands.clone(),
            forbidden_paths: effective.forbidden_paths.clone(),
            max_actions_per_hour: effective.max_actions_per_hour,
            max_cost_per_day_cents: effective.max_cost_per_day_cents,
            require_approval_for_medium_risk: effective.require_approval_for_medium_risk,
            block_high_risk_commands: effective.block_high_risk_commands,
            escape_on_approval: false,
            inherit_process_env: false,
            secret_path_mode: SecretPathMode::Deny,
            profile: None,
            tracker: ActionTracker::new(),
            graph_scratch_rel: None,
        }
    }
}

fn apply_security_profile(policy: &mut SecurityPolicy, config: &crate::config::Config) {
    use crate::config::SecurityProfile;
    policy.profile = config.security.profile;
    policy.inherit_process_env = config.security.inherit_process_env.unwrap_or(false);
    match config.security.profile {
        Some(SecurityProfile::Local) => {
            if config.security.inherit_process_env.is_none() {
                policy.inherit_process_env = true;
            }
            policy.secret_path_mode = SecretPathMode::Allow;
            policy.workspace_only = false;
            policy.forbidden_paths.retain(|p| {
                p.as_str() != "/home" && p.as_str() != "/tmp" && p.as_str() != "~/.config"
            });
        }
        Some(SecurityProfile::Isolated) => {
            policy.secret_path_mode = SecretPathMode::Ask;
        }
        Some(SecurityProfile::Readonly) => {
            policy.autonomy = AutonomyLevel::ReadOnly;
        }
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_policy() -> SecurityPolicy {
        SecurityPolicy::default()
    }

    fn readonly_policy() -> SecurityPolicy {
        SecurityPolicy {
            autonomy: AutonomyLevel::ReadOnly,
            ..SecurityPolicy::default()
        }
    }

    fn full_policy() -> SecurityPolicy {
        SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            ..SecurityPolicy::default()
        }
    }

    // ── AutonomyLevel ────────────────────────────────────────

    #[test]
    fn autonomy_default_is_supervised() {
        assert_eq!(AutonomyLevel::default(), AutonomyLevel::Supervised);
    }

    #[test]
    fn autonomy_serde_roundtrip() {
        let json = serde_json::to_string(&AutonomyLevel::Full).unwrap();
        assert_eq!(json, "\"full\"");
        let parsed: AutonomyLevel = serde_json::from_str("\"readonly\"").unwrap();
        assert_eq!(parsed, AutonomyLevel::ReadOnly);
        let parsed2: AutonomyLevel = serde_json::from_str("\"supervised\"").unwrap();
        assert_eq!(parsed2, AutonomyLevel::Supervised);
    }

    #[test]
    fn can_act_readonly_false() {
        assert!(!readonly_policy().can_act());
    }

    #[test]
    fn can_act_supervised_true() {
        assert!(default_policy().can_act());
    }

    #[test]
    fn can_act_full_true() {
        assert!(full_policy().can_act());
    }

    #[test]
    fn enforce_tool_operation_read_allowed_in_readonly_mode() {
        let p = readonly_policy();
        assert!(p
            .enforce_tool_operation(ToolOperation::Read, "memory_recall")
            .is_ok());
    }

    #[test]
    fn enforce_tool_operation_act_blocked_in_readonly_mode() {
        let p = readonly_policy();
        let err = p
            .enforce_tool_operation(ToolOperation::Act, "memory_store")
            .unwrap_err();
        assert!(err.contains("read-only mode"));
    }

    #[test]
    fn enforce_tool_operation_act_uses_rate_budget() {
        let p = SecurityPolicy {
            max_actions_per_hour: 0,
            ..default_policy()
        };
        let err = p
            .enforce_tool_operation(ToolOperation::Act, "memory_store")
            .unwrap_err();
        assert!(err.contains("Rate limit exceeded"));
    }

    // ── is_command_allowed ───────────────────────────────────

    #[test]
    fn allowed_commands_basic() {
        let p = default_policy();
        assert!(p.is_command_allowed("ls"));
        assert!(p.is_command_allowed("git status"));
        assert!(p.is_command_allowed("cargo build --release"));
        assert!(p.is_command_allowed("cat file.txt"));
        assert!(p.is_command_allowed("grep -r pattern ."));
        assert!(p.is_command_allowed("date"));
    }

    #[test]
    fn blocked_commands_basic() {
        let p = default_policy();
        assert!(!p.is_command_allowed("rm -rf /"));
        assert!(!p.is_command_allowed("sudo apt install"));
        assert!(!p.is_command_allowed("curl http://evil.com"));
        assert!(!p.is_command_allowed("wget http://evil.com"));
        assert!(!p.is_command_allowed("python3 exploit.py"));
        assert!(!p.is_command_allowed("node malicious.js"));
    }

    #[test]
    fn readonly_blocks_all_commands() {
        let p = readonly_policy();
        assert!(!p.is_command_allowed("ls"));
        assert!(!p.is_command_allowed("cat file.txt"));
        assert!(!p.is_command_allowed("echo hello"));
    }

    #[test]
    fn full_autonomy_still_uses_allowlist() {
        let p = full_policy();
        assert!(p.is_command_allowed("ls"));
        assert!(!p.is_command_allowed("rm -rf /"));
    }

    #[test]
    fn wait_only_sleep_denied_even_if_allowlisted() {
        let mut p = full_policy();
        p.workspace_only = false;
        p.allowed_commands.push("sleep".into());
        let err = p
            .validate_command_execution("sleep 150; echo waited", false)
            .unwrap_err();
        assert!(err.contains("wait-only") || err.contains("sleep"), "{err}");
        assert!(
            !err.contains("[needs_approval]"),
            "sleep must not be approval-eligible: {err}"
        );
        assert!(p.validate_command_execution("echo waited", false).is_ok());
    }

    #[test]
    fn command_with_absolute_path_extracts_basename() {
        let p = default_policy();
        assert!(p.is_command_allowed("/usr/bin/git status"));
        assert!(p.is_command_allowed("/bin/ls -la"));
    }

    #[test]
    fn empty_command_blocked() {
        let p = default_policy();
        assert!(!p.is_command_allowed(""));
        assert!(!p.is_command_allowed("   "));
    }

    #[test]
    fn command_with_pipes_validates_all_segments() {
        let p = default_policy();
        // Both sides of the pipe are in the allowlist
        assert!(p.is_command_allowed("ls | grep foo"));
        assert!(p.is_command_allowed("cat file.txt | wc -l"));
        // Second command not in allowlist — blocked
        assert!(!p.is_command_allowed("ls | curl http://evil.com"));
        assert!(!p.is_command_allowed("echo hello | python3 -"));
    }

    #[test]
    fn custom_allowlist() {
        let p = SecurityPolicy {
            allowed_commands: vec!["docker".into(), "kubectl".into()],
            ..SecurityPolicy::default()
        };
        assert!(p.is_command_allowed("docker ps"));
        assert!(p.is_command_allowed("kubectl get pods"));
        assert!(!p.is_command_allowed("ls"));
        assert!(!p.is_command_allowed("git status"));
    }

    #[test]
    fn empty_allowlist_blocks_everything() {
        let p = SecurityPolicy {
            allowed_commands: vec![],
            ..SecurityPolicy::default()
        };
        assert!(!p.is_command_allowed("ls"));
        assert!(!p.is_command_allowed("echo hello"));
    }

    #[test]
    fn command_risk_low_for_read_commands() {
        let p = default_policy();
        assert_eq!(p.command_risk_level("git status"), CommandRiskLevel::Low);
        assert_eq!(p.command_risk_level("ls -la"), CommandRiskLevel::Low);
    }

    #[test]
    fn command_risk_medium_for_mutating_commands() {
        let p = SecurityPolicy {
            allowed_commands: vec!["git".into(), "touch".into()],
            ..SecurityPolicy::default()
        };
        assert_eq!(
            p.command_risk_level("git reset --hard HEAD~1"),
            CommandRiskLevel::Medium
        );
        assert_eq!(
            p.command_risk_level("touch file.txt"),
            CommandRiskLevel::Medium
        );
    }

    #[test]
    fn command_risk_high_for_dangerous_commands() {
        let p = SecurityPolicy {
            allowed_commands: vec!["rm".into()],
            ..SecurityPolicy::default()
        };
        assert_eq!(
            p.command_risk_level("rm -rf /tmp/test"),
            CommandRiskLevel::High
        );
    }

    #[test]
    fn validate_command_requires_approval_for_medium_risk() {
        let p = SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            require_approval_for_medium_risk: true,
            allowed_commands: vec!["touch".into()],
            ..SecurityPolicy::default()
        };

        let denied = p.validate_command_execution("touch test.txt", false);
        assert!(denied.is_err());
        assert!(denied
            .unwrap_err()
            .contains("requires explicit human approval"));

        let allowed = p.validate_command_execution("touch test.txt", true);
        assert_eq!(allowed.unwrap(), CommandRiskLevel::Medium);
    }

    fn validate_command_allows_high_risk_when_human_approved() {
        let p = SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            allowed_commands: vec!["rm".into()],
            ..SecurityPolicy::default()
        };

        let result = p.validate_command_execution("rm -rf /tmp/test", true);
        assert!(result.is_ok());
    }

    #[test]
    fn validate_command_allowlist_not_bypassed_when_human_approved() {
        let p = default_policy();
        let denied = p.validate_command_execution("python3 -c 'print(1)'", false);
        assert!(denied.is_err());
        let still_denied = p.validate_command_execution("python3 -c 'print(1)'", true);
        assert!(still_denied.is_err());
        assert!(still_denied
            .unwrap_err()
            .contains("not in allowed_commands"));
    }

    #[test]
    fn allowlist_deny_mentions_ops_readonly_and_sec009() {
        let p = default_policy();
        let err = p
            .validate_command_execution("df -h", false)
            .expect_err("df should be denied by default allowlist");
        assert!(err.contains("ops-readonly"));
        assert!(err.contains("VL-SEC-009"));
        assert!(err.contains("allowed_commands"));
        assert!(err.contains("policy_patch") || err.contains("config.toml"));
    }

    #[test]
    fn validate_command_blocks_high_risk_by_default() {
        let p = SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            allowed_commands: vec!["rm".into()],
            ..SecurityPolicy::default()
        };

        let result = p.validate_command_execution("rm -rf /tmp/test", false);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("high-risk"));
    }

    #[test]
    fn normalize_autonomy_config_full_merges_extra_commands() {
        use crate::config::AutonomyConfig;
        use crate::security::AutonomyLevel;

        let input = AutonomyConfig {
            level: AutonomyLevel::Full,
            allowed_commands: vec!["echo".into()],
            forbidden_paths: vec!["/home".into(), "/etc".into()],
            ..AutonomyConfig::default()
        };
        let effective = normalize_autonomy_config(&input);
        assert!(effective.allowed_commands.contains(&"curl".into()));
        assert!(effective.allowed_commands.contains(&"ssh".into()));
        assert!(effective.allowed_commands.contains(&"echo".into()));
        assert!(!effective.forbidden_paths.contains(&"/home".into()));
        assert!(effective.forbidden_paths.contains(&"/etc".into()));
    }

    #[test]
    fn normalize_autonomy_config_supervised_is_unchanged() {
        use crate::config::AutonomyConfig;
        use crate::security::AutonomyLevel;

        let input = AutonomyConfig {
            level: AutonomyLevel::Supervised,
            allowed_commands: vec!["echo".into()],
            forbidden_paths: vec!["/home".into()],
            ..AutonomyConfig::default()
        };
        let effective = normalize_autonomy_config(&input);
        assert_eq!(effective.allowed_commands, vec!["echo".to_string()]);
        assert!(effective.forbidden_paths.contains(&"/home".into()));
    }

    #[test]
    fn append_execution_policy_prompt_lists_allowed_commands() {
        let policy = SecurityPolicy {
            allowed_commands: vec!["echo".into(), "curl".into()],
            ..SecurityPolicy::default()
        };
        let mut prompt = String::new();
        policy.append_execution_policy_prompt(
            &mut prompt,
            &PolicyPromptExtras {
                runtime_kind: "native".into(),
                sandbox_name: "landlock".into(),
                ..PolicyPromptExtras::default()
            },
        );
        assert!(prompt.contains("Allowed shell commands: echo, curl"));
        assert!(prompt.contains("Runtime kind: `native`"));
        assert!(prompt.contains("OS sandbox: `landlock`"));
        assert!(prompt.contains("escape_on_approval: disabled"));
        assert!(prompt.contains("NEVER claim a command or path is blocked"));
        assert!(prompt.contains("[sandbox_deny]"));
    }

    #[test]
    fn secret_material_denied_even_when_approved() {
        let p = SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            allowed_commands: vec!["cat".into(), "ls".into()],
            escape_on_approval: true,
            ..SecurityPolicy::default()
        };
        let denied = p.validate_command_execution("cat /home/alex/github_token_list.txt", true);
        assert!(denied.is_err());
        let err = denied.unwrap_err();
        assert!(err.contains("[policy_deny]"), "{err}");
        assert!(err.contains("secret or credential"));
        let ls_ok = p.validate_command_execution("ls /home/alex/github_token_list.txt", false);
        assert!(ls_ok.is_err(), "ls of token file must also be hard-denied");
        assert!(p
            .validate_command_execution("cat ./id_ed25519_lan", true)
            .is_err());
        assert!(
            p.validate_command_execution("cat ./raid_rsa", true).is_ok(),
            "id_rsa must not match as a substring of raid_rsa"
        );
    }

    #[test]
    fn secret_material_ask_allows_when_approved() {
        let p = SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            allowed_commands: vec!["cat".into()],
            secret_path_mode: SecretPathMode::Ask,
            ..SecurityPolicy::default()
        };
        let denied = p.validate_command_execution("cat /tmp/github_token_list.txt", false);
        assert!(denied.unwrap_err().contains("[needs_approval]"));
        assert!(p
            .validate_command_execution("cat /tmp/github_token_list.txt", true)
            .is_ok());
    }

    #[test]
    fn bash_workspace_script_body_is_secret_touching() {
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("scan.sh");
        std::fs::write(&script, "#!/bin/sh\ncat ~/github_token_list.txt\n").unwrap();
        let p = SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            workspace_dir: tmp.path().to_path_buf(),
            allowed_commands: vec!["bash".into(), "sh".into(), "cat".into()],
            secret_path_mode: SecretPathMode::Ask,
            ..SecurityPolicy::default()
        };
        let cmd = format!("bash {}", script.display());
        assert!(
            command_touches_secret_material_in(&cmd, Some(tmp.path())),
            "script body must count even without the basename on argv"
        );
        assert!(p
            .validate_command_execution(&cmd, false)
            .unwrap_err()
            .contains("[needs_approval]"));
        assert!(p.validate_command_execution(&cmd, true).is_ok());
    }

    #[test]
    fn file_path_secret_access_follows_mode() {
        let deny = SecurityPolicy {
            secret_path_mode: SecretPathMode::Deny,
            ..SecurityPolicy::default()
        };
        assert!(deny
            .validate_secret_path_access("github_token_list.txt", true)
            .unwrap_err()
            .contains("[policy_deny]"));
        let ask = SecurityPolicy {
            secret_path_mode: SecretPathMode::Ask,
            ..deny
        };
        assert!(ask
            .validate_secret_path_access("github_token_list.txt", false)
            .unwrap_err()
            .contains("[needs_approval]"));
        assert!(ask
            .validate_secret_path_access("github_token_list.txt", true)
            .is_ok());
        assert!(ask.validate_secret_path_access("README.md", false).is_ok());
    }

    #[test]
    fn dsml_carrier_in_shell_is_malformed_not_once() {
        let p = SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            allowed_commands: vec!["cat".into()],
            secret_path_mode: SecretPathMode::Ask,
            ..SecurityPolicy::default()
        };
        let tag = crate::util::DSML_TAG;
        let cmd = format!("<{tag}tool_call> cat github_token_list.txt");
        let err = p.validate_command_execution(&cmd, false).unwrap_err();
        assert!(err.contains("malformed invocation"), "{err}");
        assert!(!err.contains("[needs_approval]"), "{err}");
        assert!(admit_tool_invocation("shell", &serde_json::json!({"command": cmd})).is_err());
    }

    #[test]
    fn compound_whoami_is_allowlist_not_once() {
        let p = SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            allowed_commands: vec!["echo".into()],
            secret_path_mode: SecretPathMode::Ask,
            ..SecurityPolicy::default()
        };
        let err = p
            .validate_command_execution("set -e; whoami; true", false)
            .unwrap_err();
        assert!(err.contains("not in allowed_commands"), "{err}");
        assert!(!err.contains("[needs_approval]"), "{err}");
    }

    #[test]
    fn file_write_content_secret_basename_does_not_ask() {
        let ask = SecurityPolicy {
            secret_path_mode: SecretPathMode::Ask,
            ..SecurityPolicy::default()
        };
        assert!(admit_tool_invocation(
            "file_write",
            &serde_json::json!({
                "path": "notes.md",
                "content": "do not read github_token_list.txt"
            })
        )
        .is_ok());
        assert!(ask.validate_secret_path_access("notes.md", false).is_ok());
        let blob = "#!/usr/bin/env python3\nprint('github_token_list.txt')\n".repeat(20);
        assert!(admit_tool_invocation(
            "file_write",
            &serde_json::json!({"path": blob, "content": "x"})
        )
        .is_err());
    }

    #[test]
    fn echo_without_secret_basename_is_not_secret_touching() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("ok.sh"), "echo hello\n").unwrap();
        assert!(!command_touches_secret_material_in(
            "bash ok.sh",
            Some(tmp.path())
        ));
    }

    #[test]
    fn local_profile_allows_secret_paths_and_drops_home_forbid() {
        let mut config = crate::config::Config::default();
        config.security.profile = Some(crate::config::SecurityProfile::Local);
        config.autonomy.allowed_commands = vec!["cat".into()];
        config.autonomy.workspace_only = true;
        config.autonomy.forbidden_paths = vec!["/home".into(), "/etc".into(), "~/.ssh".into()];
        let p = SecurityPolicy::from_workspace_config(&config).unwrap();
        assert!(p.inherit_process_env);
        assert!(!p.workspace_only);
        assert!(!p.forbidden_paths.iter().any(|x| x == "/home"));
        assert!(p.forbidden_paths.iter().any(|x| x == "/etc"));
        assert_eq!(p.secret_path_mode, SecretPathMode::Allow);
        assert!(p
            .validate_command_execution("cat /home/alex/github_token_list.txt", false)
            .is_ok());
    }

    #[test]
    fn escape_on_approval_forces_approval_for_apt_under_full() {
        let p = SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            allowed_commands: vec!["apt".into(), "sudo".into()],
            escape_on_approval: true,
            ..SecurityPolicy::default()
        };
        let denied = p.validate_command_execution("apt update", false);
        assert!(denied.is_err(), "apt must prompt when escape_on_approval");
        assert!(denied.unwrap_err().contains("escape_on_approval"));
        assert!(p.validate_command_execution("apt update", true).is_ok());
        assert!(p
            .validate_command_execution("sudo apt update", true)
            .is_ok());
        // Unrelated allowlisted low-risk still auto-runs under Full.
        let p2 = SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            allowed_commands: vec!["echo".into()],
            escape_on_approval: true,
            ..SecurityPolicy::default()
        };
        assert!(p2.validate_command_execution("echo hi", false).is_ok());
    }

    #[test]
    fn escape_on_approval_off_lets_full_run_apt_without_approval() {
        let p = SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            allowed_commands: vec!["apt".into()],
            escape_on_approval: false,
            ..SecurityPolicy::default()
        };
        assert!(p.validate_command_execution("apt update", false).is_ok());
    }

    #[test]
    fn validate_command_full_mode_skips_medium_risk_approval_gate() {
        let p = SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            require_approval_for_medium_risk: true,
            allowed_commands: vec!["touch".into()],
            ..SecurityPolicy::default()
        };

        let result = p.validate_command_execution("touch test.txt", false);
        assert_eq!(result.unwrap(), CommandRiskLevel::Medium);
    }

    #[test]
    fn validate_command_rejects_background_chain_bypass() {
        let p = default_policy();
        let result = p.validate_command_execution("ls & python3 -c 'print(1)'", false);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unsafe shell construct"));
    }

    // ── is_path_allowed ─────────────────────────────────────

    #[test]
    fn relative_paths_allowed() {
        let p = default_policy();
        assert!(p.is_path_allowed("file.txt"));
        assert!(p.is_path_allowed("src/main.rs"));
        assert!(p.is_path_allowed("deep/nested/dir/file.txt"));
    }

    #[test]
    fn path_traversal_blocked() {
        let p = default_policy();
        assert!(!p.is_path_allowed("../etc/passwd"));
        assert!(!p.is_path_allowed("../../root/.ssh/id_rsa"));
        assert!(!p.is_path_allowed("foo/../../../etc/shadow"));
        assert!(!p.is_path_allowed(".."));
    }

    #[test]
    fn absolute_paths_blocked_when_workspace_only() {
        let p = default_policy();
        assert!(!p.is_path_allowed("/etc/passwd"));
        assert!(!p.is_path_allowed("/root/.ssh/id_rsa"));
        assert!(
            p.is_path_allowed("/tmp/file.txt"),
            "host /tmp rewrites to workspace scratch"
        );
    }

    #[test]
    fn tmp_rewrites_to_scratch_under_workspace_only() {
        let p = default_policy();
        assert_eq!(
            p.rewrite_temp_tool_path("/tmp/notes.txt"),
            ".velaclaw/tmp/notes.txt"
        );
        assert!(p
            .tool_fs_path("/tmp/notes.txt")
            .starts_with(&p.workspace_dir));
    }

    #[test]
    fn graph_scratch_rewrites_tmp_under_this_run() {
        let p = SecurityPolicy {
            graph_scratch_rel: Some(".velaclaw/tmp/graphs/sess/run1".into()),
            ..SecurityPolicy::default()
        };
        assert_eq!(
            p.rewrite_temp_tool_path("/tmp/notes.txt"),
            ".velaclaw/tmp/graphs/sess/run1/notes.txt"
        );
        assert_eq!(
            p.rewrite_temp_tool_path(".velaclaw/tmp/graphs/sess/run1/keep.txt"),
            ".velaclaw/tmp/graphs/sess/run1/keep.txt"
        );
        assert_eq!(
            p.rewrite_temp_tool_path(".velaclaw/tmp/graphs/other/x.txt"),
            ".velaclaw/tmp/graphs/other/x.txt"
        );
    }

    #[test]
    fn absolute_paths_allowed_when_not_workspace_only() {
        let p = SecurityPolicy {
            workspace_only: false,
            forbidden_paths: vec![],
            ..SecurityPolicy::default()
        };
        assert!(p.is_path_allowed("/tmp/file.txt"));
    }

    #[test]
    fn forbidden_paths_blocked() {
        let p = SecurityPolicy {
            workspace_only: false,
            ..SecurityPolicy::default()
        };
        assert!(!p.is_path_allowed("/etc/passwd"));
        assert!(!p.is_path_allowed("/root/.bashrc"));
        assert!(!p.is_path_allowed("~/.ssh/id_rsa"));
        assert!(!p.is_path_allowed("~/.gnupg/pubring.kbx"));
    }

    #[test]
    fn empty_path_allowed() {
        let p = default_policy();
        assert!(p.is_path_allowed(""));
    }

    #[test]
    fn dotfile_in_workspace_allowed() {
        let p = default_policy();
        assert!(p.is_path_allowed(".gitignore"));
        assert!(p.is_path_allowed(".env"));
    }

    // ── from_config ─────────────────────────────────────────

    #[test]
    fn from_config_maps_all_fields() {
        let autonomy_config = crate::config::AutonomyConfig {
            level: AutonomyLevel::Full,
            workspace_only: false,
            allowed_commands: vec!["docker".into()],
            forbidden_paths: vec!["/secret".into()],
            max_actions_per_hour: 100,
            max_cost_per_day_cents: 1000,
            require_approval_for_medium_risk: false,
            block_high_risk_commands: false,
            ..crate::config::AutonomyConfig::default()
        };
        let workspace = PathBuf::from("/tmp/test-workspace");
        let policy = SecurityPolicy::from_config(&autonomy_config, &workspace);

        assert_eq!(policy.autonomy, AutonomyLevel::Full);
        assert!(!policy.workspace_only);
        assert!(policy.allowed_commands.contains(&"docker".to_string()));
        assert!(policy.allowed_commands.contains(&"curl".to_string()));
        assert_eq!(policy.forbidden_paths, vec!["/secret"]);
        assert_eq!(policy.max_actions_per_hour, 100);
        assert_eq!(policy.max_cost_per_day_cents, 1000);
        assert!(!policy.require_approval_for_medium_risk);
        assert!(!policy.block_high_risk_commands);
        assert_eq!(policy.workspace_dir, PathBuf::from("/tmp/test-workspace"));
    }

    // ── Default policy ──────────────────────────────────────

    #[test]
    fn default_policy_has_sane_values() {
        let p = SecurityPolicy::default();
        assert_eq!(p.autonomy, AutonomyLevel::Supervised);
        assert!(p.workspace_only);
        assert!(!p.allowed_commands.is_empty());
        assert!(!p.forbidden_paths.is_empty());
        assert!(p.max_actions_per_hour > 0);
        assert!(p.max_cost_per_day_cents > 0);
        assert!(p.require_approval_for_medium_risk);
        assert!(p.block_high_risk_commands);
    }

    // ── ActionTracker / rate limiting ───────────────────────

    #[test]
    fn action_tracker_starts_at_zero() {
        let tracker = ActionTracker::new();
        assert_eq!(tracker.count(), 0);
    }

    #[test]
    fn action_tracker_records_actions() {
        let tracker = ActionTracker::new();
        assert_eq!(tracker.record(), 1);
        assert_eq!(tracker.record(), 2);
        assert_eq!(tracker.record(), 3);
        assert_eq!(tracker.count(), 3);
    }

    #[test]
    fn record_action_allows_within_limit() {
        let p = SecurityPolicy {
            max_actions_per_hour: 5,
            ..SecurityPolicy::default()
        };
        for _ in 0..5 {
            assert!(p.record_action(), "should allow actions within limit");
        }
    }

    #[test]
    fn record_action_blocks_over_limit() {
        let p = SecurityPolicy {
            max_actions_per_hour: 3,
            ..SecurityPolicy::default()
        };
        assert!(p.record_action()); // 1
        assert!(p.record_action()); // 2
        assert!(p.record_action()); // 3
        assert!(!p.record_action()); // 4 — over limit
    }

    #[test]
    fn is_rate_limited_reflects_count() {
        let p = SecurityPolicy {
            max_actions_per_hour: 2,
            ..SecurityPolicy::default()
        };
        assert!(!p.is_rate_limited());
        p.record_action();
        assert!(!p.is_rate_limited());
        p.record_action();
        assert!(p.is_rate_limited());
    }

    #[test]
    fn action_tracker_clone_is_independent() {
        let tracker = ActionTracker::new();
        tracker.record();
        tracker.record();
        let cloned = tracker.clone();
        assert_eq!(cloned.count(), 2);
        tracker.record();
        assert_eq!(tracker.count(), 3);
        assert_eq!(cloned.count(), 2); // clone is independent
    }

    // ── Edge cases: command injection ────────────────────────

    #[test]
    fn command_injection_semicolon_blocked() {
        let p = default_policy();
        // First word is "ls;" (with semicolon) — doesn't match "ls" in allowlist.
        // This is a safe default: chained commands are blocked.
        assert!(!p.is_command_allowed("ls; rm -rf /"));
    }

    #[test]
    fn command_injection_semicolon_no_space() {
        let p = default_policy();
        assert!(!p.is_command_allowed("ls;rm -rf /"));
    }

    #[test]
    fn quoted_semicolons_do_not_split_sqlite_command() {
        let p = SecurityPolicy {
            allowed_commands: vec!["sqlite3".into()],
            ..SecurityPolicy::default()
        };
        assert!(p.is_command_allowed(
            "sqlite3 /tmp/test.db \"CREATE TABLE t(id INT); INSERT INTO t VALUES(1); SELECT * FROM t;\""
        ));
        assert_eq!(
            p.command_risk_level(
                "sqlite3 /tmp/test.db \"CREATE TABLE t(id INT); INSERT INTO t VALUES(1); SELECT * FROM t;\""
            ),
            CommandRiskLevel::Low
        );
    }

    #[test]
    fn unquoted_semicolon_after_quoted_sql_still_splits_commands() {
        let p = SecurityPolicy {
            allowed_commands: vec!["sqlite3".into()],
            ..SecurityPolicy::default()
        };
        assert!(!p.is_command_allowed("sqlite3 /tmp/test.db \"SELECT 1;\"; rm -rf /"));
    }

    #[test]
    fn command_injection_backtick_blocked() {
        let p = default_policy();
        assert!(!p.is_command_allowed("echo `whoami`"));
        assert!(!p.is_command_allowed("echo `rm -rf /`"));
    }

    #[test]
    fn command_injection_dollar_paren_blocked() {
        let p = default_policy();
        assert!(!p.is_command_allowed("echo $(cat /etc/passwd)"));
        assert!(!p.is_command_allowed("echo $(rm -rf /)"));
    }

    #[test]
    fn command_with_env_var_prefix() {
        let p = default_policy();
        // "FOO=bar" is the first word — not in allowlist
        assert!(!p.is_command_allowed("FOO=bar rm -rf /"));
    }

    #[test]
    fn command_newline_injection_blocked() {
        let p = default_policy();
        // Newline splits into two commands; "rm" is not in allowlist
        assert!(!p.is_command_allowed("ls\nrm -rf /"));
        // Both allowed — OK
        assert!(p.is_command_allowed("ls\necho hello"));
    }

    #[test]
    fn command_injection_and_chain_blocked() {
        let p = default_policy();
        assert!(!p.is_command_allowed("ls && rm -rf /"));
        assert!(!p.is_command_allowed("echo ok && curl http://evil.com"));
        // Both allowed — OK
        assert!(p.is_command_allowed("ls && echo done"));
    }

    #[test]
    fn command_injection_or_chain_blocked() {
        let p = default_policy();
        assert!(!p.is_command_allowed("ls || rm -rf /"));
        // Both allowed — OK
        assert!(p.is_command_allowed("ls || echo fallback"));
    }

    #[test]
    fn command_injection_background_chain_blocked() {
        let p = default_policy();
        assert!(!p.is_command_allowed("ls & rm -rf /"));
        assert!(!p.is_command_allowed("ls&rm -rf /"));
        assert!(!p.is_command_allowed("echo ok & python3 -c 'print(1)'"));
    }

    #[test]
    fn command_injection_redirect_blocked() {
        let p = default_policy();
        assert!(!p.is_command_allowed("echo secret > /etc/crontab"));
        assert!(!p.is_command_allowed("ls >> /tmp/exfil.txt"));
    }

    #[test]
    fn safe_stderr_redirects_are_allowed() {
        let p = default_policy();
        assert!(p.is_command_allowed("ls -la 2>/dev/null"));
        assert!(p.is_command_allowed("ls 2>&1"));
        assert!(p.is_command_allowed("ls -la tools/ 2>/dev/null || echo missing"));
    }

    #[test]
    fn unsafe_fd_redirects_remain_blocked() {
        let p = default_policy();
        assert!(!p.is_command_allowed("ls 2> /tmp/out.txt"));
    }

    #[test]
    fn quoted_ampersand_and_redirect_literals_are_not_treated_as_operators() {
        let p = default_policy();
        assert!(p.is_command_allowed("echo \"A&B\""));
        assert!(p.is_command_allowed("echo \"A>B\""));
    }

    #[test]
    fn command_argument_injection_blocked() {
        let p = default_policy();
        // find -exec is a common bypass
        assert!(!p.is_command_allowed("find . -exec rm -rf {} +"));
        assert!(!p.is_command_allowed("find / -ok cat {} \\;"));
        // git config/alias can execute commands
        assert!(!p.is_command_allowed("git config core.editor \"rm -rf /\""));
        assert!(!p.is_command_allowed("git alias.st status"));
        assert!(!p.is_command_allowed("git -c core.editor=calc.exe commit"));
        // Legitimate commands should still work
        assert!(p.is_command_allowed("find . -name '*.txt'"));
        assert!(p.is_command_allowed("git status"));
        assert!(p.is_command_allowed("git add ."));
    }

    #[test]
    fn command_injection_dollar_brace_blocked() {
        let p = default_policy();
        assert!(!p.is_command_allowed("echo ${IFS}cat${IFS}/etc/passwd"));
    }

    #[test]
    fn command_injection_tee_blocked() {
        let p = default_policy();
        assert!(!p.is_command_allowed("echo secret | tee /etc/crontab"));
        assert!(!p.is_command_allowed("ls | /usr/bin/tee outfile"));
        assert!(!p.is_command_allowed("tee file.txt"));
    }

    #[test]
    fn command_injection_process_substitution_blocked() {
        let p = default_policy();
        assert!(!p.is_command_allowed("cat <(echo pwned)"));
        assert!(!p.is_command_allowed("ls >(cat /etc/passwd)"));
    }

    #[test]
    fn command_env_var_prefix_with_allowed_cmd() {
        let p = default_policy();
        // env assignment + allowed command — OK
        assert!(p.is_command_allowed("FOO=bar ls"));
        assert!(p.is_command_allowed("LANG=C grep pattern file"));
        // env assignment + disallowed command — blocked
        assert!(!p.is_command_allowed("FOO=bar rm -rf /"));
    }

    // ── Edge cases: path traversal ──────────────────────────

    #[test]
    fn path_traversal_encoded_dots() {
        let p = default_policy();
        // Literal ".." in path — always blocked
        assert!(!p.is_path_allowed("foo/..%2f..%2fetc/passwd"));
    }

    #[test]
    fn path_traversal_double_dot_in_filename() {
        let p = default_policy();
        // ".." in a filename (not a path component) is allowed
        assert!(p.is_path_allowed("my..file.txt"));
        // But actual traversal components are still blocked
        assert!(!p.is_path_allowed("../etc/passwd"));
        assert!(!p.is_path_allowed("foo/../etc/passwd"));
    }

    #[test]
    fn path_with_null_byte_blocked() {
        let p = default_policy();
        assert!(!p.is_path_allowed("file\0.txt"));
    }

    #[test]
    fn path_symlink_style_absolute() {
        let p = default_policy();
        assert!(!p.is_path_allowed("/proc/self/root/etc/passwd"));
    }

    #[test]
    fn path_home_tilde_ssh() {
        let p = SecurityPolicy {
            workspace_only: false,
            ..SecurityPolicy::default()
        };
        assert!(!p.is_path_allowed("~/.ssh/id_rsa"));
        assert!(!p.is_path_allowed("~/.gnupg/secring.gpg"));
    }

    #[test]
    fn path_var_run_blocked() {
        let p = SecurityPolicy {
            workspace_only: false,
            ..SecurityPolicy::default()
        };
        assert!(!p.is_path_allowed("/var/run/docker.sock"));
    }

    // ── Edge cases: rate limiter boundary ────────────────────

    #[test]
    fn rate_limit_exactly_at_boundary() {
        let p = SecurityPolicy {
            max_actions_per_hour: 1,
            ..SecurityPolicy::default()
        };
        assert!(p.record_action()); // 1 — exactly at limit
        assert!(!p.record_action()); // 2 — over
        assert!(!p.record_action()); // 3 — still over
    }

    #[test]
    fn rate_limit_zero_blocks_everything() {
        let p = SecurityPolicy {
            max_actions_per_hour: 0,
            ..SecurityPolicy::default()
        };
        assert!(!p.record_action());
    }

    #[test]
    fn rate_limit_high_allows_many() {
        let p = SecurityPolicy {
            max_actions_per_hour: 10000,
            ..SecurityPolicy::default()
        };
        for _ in 0..100 {
            assert!(p.record_action());
        }
    }

    // ── Edge cases: autonomy + command combos ────────────────

    #[test]
    fn readonly_blocks_even_safe_commands() {
        let p = SecurityPolicy {
            autonomy: AutonomyLevel::ReadOnly,
            allowed_commands: vec!["ls".into(), "cat".into()],
            ..SecurityPolicy::default()
        };
        assert!(!p.is_command_allowed("ls"));
        assert!(!p.is_command_allowed("cat"));
        assert!(!p.can_act());
    }

    #[test]
    fn supervised_allows_listed_commands() {
        let p = SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            allowed_commands: vec!["git".into()],
            ..SecurityPolicy::default()
        };
        assert!(p.is_command_allowed("git status"));
        assert!(!p.is_command_allowed("docker ps"));
    }

    #[test]
    fn full_autonomy_still_respects_forbidden_paths() {
        let p = SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            workspace_only: false,
            ..SecurityPolicy::default()
        };
        assert!(!p.is_path_allowed("/etc/shadow"));
        assert!(!p.is_path_allowed("/root/.bashrc"));
    }

    // ── Edge cases: from_config preserves tracker ────────────

    #[test]
    fn from_config_creates_fresh_tracker() {
        let autonomy_config = crate::config::AutonomyConfig {
            level: AutonomyLevel::Full,
            workspace_only: false,
            allowed_commands: vec![],
            forbidden_paths: vec![],
            max_actions_per_hour: 10,
            max_cost_per_day_cents: 100,
            require_approval_for_medium_risk: true,
            block_high_risk_commands: true,
            ..crate::config::AutonomyConfig::default()
        };
        let workspace = PathBuf::from("/tmp/test");
        let policy = SecurityPolicy::from_config(&autonomy_config, &workspace);
        assert_eq!(policy.tracker.count(), 0);
        assert!(!policy.is_rate_limited());
    }

    // ══════════════════════════════════════════════════════════
    // SECURITY CHECKLIST TESTS
    // Checklist: gateway not public, pairing required,
    //            filesystem scoped (no /), access via tunnel
    // ══════════════════════════════════════════════════════════

    // ── Checklist #3: Filesystem scoped (no /) ──────────────

    #[test]
    fn checklist_root_path_blocked() {
        let p = default_policy();
        if cfg!(windows) {
            assert!(!p.is_path_allowed(r"C:\"));
            assert!(!p.is_path_allowed(r"C:\anything"));
        } else {
            assert!(!p.is_path_allowed("/"));
            assert!(!p.is_path_allowed("/anything"));
        }
    }

    #[test]
    fn checklist_all_system_dirs_blocked() {
        let p = SecurityPolicy {
            workspace_only: false,
            ..SecurityPolicy::default()
        };
        for dir in [
            "/etc", "/root", "/home", "/usr", "/bin", "/sbin", "/lib", "/opt", "/boot", "/dev",
            "/proc", "/sys", "/var",
        ] {
            assert!(
                !p.is_path_allowed(dir),
                "System dir should be blocked: {dir}"
            );
            assert!(
                !p.is_path_allowed(&format!("{dir}/subpath")),
                "Subpath of system dir should be blocked: {dir}/subpath"
            );
        }
        assert!(
            p.is_path_allowed("/tmp/scratch.txt"),
            "/tmp is scratch, not a forbidden system dir"
        );
    }

    #[test]
    fn checklist_sensitive_dotfiles_blocked() {
        let p = SecurityPolicy {
            workspace_only: false,
            ..SecurityPolicy::default()
        };
        for path in [
            "~/.ssh/id_rsa",
            "~/.gnupg/secring.gpg",
            "~/.aws/credentials",
            "~/.config/secrets",
        ] {
            assert!(
                !p.is_path_allowed(path),
                "Sensitive dotfile should be blocked: {path}"
            );
        }
    }

    #[test]
    fn checklist_null_byte_injection_blocked() {
        let p = default_policy();
        assert!(!p.is_path_allowed("safe\0/../../../etc/passwd"));
        assert!(!p.is_path_allowed("\0"));
        assert!(!p.is_path_allowed("file\0"));
    }

    #[test]
    fn checklist_workspace_only_blocks_all_absolute() {
        let p = SecurityPolicy {
            workspace_only: true,
            ..SecurityPolicy::default()
        };
        let abs = if cfg!(windows) {
            r"C:\any\absolute\path"
        } else {
            "/any/absolute/path"
        };
        assert!(!p.is_path_allowed(abs));
        assert!(p.is_path_allowed("relative/path.txt"));
    }

    #[test]
    fn checklist_resolved_path_must_be_in_workspace() {
        let p = SecurityPolicy {
            workspace_dir: PathBuf::from("/home/user/project"),
            ..SecurityPolicy::default()
        };
        // Inside workspace — allowed
        assert!(p.is_resolved_path_allowed(Path::new("/home/user/project/src/main.rs")));
        // Outside workspace — blocked (symlink escape)
        assert!(!p.is_resolved_path_allowed(Path::new("/etc/passwd")));
        assert!(!p.is_resolved_path_allowed(Path::new("/home/user/other_project/file")));
        // Root — blocked
        assert!(!p.is_resolved_path_allowed(Path::new("/")));
    }

    #[test]
    fn checklist_default_policy_is_workspace_only() {
        let p = SecurityPolicy::default();
        assert!(
            p.workspace_only,
            "Default policy must be workspace_only=true"
        );
    }

    #[test]
    fn checklist_default_forbidden_paths_comprehensive() {
        let p = SecurityPolicy::default();
        // Must contain all critical system dirs
        for dir in ["/etc", "/root", "/proc", "/sys", "/dev", "/var", "/tmp"] {
            assert!(
                p.forbidden_paths.iter().any(|f| f == dir),
                "Default forbidden_paths must include {dir}"
            );
        }
        // Must contain sensitive dotfiles
        for dot in ["~/.ssh", "~/.gnupg", "~/.aws"] {
            assert!(
                p.forbidden_paths.iter().any(|f| f == dot),
                "Default forbidden_paths must include {dot}"
            );
        }
    }

    // ── §1.2 Path resolution / symlink bypass tests ──────────

    #[test]
    fn resolved_path_blocks_outside_workspace() {
        let workspace = std::env::temp_dir().join("velaclaw_test_resolved_path");
        let _ = std::fs::create_dir_all(&workspace);

        // Use the canonicalized workspace so starts_with checks match
        let canonical_workspace = workspace
            .canonicalize()
            .unwrap_or_else(|_| workspace.clone());

        let policy = SecurityPolicy {
            workspace_dir: canonical_workspace.clone(),
            ..SecurityPolicy::default()
        };

        // A resolved path inside the workspace should be allowed
        let inside = canonical_workspace.join("subdir").join("file.txt");
        assert!(
            policy.is_resolved_path_allowed(&inside),
            "path inside workspace should be allowed"
        );

        // A resolved path outside the workspace should be blocked
        let canonical_temp = std::env::temp_dir()
            .canonicalize()
            .unwrap_or_else(|_| std::env::temp_dir());
        let outside = canonical_temp.join("outside_workspace_velaclaw");
        assert!(
            !policy.is_resolved_path_allowed(&outside),
            "path outside workspace must be blocked"
        );

        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[test]
    fn resolved_path_blocks_root_escape() {
        let policy = SecurityPolicy {
            workspace_dir: PathBuf::from("/home/velaclaw_user/project"),
            ..SecurityPolicy::default()
        };

        assert!(
            !policy.is_resolved_path_allowed(Path::new("/etc/passwd")),
            "resolved path to /etc/passwd must be blocked"
        );
        assert!(
            !policy.is_resolved_path_allowed(Path::new("/root/.bashrc")),
            "resolved path to /root/.bashrc must be blocked"
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolved_path_blocks_symlink_escape() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join("velaclaw_test_symlink_escape");
        let workspace = root.join("workspace");
        let outside = root.join("outside_target");

        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        // Create a symlink inside workspace pointing outside
        let link_path = workspace.join("escape_link");
        symlink(&outside, &link_path).unwrap();

        let policy = SecurityPolicy {
            workspace_dir: workspace.clone(),
            ..SecurityPolicy::default()
        };

        // The resolved symlink target should be outside workspace
        let resolved = link_path.canonicalize().unwrap();
        assert!(
            !policy.is_resolved_path_allowed(&resolved),
            "symlink-resolved path outside workspace must be blocked"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn is_path_allowed_blocks_null_bytes() {
        let policy = default_policy();
        assert!(
            !policy.is_path_allowed("file\0.txt"),
            "paths with null bytes must be blocked"
        );
    }

    #[test]
    fn is_path_allowed_blocks_url_encoded_traversal() {
        let policy = default_policy();
        assert!(
            !policy.is_path_allowed("..%2fetc%2fpasswd"),
            "URL-encoded path traversal must be blocked"
        );
        assert!(
            !policy.is_path_allowed("subdir%2f..%2f..%2fetc"),
            "URL-encoded parent dir traversal must be blocked"
        );
    }
}
