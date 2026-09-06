//! Bounded linear L2 DAG stepper contract (VL-NA-010).
//!
//! Host-side node order + preview. Live execution stays [`crate::agent::loop_::run_tool_call_loop`]
//! (VL-NA-011). This module does not call an LLM or `execute_tool_batch`.
//!
//! 有界线性 L2 DAG 步进合同：只排序与预览；live 执行仍走既有 tool 环。

use super::dag_runner::{parse_dag_json, DagManifest, DagNode, CODE_FIX_TEMPLATE_JSON};
use anyhow::{bail, Context, Result};
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::path::Path;

/// Load a handwritten L2 DAG from a path, or the embedded code-fix template when `path` is empty.
pub fn load_bounded_dag(path: Option<&Path>) -> Result<DagManifest> {
    match path {
        Some(p) if !p.as_os_str().is_empty() => {
            let json = std::fs::read_to_string(p)
                .with_context(|| format!("read bounded DAG {}", p.display()))?;
            parse_dag_json(&json)
        }
        _ => parse_dag_json(CODE_FIX_TEMPLATE_JSON),
    }
}

/// Walk `entry` → `next` as a single chain. Fails if the graph is not a linear cover of all nodes.
pub fn linear_node_ids(dag: &DagManifest) -> Result<Vec<String>> {
    let by_id: HashMap<&str, &DagNode> = dag.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    if dag.nodes.len() != by_id.len() {
        bail!("bounded DAG '{}': duplicate node id", dag.id);
    }

    let mut order = Vec::new();
    let mut seen = HashSet::new();
    let mut current = dag.entry.as_str();
    let mut steps = 0u32;

    loop {
        if steps >= dag.max_steps {
            bail!(
                "bounded DAG '{}': linear walk exceeded max_steps={}",
                dag.id,
                dag.max_steps
            );
        }
        steps += 1;

        let Some(node) = by_id.get(current) else {
            bail!("bounded DAG '{}': missing node '{}'", dag.id, current);
        };
        if !seen.insert(current.to_string()) {
            bail!("bounded DAG '{}': cycle at '{}'", dag.id, current);
        }
        order.push(node.id.clone());

        match node.next.as_deref() {
            None => break,
            Some(next) => current = next,
        }
    }

    if order.len() != dag.nodes.len() {
        bail!(
            "bounded DAG '{}': not a linear cover (walked {} of {} nodes)",
            dag.id,
            order.len(),
            dag.nodes.len()
        );
    }

    Ok(order)
}

/// Operator-visible plan text after the planner (or operator-fixed path) has a graph.
pub fn format_preview(dag: &DagManifest, order: &[String]) -> String {
    let by_id: HashMap<&str, &DagNode> = dag.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    let mut out = format!(
        "Bounded task DAG `{}` ({} node(s), max_steps={}). Approve Build to run each node through the existing tool loop.\n",
        dag.id,
        order.len(),
        dag.max_steps
    );
    if let Some(desc) = dag.description.as_deref() {
        out.push_str(desc);
        out.push('\n');
    }
    out.push('\n');
    for (i, id) in order.iter().enumerate() {
        let Some(node) = by_id.get(id.as_str()) else {
            continue;
        };
        let next = node.next.as_deref().unwrap_or("(end)");
        let artifact = node
            .artifact
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        match artifact {
            Some(art) => {
                let _ = writeln!(
                    out,
                    "{}. {}  task_type={}  artifact={}  caps={}  next={}",
                    i + 1,
                    node.id,
                    node.task_type,
                    art,
                    node.model_selector.capabilities.join(","),
                    next
                );
            }
            None => {
                let _ = writeln!(
                    out,
                    "{}. {}  task_type={}  caps={}  next={}",
                    i + 1,
                    node.id,
                    node.task_type,
                    node.model_selector.capabilities.join(","),
                    next
                );
            }
        }
    }
    out
}

/// Host-filled work-node card (VL-NA-018). Fixed slots; not a domain prompt.
/// Last hop (`next` is END): operator-visible conclusion, not internodal HANDOFF.
pub fn node_task_card(dag_id: &str, node: &DagNode, index: usize, node_count: usize) -> String {
    let next = node.next.as_deref().unwrap_or("END");
    let last = node.next.is_none();
    let tools = "Prefer one compound shell (`&&` / pipes) over many tool rounds. \
         Independent checks: several commands in one ssh / one assistant message. \
         Work in this node's vantage. If INPUTS or USER TASK already name a vantage \
         (host, path, artifact), start there — do not substitute a local stand-in probe. \
         Do not re-run a probe whose result is already in INPUTS as this-hop-tool or this-graph-artifact. \
         Do not rewrite the same check as a new script_v2/v3 file; fix or compound the command. \
         Do not `find /` or open-ended local scans. \
         prior-graph-artifact (and other-session memory) is context for gaps only — \
         not a substitute for this-hop-tool on a live host or service check.";
    let success = if last {
        "Stop with the operator-visible conclusion as the last assistant message. \
         Do not emit internodal envelope headers (HANDOFF, verdict:, findings:, pointers:, gaps:). \
         State vantage (where you looked) and coverage (sample|partial|exhaustive). \
         Exclusive claims (only/none/all) require coverage=exhaustive; otherwise scope them \
         to the vantage. Label guesses as inference. \
         Name evidence_layer (this-hop-tool | this-graph-artifact | prior-graph-artifact | \
         host-config | protocol-dist | upstream-live | inference). \
         Recommend changes only on a layer you observed this hop. \
         Live health/status facts need this-hop-tool or this-graph-artifact from this run. \
         If later evidence revises an earlier exclusivity, say the revision. \
         The host delivers this text to the user; internodal handoff is only for mid-graph nodes."
    } else {
        "Stop with a HANDOFF as the last assistant message:\n\
         - vantage: this_host | remote_host | artifact | lan_passive | mixed\n\
         - coverage: sample | partial | exhaustive\n\
         - claim_kind: observation | inference | exclusivity\n\
         - evidence_layer: this-hop-tool | this-graph-artifact | prior-graph-artifact | host-config | protocol-dist | upstream-live | inference\n\
         - verdict: ok | partial | failed\n\
         - findings: facts at this vantage from this-hop-tool / this-graph-artifact (not a census of unseen vantages; prior-graph belongs in gaps)\n\
         - pointers: identifiers the next node needs (not source dumps)\n\
         - gaps: unknowns; keep exclusivity here until coverage=exhaustive"
    };
    let mid_hint = "The host counts a shell round only after a command actually ran \
         (policy-deny and repeat-skip do not consume the cap). After four such rounds \
         the host injects a cap notice. Do not stop early or claim a cap unless that \
         notice appeared. Until then, compound remaining checks in one ssh.";
    format!(
        "NODE TASK (host-filled slots; do not rewrite this card)\n\
         - dag_id: {dag_id}\n\
         - node_id: {id}\n\
         - index: {index} of {node_count}\n\
         - task_type: {task_type}\n\
         - next_node_id: {next}\n\
         \n\
         OBJECTIVE\n\
         Do only this node's job (task_type) for USER TASK. Do not start {next}. \
         Do not redo a prior node unless INPUTS lack pointers you need.\n\
         \n\
         TOOLS\n\
         {tools} {mid_hint}\n\
         \n\
         SUCCESS\n\
         {success}",
        id = node.id,
        task_type = node.task_type,
    )
}

/// True when this node should receive every prior clip, not only the last.
#[must_use]
pub fn is_aggregate_task_type(task_type: &str) -> bool {
    matches!(
        task_type.trim().to_ascii_lowercase().as_str(),
        "summarize" | "report"
    )
}

/// Short note kept for tests / older call sites (same job as the card id line).
pub fn node_system_note(node_id: &str, task_type: &str) -> String {
    format!(
        "You are executing bounded DAG node '{node_id}' (task_type={task_type}). Stay on this node's job; do not skip ahead."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_fix_template_is_linear_locate_patch_verify() {
        let dag = parse_dag_json(CODE_FIX_TEMPLATE_JSON).unwrap();
        let ids = linear_node_ids(&dag).unwrap();
        assert_eq!(ids, vec!["locate", "patch", "verify"]);
        let preview = format_preview(&dag, &ids);
        assert!(preview.contains("locate"));
        assert!(preview.contains("patch"));
        assert!(preview.contains("verify"));
        assert!(preview.contains("Approve Build"));
    }

    #[test]
    fn unused_node_fails_cover() {
        let json = r#"{
          "schema_version": "0.1.0",
          "id": "branchy",
          "entry": "a",
          "max_steps": 8,
          "nodes": [
            {"id":"a","task_type":"t","model_selector":{"capabilities":["coding"]},"next":null},
            {"id":"b","task_type":"t","model_selector":{"capabilities":["coding"]},"next":null}
          ]
        }"#;
        let dag = parse_dag_json(json).unwrap();
        let err = linear_node_ids(&dag).unwrap_err().to_string();
        assert!(err.contains("linear cover"), "{err}");
    }

    #[test]
    fn cycle_fails() {
        let json = r#"{
          "schema_version": "0.1.0",
          "id": "loop",
          "entry": "a",
          "max_steps": 8,
          "nodes": [
            {"id":"a","task_type":"t","model_selector":{"capabilities":["coding"]},"next":"b"},
            {"id":"b","task_type":"t","model_selector":{"capabilities":["coding"]},"next":"a"}
          ]
        }"#;
        let dag = parse_dag_json(json).unwrap();
        let err = linear_node_ids(&dag).unwrap_err().to_string();
        assert!(err.contains("cycle") || err.contains("max_steps"), "{err}");
    }

    #[test]
    fn node_note_names_id() {
        let note = node_system_note("locate", "code-fix");
        assert!(note.contains("locate"));
        assert!(note.contains("code-fix"));
    }

    #[test]
    fn node_task_card_includes_next_and_handoff() {
        let dag = parse_dag_json(CODE_FIX_TEMPLATE_JSON).unwrap();
        let locate = dag.nodes.iter().find(|n| n.id == "locate").unwrap();
        let card = node_task_card("code-fix-template", locate, 1, 3);
        assert!(card.contains("next_node_id: patch"));
        assert!(card.contains("HANDOFF"));
        assert!(card.contains("compound shell"));
        assert!(card.contains("vantage:"));
        assert!(card.contains("coverage:"));
        assert!(card.contains("claim_kind:"));
        assert!(card.contains("evidence_layer:"));
        assert!(card.contains("pointers:"));
        assert!(!card.contains("Approve Build"));
        assert!(
            !card.contains("names a remote host"),
            "no host-name special case: {card}"
        );
        let verify = dag.nodes.iter().find(|n| n.id == "verify").unwrap();
        let end = node_task_card("code-fix-template", verify, 3, 3);
        assert!(end.contains("next_node_id: END"));
        assert!(
            !end.contains("Stop with a HANDOFF"),
            "last hop must not demand internodal HANDOFF: {end}"
        );
        assert!(end.contains("operator-visible conclusion"));
        assert!(end.contains("evidence_layer"));
        assert!(end.contains("coverage=exhaustive"));
        assert!(
            !card.contains("Aim for at most four shell rounds"),
            "must not teach early HANDOFF on a soft four-round slogan: {card}"
        );
        assert!(card.contains("this-hop-tool"));
        assert!(card.contains("SHELL_ROUND_CAP") || card.contains("cap notice"));
        assert!(card.contains("actually ran"));
    }

    #[test]
    fn summarize_is_aggregate_task_type() {
        assert!(is_aggregate_task_type("summarize"));
        assert!(is_aggregate_task_type("Report"));
        assert!(!is_aggregate_task_type("ops-check"));
    }

    #[test]
    fn load_embedded_when_path_none() {
        let dag = load_bounded_dag(None).unwrap();
        assert_eq!(dag.id, "code-fix-template");
    }

    #[test]
    fn artifact_optional_and_preview() {
        let json = r#"{
          "schema_version": "0.1.0",
          "id": "one",
          "entry": "a",
          "max_steps": 8,
          "nodes": [
            {"id":"a","task_type":"ops-check","model_selector":{"capabilities":["tool_calling"]},"next":null}
          ]
        }"#;
        let dag = parse_dag_json(json).unwrap();
        assert!(dag.nodes[0].artifact.is_none());
        let with = r#"{
          "schema_version": "0.1.0",
          "id": "one",
          "entry": "a",
          "max_steps": 8,
          "nodes": [
            {"id":"a","task_type":"ops-check","artifact":"official-issue-notes","model_selector":{"capabilities":["tool_calling"]},"next":null}
          ]
        }"#;
        let dag = parse_dag_json(with).unwrap();
        let preview = format_preview(&dag, &["a".into()]);
        assert!(preview.contains("artifact=official-issue-notes"));
    }
}
