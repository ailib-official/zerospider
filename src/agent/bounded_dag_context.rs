//! Per-node Context DB writeback + capability Contact (VL-NA-013/014).
//!
//! Live only when `[agent].bounded_dag_live` is on. Does not add a
//! second tool loop: callers still invoke [`super::loop_::run_tool_call_loop`].
//!
//! 节点产物写入既有 Memory；下一步只注入合同允许的块。Contact 按节点 capabilities 选 hint。

use super::bounded_dag::{is_aggregate_task_type, node_task_card};
use super::context_contract::{retrieve_memory_chunks, retrieve_workspace_files};
use super::dag_runner::DagNode;
use super::intent_route::{hint_to_tag, hints_for_tag};
use crate::memory::{Memory, MemoryCategory};
use crate::orchestration::{TurnModelDecision, TurnModelSource};
use crate::providers::ChatMessage;
use anyhow::Result;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Clip node output stored as a Daily memory row (layer-3 / tool_result retrieve).
pub const ARTIFACT_MAX_CHARS: usize = 4_096;

/// Prefer the planner's first listed tag so mixed caps do not collapse to coding.
/// Preference order is only a fallback when listed tags have no configured hint.
const CONTACT_TAG_PREFERENCE: &[&str] = &[
    "document_understanding",
    "high-reasoning",
    "coding",
    "tool_calling",
    "speed",
];

fn hint_contact_for_tag(
    tag: &str,
    available_hints: &[String],
    capabilities: Vec<String>,
) -> Option<NodeContact> {
    let mut candidates = hints_for_tag(tag);
    if !candidates.iter().any(|h| h.eq_ignore_ascii_case(tag)) {
        candidates.push(tag);
    }
    for hint in candidates {
        if available_hints.iter().any(|h| h.eq_ignore_ascii_case(hint)) {
            let canon = available_hints
                .iter()
                .find(|h| h.eq_ignore_ascii_case(hint))
                .cloned()
                .unwrap_or_else(|| hint.to_string());
            return Some(NodeContact {
                model: format!("hint:{canon}"),
                reason: format!("node_capability:{tag}:hint:{canon}"),
                capabilities,
            });
        }
    }
    None
}

/// Observable Contact choice for one DAG node (not a second router).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeContact {
    pub model: String,
    pub reason: String,
    pub capabilities: Vec<String>,
}

impl NodeContact {
    #[must_use]
    pub fn observe_line(&self) -> String {
        format!(
            "contact model={} reason={} caps={}",
            self.model,
            self.reason,
            self.capabilities.join(",")
        )
    }

    #[must_use]
    pub fn to_turn_decision(&self) -> TurnModelDecision {
        TurnModelDecision {
            model: self.model.clone(),
            source: TurnModelSource::NodeCapability,
            reason: self.reason.clone(),
        }
    }
}

pub fn artifact_memory_key(session_id: &str, node_id: &str) -> String {
    format!("dag_art:{session_id}:{node_id}")
}

fn clip_artifact(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= ARTIFACT_MAX_CHARS {
        return trimmed.to_string();
    }
    trimmed.chars().take(ARTIFACT_MAX_CHARS).collect()
}

pub async fn store_node_artifact(
    mem: &dyn Memory,
    session_id: &str,
    node_id: &str,
    text: &str,
) -> Result<()> {
    let key = artifact_memory_key(session_id, node_id);
    mem.store(
        &key,
        &clip_artifact(text),
        MemoryCategory::Daily,
        Some(session_id),
    )
    .await
}

pub async fn load_node_artifact(
    mem: &dyn Memory,
    session_id: &str,
    node_id: &str,
) -> Result<Option<String>> {
    let key = artifact_memory_key(session_id, node_id);
    Ok(mem.get(&key).await?.map(|e| e.content))
}

fn chunk_plain_text(chunk: &ai_lib_rust::context::MessageChunk) -> String {
    match &chunk.message.content {
        ai_lib_rust::types::message::MessageContent::Text(s) => s.clone(),
        ai_lib_rust::types::message::MessageContent::Blocks(_) => {
            format!("[{}]", chunk.chunk_id)
        }
    }
}

/// Slots for one work-node packet (host-filled; GOV-007 still uses prepare_turn_history).
pub struct NodeWorkPacket<'a> {
    pub dag_id: &'a str,
    pub node: &'a DagNode,
    pub index: usize,
    pub node_count: usize,
    pub user_task: &'a str,
    pub retrieve_texts: &'a [String],
}

/// User-role retrieve blobs for this node (workspace / memory / prior artifacts).
pub async fn node_retrieve_texts(
    mem: &dyn Memory,
    workspace_dir: &Path,
    session_id: &str,
    node: &DagNode,
    prior_ids: &[String],
) -> Vec<String> {
    let mut out = Vec::new();
    let mut injected: HashSet<String> = HashSet::new();

    for retrieve in &node.context_requirements.retrieve {
        match retrieve.kind.as_str() {
            "workspace" => {
                if let Ok(chunks) = retrieve_workspace_files(workspace_dir) {
                    for chunk in chunks {
                        out.push(chunk_plain_text(&chunk));
                    }
                }
            }
            "memory" => {
                let q = retrieve.query.as_deref().unwrap_or("");
                if let Ok(chunks) = retrieve_memory_chunks(mem, q, 3, Some(session_id)).await {
                    for chunk in chunks {
                        out.push(chunk_plain_text(&chunk));
                    }
                }
            }
            "tool_result" => {
                for prev in prior_ids_for_node(node, prior_ids) {
                    if injected.contains(prev) {
                        continue;
                    }
                    if let Ok(Some(body)) = load_node_artifact(mem, session_id, prev).await {
                        out.push(format!(
                            "[dag_artifact node={prev} alias={}]\n{body}",
                            retrieve.alias.as_deref().unwrap_or("tool_result")
                        ));
                        injected.insert(prev.clone());
                    }
                }
            }
            other => {
                tracing::debug!(kind = other, "bounded DAG retrieve kind skipped");
            }
        }
    }

    let wants_summary = node
        .context_requirements
        .layers
        .iter()
        .any(|&layer| layer >= 3);
    for prev in prior_ids_for_node(node, prior_ids) {
        if injected.contains(prev) {
            continue;
        }
        if let Ok(Some(body)) = load_node_artifact(mem, session_id, prev).await {
            let tag = if wants_summary { "layer=3" } else { "prior" };
            out.push(format!("[dag_artifact node={prev} {tag}]\n{body}"));
            injected.insert(prev.clone());
        }
    }

    out
}

#[must_use]
pub fn graph_run_token(session_id: &str, dag_id: &str) -> String {
    format!(
        "{}-{}",
        sanitize_id_token(session_id),
        sanitize_id_token(dag_id)
    )
}

#[must_use]
pub fn graph_scratch_rel(session_id: &str, dag_id: &str) -> String {
    format!(
        "{}/graphs/{}/{}",
        crate::security::policy::SCRATCH_REL,
        sanitize_id_token(session_id),
        sanitize_id_token(dag_id)
    )
}

fn sanitize_id_token(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(48)
        .collect()
}

pub fn ensure_graph_scratch(
    workspace: &Path,
    session_id: &str,
    dag_id: &str,
) -> std::io::Result<PathBuf> {
    let rel = graph_scratch_rel(session_id, dag_id);
    let dir = workspace.join(&rel);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[must_use]
pub fn cached_fail_block(fail: &crate::agent::bounded_dag_live::DagFailCursor) -> String {
    format!(
        "[cached hop_fail node={} class={} evidence_layer=prior-graph-artifact]\n{}",
        fail.node_id,
        fail.fail_class,
        fail.err.chars().take(800).collect::<String>()
    )
}

/// INPUTS listing: this-graph scratch + same-session prior runs. Never lists other sessions.
#[must_use]
pub fn scratch_retrieve_text(workspace: &Path, session_id: &str, dag_id: &str) -> Option<String> {
    let current = graph_scratch_rel(session_id, dag_id);
    let session_root = workspace.join(format!(
        "{}/graphs/{}",
        crate::security::policy::SCRATCH_REL,
        sanitize_id_token(session_id)
    ));
    let mut lines = vec![format!(
        "[this-graph-artifact scratch={current}]\nWrite temp files only under this directory. Do not treat other tmp files as this-task evidence. prior-graph-artifact listings below are context for gaps only — live USER TASK needs this-hop-tool."
    )];
    if let Ok(rd) = std::fs::read_dir(&session_root) {
        for ent in rd.flatten() {
            let name = ent.file_name();
            let name = name.to_string_lossy();
            if name.as_ref() == sanitize_id_token(dag_id) {
                continue;
            }
            lines.push(format!(
                "[prior-graph-artifact scratch={}/graphs/{session_seg}/{name}] context-only; do not copy findings into this hop",
                crate::security::policy::SCRATCH_REL,
                session_seg = sanitize_id_token(session_id)
            ));
        }
    }
    Some(lines.join("\n"))
}

fn prior_ids_for_node<'a>(node: &DagNode, prior_ids: &'a [String]) -> &'a [String] {
    if prior_ids.is_empty() {
        return prior_ids;
    }
    if is_aggregate_task_type(&node.task_type) {
        return prior_ids;
    }
    let start = prior_ids.len() - 1;
    &prior_ids[start..]
}

fn keep_leading_product_system(history: &mut Vec<ChatMessage>) {
    let first = history
        .iter()
        .find(|message| message.role == "system" && !message.content.starts_with("NODE TASK"))
        .cloned()
        .or_else(|| {
            history
                .iter()
                .find(|message| message.role == "system")
                .cloned()
        });
    history.clear();
    if let Some(system) = first {
        history.push(system);
    }
}

/// Rebuild work-node chat: product system + task card + USER TASK + retrieve (no Plan chrome).
///
/// `product_system`: when set, replace the leading product system (work-node P0+P1 pack).
/// When `None`, keep the first non-NODE-TASK system already in `history`.
pub fn reset_chat_scope(
    history: &mut Vec<ChatMessage>,
    packet: &NodeWorkPacket<'_>,
    product_system: Option<&str>,
) {
    if let Some(system) = product_system {
        history.clear();
        if !system.trim().is_empty() {
            history.push(ChatMessage::system(system));
        }
    } else {
        keep_leading_product_system(history);
    }
    history.push(ChatMessage::system(node_task_card(
        packet.dag_id,
        packet.node,
        packet.index,
        packet.node_count,
    )));
    let task = packet.user_task.trim();
    if !task.is_empty() {
        history.push(ChatMessage::user(format!("USER TASK\n{task}")));
    }
    for text in packet.retrieve_texts {
        history.push(ChatMessage::user(text.clone()));
    }
}

/// Map node `model_selector.capabilities` to a `hint:` id or the session default.
///
/// Planner / Plan-preview still use capability Contact. Live **work** hops pass
/// the session picker as `explicit_model` via [`contact_for_live_node`].
/// Does not enable `host_decide` / CAP live.
pub fn contact_for_node(
    node: &DagNode,
    default_model: &str,
    available_hints: &[String],
    explicit_model: Option<&str>,
) -> NodeContact {
    let capabilities = node.model_selector.capabilities.clone();
    if let Some(raw) = explicit_model.map(str::trim).filter(|s| !s.is_empty()) {
        return NodeContact {
            model: raw.to_string(),
            reason: "explicit_user_pick".into(),
            capabilities,
        };
    }

    for cap in &capabilities {
        let tag = hint_to_tag(cap).unwrap_or(cap.as_str());
        if let Some(c) = hint_contact_for_tag(tag, available_hints, capabilities.clone()) {
            return c;
        }
    }

    for tag in CONTACT_TAG_PREFERENCE {
        if !capabilities
            .iter()
            .any(|c| hint_to_tag(c).is_some_and(|t| t.eq_ignore_ascii_case(tag)) || c == tag)
        {
            continue;
        }
        if let Some(c) = hint_contact_for_tag(tag, available_hints, capabilities.clone()) {
            return c;
        }
    }

    NodeContact {
        model: default_model.to_string(),
        reason: "node_capability:unmapped_default".into(),
        capabilities,
    }
}

/// Live work hop: session picker is the default model (VL-NA-043).
/// Capability hints apply only after [`force_default`] fail-strategy (peer / retry).
pub fn contact_for_live_node(
    node: &DagNode,
    default_model: &str,
    available_hints: &[String],
    force_default: bool,
) -> NodeContact {
    if force_default {
        return NodeContact {
            model: default_model.to_string(),
            reason: "fail_strategy:default_model".into(),
            capabilities: node.model_selector.capabilities.clone(),
        };
    }
    let session = default_model.trim();
    if session.is_empty() {
        return contact_for_node(node, default_model, available_hints, None);
    }
    contact_for_node(node, default_model, available_hints, Some(session))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::dag_runner::{parse_dag_json, CODE_FIX_TEMPLATE_JSON};

    #[test]
    fn verify_prefers_speed_hint() {
        let dag = parse_dag_json(CODE_FIX_TEMPLATE_JSON).unwrap();
        let verify = dag.nodes.iter().find(|n| n.id == "verify").unwrap();
        let c = contact_for_node(
            verify,
            "deepseek/deepseek-v4-flash",
            &["fast".into(), "code".into()],
            None,
        );
        assert_eq!(c.model, "hint:fast");
        assert!(c.reason.contains("speed"), "{}", c.reason);
    }

    #[test]
    fn locate_prefers_coding_hint() {
        let dag = parse_dag_json(CODE_FIX_TEMPLATE_JSON).unwrap();
        let locate = dag.nodes.iter().find(|n| n.id == "locate").unwrap();
        let c = contact_for_node(
            locate,
            "deepseek/deepseek-v4-flash",
            &["fast".into(), "code".into()],
            None,
        );
        assert_eq!(c.model, "hint:code");
        assert!(c.reason.contains("coding"), "{}", c.reason);
    }

    #[test]
    fn explicit_pick_wins() {
        let dag = parse_dag_json(CODE_FIX_TEMPLATE_JSON).unwrap();
        let locate = dag.nodes.iter().find(|n| n.id == "locate").unwrap();
        let c = contact_for_node(
            locate,
            "default/x",
            &["code".into()],
            Some("nvidia/nemotron"),
        );
        assert_eq!(c.model, "nvidia/nemotron");
        assert_eq!(c.reason, "explicit_user_pick");
    }

    #[test]
    fn live_work_hop_uses_session_model_not_coding_hint() {
        let dag = parse_dag_json(CODE_FIX_TEMPLATE_JSON).unwrap();
        let locate = dag.nodes.iter().find(|n| n.id == "locate").unwrap();
        let c = contact_for_live_node(
            locate,
            "nvidia/nemotron-3-ultra-550b-a55b",
            &["code".into(), "fast".into()],
            false,
        );
        assert_eq!(c.model, "nvidia/nemotron-3-ultra-550b-a55b");
        assert_eq!(c.reason, "explicit_user_pick");
        let retried = contact_for_live_node(
            locate,
            "nvidia/nemotron-3-ultra-550b-a55b",
            &["code".into()],
            true,
        );
        assert_eq!(retried.reason, "fail_strategy:default_model");
        assert_eq!(retried.model, "nvidia/nemotron-3-ultra-550b-a55b");
    }

    #[test]
    fn document_prefers_document_hint() {
        let json = r#"{
          "schema_version": "0.1.0",
          "id": "paper",
          "entry": "read",
          "max_steps": 8,
          "nodes": [
            {"id":"read","task_type":"summarize","model_selector":{"capabilities":["document_understanding"]},"next":null}
          ]
        }"#;
        let dag = parse_dag_json(json).unwrap();
        let read = &dag.nodes[0];
        let c = contact_for_node(
            read,
            "deepseek/deepseek-v4-flash",
            &["document".into(), "fast".into()],
            None,
        );
        assert_eq!(c.model, "hint:document");
        assert!(c.reason.contains("document_understanding"), "{}", c.reason);
    }

    #[test]
    fn first_listed_capability_wins_over_preference() {
        let json = r#"{
          "schema_version": "0.1.0",
          "id": "mix",
          "entry": "n",
          "max_steps": 8,
          "nodes": [
            {"id":"n","task_type":"ops-check","model_selector":{"capabilities":["speed","coding"]},"next":null}
          ]
        }"#;
        let dag = parse_dag_json(json).unwrap();
        let c = contact_for_node(
            &dag.nodes[0],
            "deepseek/deepseek-v4-flash",
            &["fast".into(), "code".into()],
            None,
        );
        assert_eq!(c.model, "hint:fast", "{}", c.reason);
    }

    #[test]
    fn artifact_key_is_session_scoped() {
        assert_eq!(artifact_memory_key("s1", "locate"), "dag_art:s1:locate");
    }

    #[tokio::test]
    async fn empty_retrieve_still_injects_prior_artifact() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = crate::config::MemoryConfig {
            backend: "sqlite".into(),
            ..crate::config::MemoryConfig::default()
        };
        let mem = crate::memory::create_memory(&cfg, tmp.path(), None).unwrap();
        let json = r#"{
          "schema_version": "0.1.0",
          "id": "generic",
          "entry": "a",
          "max_steps": 8,
          "nodes": [
            {"id":"a","task_type":"read","model_selector":{"capabilities":["document_understanding"]},"next":"b"},
            {"id":"b","task_type":"write","model_selector":{"capabilities":["high-reasoning"]},"next":null}
          ]
        }"#;
        let dag = parse_dag_json(json).unwrap();
        let b = dag.nodes.iter().find(|n| n.id == "b").unwrap();
        store_node_artifact(mem.as_ref(), "sess", "a", "PRIOR_BODY_UNIQUE")
            .await
            .unwrap();
        let texts = node_retrieve_texts(mem.as_ref(), tmp.path(), "sess", b, &["a".into()]).await;
        assert!(
            texts.iter().any(|t| t.contains("PRIOR_BODY_UNIQUE")),
            "{texts:?}"
        );
        assert!(
            texts.iter().any(|t| t.contains("dag_artifact")),
            "{texts:?}"
        );
    }

    #[test]
    fn reset_chat_scope_drops_plan_preview() {
        let dag = parse_dag_json(CODE_FIX_TEMPLATE_JSON).unwrap();
        let locate = dag.nodes.iter().find(|n| n.id == "locate").unwrap();
        let mut history = vec![
            ChatMessage::system("product system"),
            ChatMessage::user("fix the flaky test"),
            ChatMessage::assistant("Bounded task DAG `code-fix-template`. Approve Build to run."),
            ChatMessage::user("同意"),
        ];
        reset_chat_scope(
            &mut history,
            &NodeWorkPacket {
                dag_id: "code-fix-template",
                node: locate,
                index: 1,
                node_count: 3,
                user_task: "fix the flaky test",
                retrieve_texts: &[],
            },
            None,
        );
        let joined: String = history.iter().map(|m| m.content.as_str()).collect();
        assert!(joined.contains("product system"));
        assert!(joined.contains("USER TASK"));
        assert!(joined.contains("fix the flaky test"));
        assert!(joined.contains("next_node_id: patch"));
        assert!(!joined.contains("Approve Build"));
        assert!(!joined.contains("同意"));
    }

    #[test]
    fn reset_chat_scope_can_replace_product_system() {
        let dag = parse_dag_json(CODE_FIX_TEMPLATE_JSON).unwrap();
        let locate = dag.nodes.iter().find(|n| n.id == "locate").unwrap();
        let mut history = vec![ChatMessage::system("FAT SYSTEM WITH TOOL JSON")];
        reset_chat_scope(
            &mut history,
            &NodeWorkPacket {
                dag_id: "code-fix-template",
                node: locate,
                index: 1,
                node_count: 3,
                user_task: "generic task",
                retrieve_texts: &[],
            },
            Some("slim P0+P1"),
        );
        assert_eq!(history[0].content, "slim P0+P1");
        assert!(!history.iter().any(|m| m.content.contains("FAT SYSTEM")));
    }

    #[tokio::test]
    async fn summarize_injects_all_prior_artifacts() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = crate::config::MemoryConfig {
            backend: "sqlite".into(),
            ..crate::config::MemoryConfig::default()
        };
        let mem = crate::memory::create_memory(&cfg, tmp.path(), None).unwrap();
        let json = r#"{
          "schema_version": "0.1.0",
          "id": "generic",
          "entry": "a",
          "max_steps": 8,
          "nodes": [
            {"id":"a","task_type":"ops-check","model_selector":{"capabilities":["coding"]},"next":"b"},
            {"id":"b","task_type":"analyze","model_selector":{"capabilities":["document_understanding"]},"next":"c"},
            {"id":"c","task_type":"summarize","model_selector":{"capabilities":["document_understanding"]},"next":null}
          ]
        }"#;
        let dag = parse_dag_json(json).unwrap();
        let c = dag.nodes.iter().find(|n| n.id == "c").unwrap();
        store_node_artifact(mem.as_ref(), "sess", "a", "ALPHA_ONLY")
            .await
            .unwrap();
        store_node_artifact(mem.as_ref(), "sess", "b", "BETA_ONLY")
            .await
            .unwrap();
        let texts = node_retrieve_texts(
            mem.as_ref(),
            tmp.path(),
            "sess",
            c,
            &["a".into(), "b".into()],
        )
        .await;
        assert!(texts.iter().any(|t| t.contains("ALPHA_ONLY")), "{texts:?}");
        assert!(texts.iter().any(|t| t.contains("BETA_ONLY")), "{texts:?}");
        let b = dag.nodes.iter().find(|n| n.id == "b").unwrap();
        let mid = node_retrieve_texts(mem.as_ref(), tmp.path(), "sess", b, &["a".into()]).await;
        assert!(mid.iter().any(|t| t.contains("ALPHA_ONLY")));
        assert!(!mid.iter().any(|t| t.contains("BETA_ONLY")));
    }

    #[test]
    fn scratch_listing_labels_this_and_prior_graphs() {
        let tmp = tempfile::tempdir().unwrap();
        let session = "sess-a";
        ensure_graph_scratch(tmp.path(), session, "run1").unwrap();
        ensure_graph_scratch(tmp.path(), session, "run2").unwrap();
        std::fs::create_dir_all(
            tmp.path()
                .join(graph_scratch_rel("other-session", "foreign")),
        )
        .unwrap();
        let text = scratch_retrieve_text(tmp.path(), session, "run1").unwrap();
        assert!(text.contains("this-graph-artifact"), "{text}");
        assert!(text.contains("prior-graph-artifact"), "{text}");
        assert!(text.contains("run2"), "{text}");
        assert!(!text.contains("foreign"), "{text}");
        assert!(!text.contains("other-session"), "{text}");
        assert!(
            text.contains("context-only") || text.contains("context for gaps"),
            "{text}"
        );
    }

    #[test]
    fn cached_fail_block_is_labeled_cached_not_upstream_live() {
        let fail = crate::agent::bounded_dag_live::DagFailCursor {
            node_id: "n1".into(),
            index: 0,
            err: "timeout talking to api".into(),
            dag_id: "run1".into(),
            auto_replan_count: 1,
            fail_class: "timeout".into(),
        };
        let block = cached_fail_block(&fail);
        assert!(block.contains("[cached hop_fail"), "{block}");
        assert!(
            block.contains("evidence_layer=prior-graph-artifact"),
            "{block}"
        );
        assert!(!block.contains("upstream-live"), "{block}");
        assert!(block.contains("n1"), "{block}");
    }
}
