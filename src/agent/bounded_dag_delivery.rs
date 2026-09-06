//! Host parlor (VL-NA-031): internodal HANDOFF never becomes the user-visible reply.
//!
//! 会客厅：节点信封只进 artifact；用户出口由宿主整理或模板降级。

use crate::providers::{ChatMessage, ChatRequest, Provider};
use anyhow::Result;

/// System prompt for the host Delivery rewrite (not a planner node, not a work-node card).
pub const DELIVERY_SYSTEM_PROMPT: &str = "\
You write the operator-visible conclusion for USER TASK.\n\
Use the node artifacts as evidence. Be direct.\n\
Do not use internodal envelope headers: HANDOFF, verdict:, findings:, pointers:, gaps:.\n\
Do not tell the operator to hand off to another node.\n\
Every claim has a vantage (where evidence was gathered) and coverage (sample|partial|exhaustive).\n\
Exclusive wording (only/none/all of a population) is allowed only when coverage is exhaustive.\n\
Otherwise say what this vantage saw, not what the unseen rest of the world is.\n\
If a later artifact expands vantage, revise earlier exclusive claims instead of leaving both.\n\
Name evidence_layer on each claim: this-hop-tool | this-graph-artifact | prior-graph-artifact | host-config | protocol-dist | upstream-live | inference.\n\
Recommend changes only to a layer you observed. Do not treat leftover workspace tmp from other graphs as this-task evidence.\n\
Do not report live host/service health from prior-graph-artifact or other-session memory; put those in the gap, not the conclusion.\n\
Cached hop_fail blocks are prior errors, not this-hop upstream-live.\n\
Do not invent geography, identity, or type labels that are not in the artifacts; mark guesses as inference.\n\
If evidence is incomplete, say what is known and the single next action.\n\
When PRIOR OPERATOR-VISIBLE CLAIMS are provided, later evidence supersedes earlier exclusivity.\n";

/// True when `text` is a work-node internodal envelope, not a parlor reply.
#[must_use]
pub fn looks_like_internodal_envelope(text: &str) -> bool {
    let trimmed = text.trim_start();
    if trimmed.is_empty() {
        return false;
    }
    let head = trimmed.lines().next().unwrap_or("").trim();
    if is_handoff_heading(head) {
        return true;
    }
    let lower = trimmed.to_ascii_lowercase();
    let has_verdict = lower.contains("verdict:");
    let has_pointers = [
        "\npointers:",
        "\n## pointers",
        "\npointers\n",
        "\n- pointers:",
    ]
    .iter()
    .any(|k| lower.contains(k));
    let has_gaps = ["\ngaps:", "\n## gaps", "\ngaps\n", "\n- gaps:"]
        .iter()
        .any(|k| lower.contains(k));
    has_verdict && has_pointers && has_gaps
}

fn internodal_line_key(line: &str) -> String {
    let t = line.trim().trim_start_matches('#').trim();
    let t = t.trim_start_matches(['-', '*']).trim();
    t.trim_matches('*').trim().to_ascii_lowercase()
}

fn is_handoff_heading(line: &str) -> bool {
    internodal_line_key(line).trim_end_matches(':').trim() == "handoff"
}

fn is_verdict_field_line(line: &str) -> bool {
    let t = internodal_line_key(line);
    t == "verdict" || t == "verdict:" || t.starts_with("verdict:")
}

/// Byte offset where an internodal suffix begins (`HANDOFF` / `verdict:` / `---` + those).
#[must_use]
pub fn internodal_suffix_start(text: &str) -> Option<usize> {
    let mut offset = 0usize;
    let mut after_rule = None;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed == "---" {
            after_rule = Some(offset);
            offset += line.len();
            continue;
        }
        let start = after_rule.unwrap_or(offset);
        if is_handoff_heading(trimmed)
            || (is_verdict_field_line(trimmed) && looks_like_internodal_envelope(&text[start..]))
        {
            return Some(start);
        }
        after_rule = None;
        offset += line.len();
    }
    None
}

/// Drop internodal footer; keep the operator report prefix.
#[must_use]
pub fn strip_internodal_suffix(text: &str) -> String {
    match internodal_suffix_start(text) {
        Some(0) | None => text.to_string(),
        Some(i) => text[..i].trim_end().to_string(),
    }
}

/// Host template when Delivery rewrite is skipped or still internodal (no replan).
#[must_use]
pub fn parlor_fallback(user_task: &str, internodal: &str) -> String {
    let _ = user_task;
    let findings = section_after(internodal, &["findings:", "findings"]);
    let next = section_after(internodal, &["pointers:", "pointers"])
        .or_else(|| section_after(internodal, &["gaps:", "gaps"]));
    let mut out = String::new();
    if let Some(body) = findings {
        out.push_str(body.trim());
        out.push('\n');
    } else {
        let stripped = internodal
            .lines()
            .filter(|line| {
                let t = line.trim();
                !t.eq_ignore_ascii_case("handoff")
                    && !t.to_ascii_lowercase().starts_with("verdict:")
                    && !t.to_ascii_lowercase().starts_with("findings:")
                    && !t.to_ascii_lowercase().starts_with("pointers:")
                    && !t.to_ascii_lowercase().starts_with("gaps:")
            })
            .collect::<Vec<_>>()
            .join("\n");
        out.push_str(stripped.trim());
        out.push('\n');
    }
    if let Some(n) = next {
        let one: String = n
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("")
            .into();
        if !one.is_empty() {
            out.push_str("\n下一步：");
            out.push_str(one.trim_start_matches('-').trim());
            out.push('\n');
        }
    }
    let visible = out.trim().to_string();
    if looks_like_internodal_envelope(&visible) {
        "任务已完成部分核查。请根据会话中的步骤记录确认下一步；详细信封未向操作者展示。".into()
    } else {
        visible
    }
}

/// True when the text asserts a closed population (only / none / all) without
/// a domain-specific denylist — host uses this to stamp vantage, not to ban topics.
#[must_use]
pub fn has_exclusivity_quantifier(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    const CJK: &[&str] = &[
        "只有",
        "仅有",
        "唯一",
        "无人",
        "没有任何其他",
        "不存在其他",
        "全都没有",
    ];
    if CJK.iter().any(|p| text.contains(p)) {
        return true;
    }
    const EN: &[&str] = &[
        "only ",
        "the only",
        "no other",
        "nobody else",
        "none of the",
        "no one else",
    ];
    EN.iter().any(|p| lower.contains(p))
}

#[must_use]
pub fn declares_exhaustive_coverage(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("coverage: exhaustive")
        || lower.contains("coverage=exhaustive")
        || text.contains("穷尽")
        || lower.contains("exhaustive census")
}

fn already_scope_stamped(text: &str) -> bool {
    text.contains("当前观测所及") || text.contains("current vantage only")
}

/// If exclusive wording is unscoped, stamp that it is not a census.
#[must_use]
pub fn stamp_unscoped_exclusivity(text: &str, cjk: bool) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty()
        || !has_exclusivity_quantifier(trimmed)
        || declares_exhaustive_coverage(trimmed)
        || already_scope_stamped(trimmed)
        || trimmed.to_ascii_lowercase().contains("vantage:")
        || trimmed.contains("观测范围")
    {
        return text.to_string();
    }
    let stamp = if cjk {
        "（范围：当前观测所及，并非全集穷尽。）\n"
    } else {
        "(Scope: current vantage only; not an exhaustive census.)\n"
    };
    format!("{stamp}{text}")
}

/// Join earlier operator-visible text that used exclusive wording (session or mid-hop).
#[must_use]
pub fn collect_prior_exclusivity<'a, I>(chunks: I) -> String
where
    I: IntoIterator<Item = &'a str>,
{
    let mut out = String::new();
    for chunk in chunks {
        if !has_exclusivity_quantifier(chunk) {
            continue;
        }
        if !out.is_empty() {
            out.push_str("\n---\n");
        }
        let clip: String = chunk.chars().take(800).collect();
        out.push_str(&clip);
    }
    out.chars().take(4_000).collect()
}

/// Lead-in when later evidence must supersede earlier exclusive wording.
#[must_use]
pub fn revision_lead(prior_visible: &str, latest: &str, cjk: bool) -> Option<String> {
    if !has_exclusivity_quantifier(prior_visible) {
        return None;
    }
    let later_expands = declares_exhaustive_coverage(latest)
        || latest.to_ascii_lowercase().contains("vantage: remote")
        || latest.to_ascii_lowercase().contains("vantage: artifact")
        || latest.contains("观测范围")
        || latest.chars().count() > 120;
    if !later_expands && has_exclusivity_quantifier(latest) {
        return None;
    }
    Some(if cjk {
        "修订：此前排他结论来自更窄观测；以下以后续证据为准。\n".into()
    } else {
        "Revision: earlier exclusive claims were from a narrower vantage; later evidence below supersedes them.\n".into()
    })
}

/// Last-hop user body: internodal-free, then unscoped-exclusivity stamp.
#[must_use]
pub fn finalize_operator_visible(user_task: &str, body: &str) -> String {
    let visible = ensure_user_visible(user_task, body);
    stamp_unscoped_exclusivity(
        &visible,
        crate::agent::bounded_dag_live::user_prefers_cjk(user_task),
    )
}

fn section_after(text: &str, headers: &[&str]) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let mut start = None;
    for h in headers {
        if let Some(idx) = lower.find(h) {
            start = Some(idx + h.len());
            break;
        }
    }
    let start = start?;
    let rest = &text[start..];
    let rest_lower = rest.to_ascii_lowercase();
    let mut end = rest.len();
    for stop in ["\npointers:", "\ngaps:", "\nverdict:"] {
        if let Some(i) = rest_lower.find(stop) {
            end = end.min(i);
        }
    }
    let body = rest[..end].trim();
    if body.is_empty() {
        None
    } else {
        Some(body.to_string())
    }
}

/// Hard gate: internodal skeleton never leaves as the chat body.
#[must_use]
pub fn ensure_user_visible(user_task: &str, body: &str) -> String {
    let stripped = strip_internodal_suffix(body);
    if looks_like_internodal_envelope(&stripped) {
        parlor_fallback(user_task, &stripped)
    } else if stripped.is_empty() {
        parlor_fallback(user_task, body)
    } else {
        stripped
    }
}

/// Last hop ends the graph: parlor, never `replan_remaining` (VL-NA-035).
///
/// Ignores body shape. Envelope detection is only for rewriting internodal text.
#[must_use]
pub fn last_hop_ends_graph(remaining_nodes: usize) -> bool {
    remaining_nodes == 0
}

/// Mid-hops get an operator-visible note; the last hop is parlor only (VL-NA-037).
#[must_use]
pub fn should_emit_mid_hop_note(remaining_nodes: usize) -> bool {
    remaining_nodes > 0
}

const OPERATOR_NOTE_MAX: usize = 1200;
const MID_HOP_GIST_MAX: usize = 360;
const MID_HOP_GIST_LINES: usize = 4;

fn xml_tag_inner(text: &str, open: &str, close: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let o = open.to_ascii_lowercase();
    let c = close.to_ascii_lowercase();
    let start = lower.find(&o)? + o.len();
    let rest = &lower[start..];
    let end = rest.find(&c).unwrap_or(rest.len());
    let body = text[start..start + end].trim();
    if body.is_empty() {
        None
    } else {
        Some(body.to_string())
    }
}

fn internodal_noise_line(line: &str) -> bool {
    let k = internodal_line_key(line);
    let k = k.trim_end_matches(':').trim();
    matches!(
        k,
        "handoff"
            | "verdict"
            | "findings"
            | "pointers"
            | "gaps"
            | "next_node"
            | "next_node_id"
            | "vantage"
            | "coverage"
            | "claim_kind"
    ) || k.starts_with("verdict:")
        || k.starts_with("pointers:")
        || k.starts_with("next_node")
        || k.starts_with("vantage:")
        || k.starts_with("coverage:")
        || k.starts_with("claim_kind:")
        || k.starts_with("evidence_layer")
}

fn process_noise_line(line: &str) -> bool {
    let t = line.trim();
    if t.starts_with("**:") || t.contains("Node Result") {
        return true;
    }
    let l = t.to_ascii_lowercase();
    l.starts_with("let me ")
        || l.starts_with("there's ")
        || l.starts_with("there is ")
        || l.starts_with("i'll ")
        || l.starts_with("i will ")
}

fn gist_from_node_body(body: &str) -> String {
    if let Some(f) = xml_tag_inner(body, "<findings>", "</findings>") {
        return truncate_chars(f.trim(), MID_HOP_GIST_MAX);
    }
    if let Some(f) = section_after(body, &["findings:", "findings"]) {
        return truncate_chars(f.trim(), MID_HOP_GIST_MAX);
    }
    let stripped = strip_internodal_suffix(body);
    let mut kept = Vec::new();
    for line in stripped.lines() {
        let t = line.trim();
        if t.is_empty() || internodal_noise_line(t) || process_noise_line(t) {
            continue;
        }
        kept.push(t);
        if kept.len() >= MID_HOP_GIST_LINES {
            break;
        }
    }
    truncate_chars(&kept.join("\n"), MID_HOP_GIST_MAX)
}

fn truncate_chars(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn truncate_operator_note(text: &str) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c == '\r' { '\n' } else { c })
        .collect();
    truncate_chars(&flat, OPERATOR_NOTE_MAX)
}

/// Host-only mid-hop conclusion: internodal-free, no Delivery LLM.
#[must_use]
pub fn mid_hop_operator_note(
    user_task: &str,
    node_id: &str,
    body: &str,
    failed: Option<&str>,
) -> String {
    let label = crate::agent::bounded_dag_live::prettify_node_id(node_id);
    let cjk = crate::agent::bounded_dag_live::user_prefers_cjk(user_task);
    if let Some(err) = failed {
        let err = truncate_operator_note(err);
        return if cjk {
            format!("步骤「{label}」未完成。{err}")
        } else {
            format!("Step `{label}` did not finish. {err}")
        };
    }
    let extracted = gist_from_node_body(body);
    format!("### {label}\n{extracted}")
}

/// Append a streamed operator chunk; returns the progress frame to emit.
#[must_use]
pub fn append_operator_chunk(
    prefix: &mut String,
    text: &str,
) -> Option<crate::agent::turn_progress::TurnProgress> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    let piece = format!("{t}\n\n");
    prefix.push_str(&piece);
    Some(crate::agent::turn_progress::TurnProgress::Note { text: piece })
}

/// CLI live notes: same text as Web `TurnProgress::Note` (VL-NA-037).
pub fn print_operator_note(
    prefix: &mut String,
    text: &str,
    fold_cache: Option<&crate::agent::turn_progress::FoldCache>,
) {
    if let Some(progress) = append_operator_chunk(prefix, text) {
        crate::agent::turn_progress::print_cli_progress(&progress, fold_cache);
    }
}

/// WS already streamed `already`; send only the parlor suffix of `full`.
#[must_use]
pub fn remaining_operator_delta<'a>(already: &str, full: &'a str) -> &'a str {
    if already.is_empty() {
        return full;
    }
    full.strip_prefix(already).unwrap_or(full)
}

/// Mid-graph only: last hop skips the observe LLM (latency + no splice).
#[must_use]
pub fn should_observe_after_hop(remaining_nodes: usize) -> bool {
    remaining_nodes > 0
}

/// Last hop never replans the remaining chain, even if the body is prose.
#[must_use]
pub fn skip_replan_for_parlor(remaining_nodes: usize, _last_body: &str) -> bool {
    last_hop_ends_graph(remaining_nodes)
}

/// Per-hop RAO budget is the configured tool-iteration cap (not DAG hop count).
#[must_use]
pub fn per_hop_tool_iteration_budget(configured: usize) -> usize {
    if configured == 0 {
        10
    } else {
        configured
    }
}

/// Host Delivery: optional no-tool rewrite, then [`finalize_operator_visible`].
/// Same provider as the turn (planner-style `chat`, empty tools). Not a second tool-loop.
pub async fn host_delivery(
    provider: &dyn Provider,
    model: &str,
    temperature: f64,
    user_task: &str,
    last_node_body: &str,
    prior_visible: &str,
) -> Result<String> {
    let stripped = strip_internodal_suffix(last_node_body);
    let body = if looks_like_internodal_envelope(&stripped) {
        match delivery_chat(
            provider,
            model,
            temperature,
            user_task,
            &stripped,
            prior_visible,
        )
        .await
        {
            Ok(text) if !text.trim().is_empty() && !looks_like_internodal_envelope(&text) => text,
            Ok(_) | Err(_) => parlor_fallback(user_task, &stripped),
        }
    } else {
        last_node_body.to_string()
    };
    let mut visible = finalize_operator_visible(user_task, &body);
    let cjk = crate::agent::bounded_dag_live::user_prefers_cjk(user_task);
    if let Some(lead) = revision_lead(prior_visible, &visible, cjk) {
        visible = format!("{lead}{visible}");
    }
    Ok(visible)
}

async fn delivery_chat(
    provider: &dyn Provider,
    model: &str,
    temperature: f64,
    user_task: &str,
    internodal: &str,
    prior_visible: &str,
) -> Result<String> {
    let clip: String = internodal.chars().take(6_000).collect();
    let prior: String = prior_visible.chars().take(4_000).collect();
    let mut user = format!("USER TASK\n{user_task}\n\n");
    if !prior.trim().is_empty() {
        user.push_str(
            "PRIOR OPERATOR-VISIBLE CLAIMS (supersede exclusive wording if later evidence expands vantage)\n",
        );
        user.push_str(&prior);
        user.push_str("\n\n");
    }
    user.push_str("NODE ARTIFACT\n");
    user.push_str(&clip);
    let messages = [
        ChatMessage::system(DELIVERY_SYSTEM_PROMPT),
        ChatMessage::user(user),
    ];
    let request = ChatRequest {
        messages: &messages,
        tools: None,
    };
    let response = provider.chat(request, model, temperature).await?;
    Ok(response.text_or_empty().trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SMART_TUBE: &str = "HANDOFF\nverdict: partial\nfindings:\n- issue #5917 fixed in 32.38s\npointers:\n- upgrade to 32.38s\ngaps:\n- device version unknown";

    #[test]
    fn envelope_detected() {
        assert!(looks_like_internodal_envelope(SMART_TUBE));
        assert!(looks_like_internodal_envelope(
            "**Verdict: partial**\n## Findings(node 3/3,x)\nfoo\n## Pointers\na\n## Gaps\nb"
        ));
        assert!(!looks_like_internodal_envelope(
            "升级到 32.38s，若仍卡在 1 分钟再切 Cronet。"
        ));
    }

    #[test]
    fn delivery_prompt_names_evidence_layer() {
        assert!(DELIVERY_SYSTEM_PROMPT.contains("evidence_layer"));
        assert!(DELIVERY_SYSTEM_PROMPT.contains("prior-graph-artifact"));
        assert!(DELIVERY_SYSTEM_PROMPT.contains("live host/service health"));
        assert!(DELIVERY_SYSTEM_PROMPT.contains("other-session memory"));
    }

    #[test]
    fn strip_handoff_footer_keeps_report() {
        let mixed = "已完成全部检查。\n\n## Google\n| 项 | 值 |\n|---|---|\n| gProxy | 204 |\n---\n**HANDOFF**\n- verdict: ok\n- findings: x\n- pointers: y\n- gaps: z";
        let out = ensure_user_visible("检查 xray", mixed);
        assert!(out.contains("gProxy"));
        assert!(!out.to_ascii_lowercase().contains("handoff"));
        assert!(!out.contains("verdict:"));
        let no_heading = "表格报告。\n- verdict: ok\n- findings: a\n- pointers: b\n- gaps: c";
        let out2 = ensure_user_visible("检查", no_heading);
        assert!(out2.contains("表格报告"));
        assert!(!out2.contains("verdict:"));
    }

    #[test]
    fn fallback_strips_envelope_headers() {
        let out = parlor_fallback("电视 SmartTube 只播一分钟", SMART_TUBE);
        assert!(!looks_like_internodal_envelope(&out));
        assert!(out.contains("5917") || out.contains("32.38"));
        assert!(!out.trim_start().to_ascii_lowercase().starts_with("handoff"));
    }

    #[test]
    fn ensure_passes_clean_text() {
        let clean = "Google 路由当前可用。";
        assert_eq!(ensure_user_visible("check", clean), clean);
    }

    #[test]
    fn last_hop_always_ends_graph() {
        assert!(last_hop_ends_graph(0));
        assert!(!last_hop_ends_graph(2));
        assert!(!should_observe_after_hop(0));
        assert!(should_observe_after_hop(1));
        assert!(skip_replan_for_parlor(0, SMART_TUBE));
        assert!(skip_replan_for_parlor(0, "升级到 32.38s。"));
        assert!(!skip_replan_for_parlor(2, SMART_TUBE));
        assert!(should_emit_mid_hop_note(1));
        assert!(!should_emit_mid_hop_note(0));
    }

    #[test]
    fn mid_hop_note_strips_xml_findings() {
        let body = "Research complete.\n<handoff>\n<verdict>ok</verdict>\n<findings>\n- npm 2026.9.1\n</findings>\n<pointers>\nx\n</pointers>\n<gaps>\ny\n</gaps>\n</handoff>";
        let note = mid_hop_operator_note(
            "检查 openclaw",
            "research-official-upgrade-method",
            body,
            None,
        );
        assert!(note.contains("2026.9.1"), "{note}");
        assert!(!note.to_ascii_lowercase().contains("<handoff"), "{note}");
        assert!(!note.contains("pointers"), "{note}");
        assert!(should_emit_mid_hop_note(2));
    }

    #[test]
    fn remaining_delta_skips_already_streamed_prefix() {
        let already = "将按 2 步：a → b。\n\n";
        let full = format!("{already}最终结论");
        assert_eq!(remaining_operator_delta(already, &full), "最终结论");
        assert_eq!(remaining_operator_delta("", "only"), "only");
    }

    #[test]
    fn per_hop_budget_is_not_dag_hop_count() {
        assert_eq!(per_hop_tool_iteration_budget(0), 10);
        assert_eq!(per_hop_tool_iteration_budget(64), 64);
    }

    #[test]
    fn parlor_fallback_does_not_echo_user_task() {
        let out = parlor_fallback("请检查本地局域网有多少终端", SMART_TUBE);
        assert!(!out.contains("请检查本地局域网"));
        assert!(out.contains("5917"));
    }

    #[test]
    fn mid_hop_gist_is_short_and_drops_internodal_keys() {
        let body = "The ping scan shows 9 active hosts.\n\
pointers:\n- 192.168.2.13:8889\n\
next_node: xray-check\n\
findings:\n- this host .98 has 6 ESTAB to .13:8889\n\
- .87 did not answer ICMP\n\
gaps:\n- other hosts unknown";
        let note = mid_hop_operator_note("请检查局域网", "lan-scan", body, None);
        assert!(note.contains("ESTAB") || note.contains(".98"), "{note}");
        assert!(!note.contains("next_node"), "{note}");
        assert!(!note.contains("pointers"), "{note}");
        assert!(
            note.chars().count() < 500,
            "mid-hop must stay a gist, got {} chars: {note}",
            note.chars().count()
        );
        assert!(!note.contains("请检查局域网"), "{note}");
    }

    #[test]
    fn mid_hop_gist_drops_process_english() {
        let body =
            "Let me do one GraphQL call.\nThere's latency.\n## Node Result\nrepos listed: 15";
        let note = mid_hop_operator_note("检查仓库", "discover", body, None);
        assert!(!note.to_ascii_lowercase().contains("let me"), "{note}");
        assert!(!note.contains("Node Result"), "{note}");
        assert!(note.contains("15") || note.contains("repos"), "{note}");
    }

    #[test]
    fn persist_delta_is_parlor_when_prefix_not_in_body() {
        let prefix = "将按 2 步：a → b。\n\n### source trace\nshort gist\n\n";
        let parlor = "发行链已对齐 dist/v2。";
        assert_eq!(remaining_operator_delta(prefix, parlor), parlor);
        assert!(!parlor.contains("###"));
        assert!(!parlor.contains("将按"));
    }

    #[test]
    fn unscoped_exclusivity_gets_vantage_stamp() {
        let t = "当前使用该链路的终端——只有本机一台。";
        let out = stamp_unscoped_exclusivity(t, true);
        assert!(out.contains("当前观测所及"), "{out}");
        assert!(out.contains("只有本机"), "{out}");
        let scoped = "vantage: this_host\n只有本机连上了监听端口。";
        let out2 = stamp_unscoped_exclusivity(scoped, true);
        assert_eq!(out2, scoped);
        let exhaustive = "coverage: exhaustive\n只有这三台在 ARP 里。";
        assert_eq!(stamp_unscoped_exclusivity(exhaustive, true), exhaustive);
    }

    #[test]
    fn revision_lead_when_later_vantage_expands() {
        let prior = "当前使用该链路的终端——只有本机一台。";
        let later = "coverage: exhaustive\nvantage: artifact\n日志里 6 台主机有 ESTAB。";
        let lead = revision_lead(prior, later, true).expect("lead");
        assert!(lead.contains("修订"), "{lead}");
        assert!(revision_lead("no exclusive", later, true).is_none());
    }

    #[test]
    fn collect_prior_keeps_exclusive_chunks() {
        let s = collect_prior_exclusivity(["hello", "只有 .98", "ok"]);
        assert!(s.contains("只有 .98"));
        assert!(!s.contains("hello"));
    }

    #[test]
    fn finalize_stamps_unscoped_census_wording() {
        let out = finalize_operator_visible("请检查各终端", "当前使用该链路的终端——只有本机一台。");
        assert!(out.contains("当前观测所及"), "{out}");
        assert!(!out.to_ascii_lowercase().contains("handoff"));
    }
}
