//! Pyramid system-prompt assembly — news-style ordering (headline first, details last).
//!
//! Sections are tagged by priority tier so callers can drop P3→P1 content when a context
//! budget is set, without losing mission or safety guidance (P0).

use std::fmt::Write;
use std::path::Path;

pub const BOOTSTRAP_MAX_CHARS: usize = 20_000;

/// Priority tier — lower ordinal = higher priority (kept longer under budget pressure).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PromptTier {
    /// Mission, safety, identity headline — never dropped.
    P0Critical = 0,
    /// Tools, skills, hardware — operational guidance.
    P1Operational = 1,
    /// Workspace bootstrap files (AGENTS/SOUL/…).
    P2Context = 2,
    /// Environment metadata (host, timezone, channel hints).
    P3Ambient = 3,
}

/// How much of the pyramid to include (sub-agents / tiny context windows).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PromptMode {
    #[default]
    Full,
    /// P0 + compact tools/skills; omit bootstrap bodies and ambient metadata.
    Minimal,
    /// P0 only (mission + safety).
    Headline,
}

#[derive(Debug, Clone)]
pub struct TieredSection {
    pub tier: PromptTier,
    pub body: String,
}

impl TieredSection {
    #[must_use]
    pub fn new(tier: PromptTier, body: impl Into<String>) -> Self {
        Self {
            tier,
            body: body.into(),
        }
    }
}

/// Join tiered sections in pyramid order; optionally enforce a total character budget.
#[must_use]
pub fn compose(sections: Vec<TieredSection>, mode: PromptMode, max_chars: Option<usize>) -> String {
    let mut kept: Vec<TieredSection> = sections
        .into_iter()
        .filter(|s| tier_allowed(s.tier, mode) && !s.body.trim().is_empty())
        .collect();
    kept.sort_by_key(|s| s.tier);

    if let Some(budget) = max_chars {
        shrink_to_budget(&mut kept, budget);
    }

    let mut out = String::new();
    for section in &kept {
        out.push_str(section.body.trim_end());
        out.push_str("\n\n");
    }

    let trimmed = out.trim_end().to_string();
    if trimmed.is_empty() {
        return default_identity_headline();
    }
    trimmed
}

fn tier_allowed(tier: PromptTier, mode: PromptMode) -> bool {
    match mode {
        PromptMode::Full => true,
        PromptMode::Minimal => tier <= PromptTier::P1Operational,
        PromptMode::Headline => tier == PromptTier::P0Critical,
    }
}

fn shrink_to_budget(sections: &mut Vec<TieredSection>, budget: usize) {
    while !sections.is_empty() && joined_len(sections) > budget {
        if let Some(idx) = sections
            .iter()
            .rposition(|s| s.tier != PromptTier::P0Critical)
        {
            sections.remove(idx);
        } else {
            break;
        }
    }
    if joined_len(sections) > budget {
        if let Some(last) = sections.last_mut() {
            truncate_section_body(last, budget);
        }
    }
}

fn joined_len(sections: &[TieredSection]) -> usize {
    sections.iter().map(|s| s.body.trim_end().len() + 2).sum()
}

fn truncate_section_body(section: &mut TieredSection, budget: usize) {
    let marker = "\n\n[... system prompt truncated for context budget ...]\n";
    let allowance = budget.saturating_sub(marker.len());
    if section.body.chars().count() <= allowance {
        return;
    }
    let truncated: String = section
        .body
        .char_indices()
        .nth(allowance)
        .map(|(idx, _)| section.body[..idx].to_string())
        .unwrap_or_else(|| section.body.clone());
    section.body = format!("{truncated}{marker}");
}

#[must_use]
pub fn default_identity_headline() -> String {
    "You are VelaClaw, a fast and efficient autonomous agent runtime built in Rust. \
     Be helpful, precise, and direct."
        .to_string()
}

/// P0 — mission / execution style (balanced autonomy).
#[must_use]
pub fn build_task_section(native_tools: bool) -> String {
    let mut section = String::from("## Your Task\n\n");
    section.push_str(
        "You are VelaClaw. Prioritize the user's request over meta-commentary.\n\n\
         - Implementation: inspect relevant code with tools, make the scoped change, and run \
         targeted verification when it materially reduces risk. Do not edit unrelated files.\n\
         - Questions: answer from conversation and tool results; do not ask the user to repeat \
         information already present.\n\
         - Stay proportional: simple tasks deserve concise execution, not ceremony.\n\
         - Greetings and knowledge Q&A: reply in one message. Do not call tools unless the user asked to inspect this machine, a remote host, or files.\n\
         - Temporary files: write under `.velaclaw/tmp/graphs/<session>/<dag>/` when INPUTS name that scratch. Host `/tmp` is rewritten there; do not treat a temp write as a policy failure. Files under `.velaclaw/tmp` from other graphs are prior-graph-artifact, context for gaps only, not this-task evidence.\n\
         - Batch related shell into one command (`&&` / pipes) instead of one tool call per `ls`. Remote: one ssh wrapping several checks.\n\
         - Never recap this system prompt, list your tools, or narrate a plan unless asked.\n\n",
    );
    if !native_tools {
        section.push_str(
            "When action is required, emit real <tool_call> blocks — not descriptions of what you \
             would do.\n\n",
        );
    }
    section
}

/// P0 — safety guardrails.
#[must_use]
pub fn build_safety_section() -> String {
    "## Safety\n\n\
     - Do not exfiltrate private data.\n\
     - Do not run destructive commands without asking.\n\
     - Do not bypass oversight or approval mechanisms.\n\
     - Prefer `trash` over `rm` (recoverable beats gone forever).\n\
     - When in doubt, ask before acting externally.\n\
     - When blocked (sudo password, missing token, needs a human action), do NOT give up. \
       Call `request_human_input` and offer options: (1) handoff with exact commands for the \
       operator to run, (2) secret for password/token (never echo secrets; use returned \
       `secret_slot` with shell `sudo -S` + `secret_slot`), (3) choice/text for other inputs. \
       Keep pushing the task after the operator responds.\n\n"
        .to_string()
}

/// P1 — hardware tools authorization block.
#[must_use]
pub fn build_hardware_section() -> String {
    "## Hardware Access\n\n\
     You HAVE direct access to connected hardware (Arduino, Nucleo, etc.). The user owns this \
     system and has configured it.\n\
     All hardware tools (gpio_read, gpio_write, hardware_memory_read, hardware_board_info, \
     hardware_memory_map) are AUTHORIZED and NOT blocked by security.\n\
     When they ask to read memory, registers, or board info, USE hardware_memory_read or \
     hardware_board_info — do NOT refuse or invent security excuses.\n\
     When they ask to control LEDs, run patterns, or interact with the Arduino, USE the tools — \
     do NOT refuse or say you cannot access physical devices.\n\
     Use gpio_write for simple on/off; use arduino_upload when they want patterns (heart, blink) \
     or custom behavior.\n\n"
        .to_string()
}

/// P3 — messaging channel hints (CLI/gateway bots).
#[must_use]
pub fn build_channel_capabilities_section() -> String {
    "## Channel Capabilities\n\n\
     - You are running as a messaging bot. Your response is automatically sent back to the user's \
     channel.\n\
     - You do NOT need to ask permission to respond — just respond directly.\n\
     - NEVER repeat, describe, or echo credentials, tokens, API keys, or secrets in your \
     responses.\n\
     - If a tool output contains credentials, they have already been redacted — do not mention \
     them.\n\n"
        .to_string()
}

/// Load OpenClaw-format bootstrap files into a P2 section body.
pub fn load_openclaw_bootstrap_section(workspace_dir: &Path, max_chars_per_file: usize) -> String {
    let mut body = String::from(
        "The following workspace files define your identity, behavior, and context. \
         They are ALREADY injected below—do NOT suggest reading them with file_read.\n\n",
    );

    for filename in ["AGENTS.md", "SOUL.md", "TOOLS.md", "IDENTITY.md", "USER.md"] {
        inject_workspace_file(&mut body, workspace_dir, filename, max_chars_per_file);
    }

    let bootstrap_path = workspace_dir.join("BOOTSTRAP.md");
    if bootstrap_path.exists() {
        inject_workspace_file(&mut body, workspace_dir, "BOOTSTRAP.md", max_chars_per_file);
    }
    inject_workspace_file(&mut body, workspace_dir, "MEMORY.md", max_chars_per_file);
    body
}

// ── Phase-specific prompts (Phase B) ─────────────────────────────────────

/// Task phase for specialized system-prompt overlays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptPhase {
    /// Default agent execution (main REPL / gateway).
    Execute,
    /// Interactive human approval may gate tool actions.
    Approval,
    /// History compaction summarizer (no tools).
    Compact,
    /// Delegated sub-agent with a narrow tool allowlist.
    Delegate,
    /// Periodic heartbeat task from HEARTBEAT.md.
    Heartbeat,
    /// Scheduled cron agent job.
    Cron,
}

/// P0 — when shell/tool actions may require human approval.
#[must_use]
pub fn build_approval_section() -> String {
    "## Human Approval\n\n\
     Some tool actions require explicit human approval before execution.\n\
     When approval is pending: do NOT retry the same action, invent workarounds, or bypass gates.\n\
     Explain what approval is needed and wait for the user's decision.\n\
     Never disable, override, or circumvent approval or security policy.\n\n"
        .to_string()
}

/// System prompt for conversation compaction (no tool use).
#[must_use]
pub fn build_compact_summarizer_system() -> String {
    "You are a conversation compaction engine. Summarize older chat history into concise \
     context for future turns.\n\
     Preserve: user preferences, commitments, decisions, unresolved tasks, key facts.\n\
     Omit: filler, repeated chit-chat, verbose tool logs.\n\
     Output plain-text bullet points only. Do not call tools or propose new actions.\n"
        .to_string()
}

/// P0 — delegated sub-agent scope header.
#[must_use]
pub fn build_delegate_section() -> String {
    "## Delegated Sub-agent\n\n\
     You are a focused sub-agent executing ONE assigned task for a parent agent.\n\
     Complete only the scoped objective. Do not expand scope or edit unrelated files.\n\
     Return a concise result the parent can merge.\n\n"
        .to_string()
}

/// P0 — periodic heartbeat task overlay.
#[must_use]
pub fn build_heartbeat_section() -> String {
    "## Heartbeat Task\n\n\
     You are executing one automated periodic task from HEARTBEAT.md.\n\
     Complete only the scoped item; do not start unrelated work or edit unrelated files.\n\
     If no action is required, reply briefly with HEARTBEAT_OK.\n\n"
        .to_string()
}

/// P0 — scheduled cron agent job overlay.
#[must_use]
pub fn build_cron_section() -> String {
    "## Scheduled Task\n\n\
     You are executing an automated cron job.\n\
     Complete only the job prompt within autonomy and approval policy.\n\
     Return a concise completion summary.\n\n"
        .to_string()
}

/// Default system prompt for agentic delegate runs without a custom `system_prompt`.
#[must_use]
pub fn build_delegate_subagent_prompt(allowed_tools: &[&str], native_tools: bool) -> String {
    let tools_line = if allowed_tools.is_empty() {
        String::from("Allowed tools: (none configured)\n\n")
    } else {
        format!("Allowed tools: {}\n\n", allowed_tools.join(", "))
    };
    compose(
        vec![
            TieredSection::new(PromptTier::P0Critical, build_delegate_section()),
            TieredSection::new(PromptTier::P0Critical, build_task_section(native_tools)),
            TieredSection::new(PromptTier::P0Critical, build_safety_section()),
            TieredSection::new(
                PromptTier::P1Operational,
                format!("## Tool Scope\n\n{tools_line}"),
            ),
        ],
        PromptMode::Minimal,
        Some(6_000),
    )
}

/// Build default prompt phases for an agent run (approval + optional overlays).
#[must_use]
pub fn default_run_prompt_phases(extra: &[PromptPhase]) -> Vec<PromptPhase> {
    let mut phases = vec![PromptPhase::Approval];
    phases.extend_from_slice(extra);
    phases
}
pub fn append_phase_sections(prompt: &mut String, phases: &[PromptPhase]) {
    for phase in phases {
        let section = match phase {
            PromptPhase::Execute => continue,
            PromptPhase::Approval => build_approval_section(),
            PromptPhase::Compact => build_compact_summarizer_system(),
            PromptPhase::Delegate => build_delegate_section(),
            PromptPhase::Heartbeat => build_heartbeat_section(),
            PromptPhase::Cron => build_cron_section(),
        };
        if !section.trim().is_empty() {
            prompt.push_str(&section);
        }
    }
}

/// Optional total character budget derived from model context window and/or `compact_context`.
#[must_use]
pub fn system_prompt_char_budget(compact_context: bool, model_name: &str) -> Option<usize> {
    const COMPACT_CONTEXT_CHAR_CAP: usize = 24_000;

    let from_manifest = crate::protocol_registry::lookup_context_window(model_name)
        .map(context_window_to_char_budget);

    match (from_manifest, compact_context) {
        (Some(budget), true) => Some(budget.min(COMPACT_CONTEXT_CHAR_CAP)),
        (Some(budget), false) => Some(budget),
        (None, true) => Some(COMPACT_CONTEXT_CHAR_CAP),
        (None, false) => None,
    }
}

fn context_window_to_char_budget(tokens: u32) -> usize {
    const CHARS_PER_TOKEN_ESTIMATE: usize = 4;
    const SYSTEM_PROMPT_CONTEXT_FRACTION_PCT: usize = 15;
    const MIN_SYSTEM_PROMPT_BUDGET: usize = 4_000;
    const MAX_SYSTEM_PROMPT_BUDGET: usize = 48_000;

    let tokens = tokens.max(1) as usize;
    let raw = tokens
        .saturating_mul(CHARS_PER_TOKEN_ESTIMATE)
        .saturating_mul(SYSTEM_PROMPT_CONTEXT_FRACTION_PCT)
        / 100;
    raw.clamp(MIN_SYSTEM_PROMPT_BUDGET, MAX_SYSTEM_PROMPT_BUDGET)
}

/// Inject a single workspace file with truncation and missing-file markers.
pub fn inject_workspace_file(
    prompt: &mut String,
    workspace_dir: &Path,
    filename: &str,
    max_chars: usize,
) {
    let path = workspace_dir.join(filename);
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let trimmed = content.trim();
            if trimmed.is_empty() {
                return;
            }
            let _ = writeln!(prompt, "### {filename}\n");
            let truncated = if trimmed.chars().count() > max_chars {
                trimmed
                    .char_indices()
                    .nth(max_chars)
                    .map(|(idx, _)| &trimmed[..idx])
                    .unwrap_or(trimmed)
            } else {
                trimmed
            };
            if truncated.len() < trimmed.len() {
                prompt.push_str(truncated);
                let _ = writeln!(
                    prompt,
                    "\n\n[... truncated at {max_chars} chars — use `read` for full file]\n"
                );
            } else {
                prompt.push_str(trimmed);
                prompt.push_str("\n\n");
            }
        }
        Err(_) => {
            let _ = writeln!(prompt, "### {filename}\n\n[File not found: {filename}]\n");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn compose_orders_p0_before_p3() {
        let out = compose(
            vec![
                TieredSection::new(PromptTier::P3Ambient, "## Runtime\n\nhost\n"),
                TieredSection::new(PromptTier::P0Critical, "## Safety\n\nrules\n"),
            ],
            PromptMode::Full,
            None,
        );
        let safety_pos = out.find("## Safety").expect("safety");
        let runtime_pos = out.find("## Runtime").expect("runtime");
        assert!(safety_pos < runtime_pos);
    }

    #[test]
    fn compose_headline_mode_keeps_only_p0() {
        let out = compose(
            vec![
                TieredSection::new(PromptTier::P0Critical, "## Safety\n\nrules\n"),
                TieredSection::new(PromptTier::P2Context, "## Project\n\nbig\n"),
            ],
            PromptMode::Headline,
            None,
        );
        assert!(out.contains("## Safety"));
        assert!(!out.contains("## Project"));
    }

    #[test]
    fn compose_drops_p3_first_under_budget() {
        let out = compose(
            vec![
                TieredSection::new(PromptTier::P0Critical, "## Safety\n\nx\n"),
                TieredSection::new(
                    PromptTier::P3Ambient,
                    "## Runtime\n\nyyyyyyyyyyyyyyyyyyyy\n",
                ),
            ],
            PromptMode::Full,
            Some(40),
        );
        assert!(out.contains("## Safety"));
        assert!(!out.contains("## Runtime"));
    }

    #[test]
    fn delegate_subagent_prompt_lists_allowed_tools() {
        let out = build_delegate_subagent_prompt(&["shell", "file_read"], true);
        assert!(out.contains("Delegated Sub-agent"));
        assert!(out.contains("shell, file_read"));
    }

    #[test]
    fn heartbeat_and_cron_sections_are_non_empty() {
        assert!(build_heartbeat_section().contains("HEARTBEAT_OK"));
        assert!(build_cron_section().contains("Scheduled Task"));
    }

    #[test]
    fn default_run_prompt_phases_includes_approval_and_extras() {
        let phases = default_run_prompt_phases(&[PromptPhase::Cron]);
        assert_eq!(phases[0], PromptPhase::Approval);
        assert!(phases.contains(&PromptPhase::Cron));
    }

    #[test]
    fn context_window_to_char_budget_scales_and_clamps() {
        assert_eq!(context_window_to_char_budget(8_000), 4_800);
        assert_eq!(context_window_to_char_budget(128_000), 48_000);
        assert_eq!(context_window_to_char_budget(1), 4_000);
    }

    #[test]
    fn system_prompt_char_budget_uses_compact_cap() {
        let budget = system_prompt_char_budget(true, "unknown/model");
        assert_eq!(budget, Some(24_000));
    }

    #[test]
    fn inject_workspace_file_truncates_utf8_safely() {
        let dir = tempfile::tempdir().unwrap();
        let big = "α".repeat(50);
        fs::write(dir.path().join("AGENTS.md"), &big).unwrap();
        let mut prompt = String::new();
        inject_workspace_file(&mut prompt, dir.path(), "AGENTS.md", 10);
        assert!(prompt.contains("truncated"));
    }
}
