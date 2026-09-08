//! Live bounded DAG: first hop is chat_only or a linear graph; work is DAG nodes.
//!
//! When `[agent].bounded_dag_live` is on and `bounded_dag_path` is empty, the
//! first hop emits in-band `chat_only` or a 1–8 node linear DAG. There is no
//! parallel `single_work` tool loop: that label (and invalid JSON) becomes a
//! synthetic 1-node graph, then one split-refine chat before tools. Plan phase,
//! fail cursor, and operator-fixed graphs skip refine. Dist default remains off.
//!
//! Turn contract (VL-CTX-001 / VL-NA-019 / VL-NA-030): append the user message,
//! run `prepare_turn_history` on the session (skip only HostPhase::Plan preview),
//! then first hop **consumes that prepared history**. DAG nodes still slim via
//! `reset_chat_scope` (intra-graph). Session follow-ups do not replace the
//! stored graph task.
//!
//! 有界 DAG live：首跳仅 chat_only 或线性图；1 节点开工具前再拆一次。

use super::bounded_dag::{format_preview, linear_node_ids, load_bounded_dag};
use super::bounded_dag_context::contact_for_node;
use super::candidate_dag::validate_candidate_dag_json;
use super::dag_runner::{parse_dag_json, DagManifest, CODE_FIX_TEMPLATE_JSON};
use super::host_phase::HostPhase;
use crate::memory::{Memory, MemoryCategory};
use crate::orchestration::dag_emit::{
    extract_json_object, planner_chat_text, DAG_PLAN_SYSTEM_PROMPT,
};
use crate::providers::{ChatMessage, ChatRequest, Provider};
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::hash::BuildHasher;
use std::path::Path;

pub const PLANNED_DAG_KEY_PREFIX: &str = "dag_plan:";
pub const DAG_FAIL_KEY_PREFIX: &str = "dag_fail:";
const MAX_SESSION_DAG_FORGET: usize = 32;

pub fn planned_dag_key(session_id: &str) -> String {
    format!("{PLANNED_DAG_KEY_PREFIX}{session_id}")
}

pub fn dag_fail_key(session_id: &str) -> String {
    format!("{DAG_FAIL_KEY_PREFIX}{session_id}")
}

pub const GRAPH_USER_KEY_PREFIX: &str = "dag_user:";

pub fn graph_user_key(session_id: &str) -> String {
    format!("{GRAPH_USER_KEY_PREFIX}{session_id}")
}

/// Observe whether the last assistant output advanced the user task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObserveVerdict {
    Continue,
    ReplanRemaining,
    Stop,
}

/// Chat-only vs DAG. DAG rules are [`DAG_PLAN_SYSTEM_PROMPT`] (GOV-007; not a second planner).
pub const LIVE_FIRST_HOP_PREAMBLE: &str = "\
Start this VelaClaw turn now. Reply with ONLY one JSON object, no markdown.\n\
Prior user/assistant messages are this same session. Continue that work (same hosts, paths, and findings). Do not claim you forgot earlier turns. Do not ask what to do if the session already did it.\n\
Greeting, thanks, or general knowledge (no repo/host/files): {\"path\":\"chat_only\",\"reply\":\"<full user-visible reply>\"}\n\
Any turn that needs tools: a linear DAG JSON object using the planner rules below (schema_version 0.1.0, 1 to 8 nodes). That object is the plan — do not send a mode label without the graph.\n\
Never use chat_only when the user asks to inspect a local repo, workspace, or host. Do not use path single_work.\n";

#[must_use]
pub fn live_first_hop_system_prompt() -> String {
    format!("{LIVE_FIRST_HOP_PREAMBLE}\n{DAG_PLAN_SYSTEM_PROMPT}")
}

const GRAPH_USER_TASK_MAX_CHARS: usize = 4000;

/// Skip session Envelope only for Plan-phase preview chrome (VL-NA-019).
/// Live DAG work turns still run VL-CTX-001 `prepare_turn_history` first.
#[must_use]
pub fn skip_session_prepare_for_live(bounded_dag_live: bool, host_phase: HostPhase) -> bool {
    bounded_dag_live && host_phase == HostPhase::Plan
}

/// Hop system first; session user/assistant after CTX prepare; skip product system.
#[must_use]
pub fn first_hop_chat_messages(
    system: &str,
    history: &[ChatMessage],
    user_task: &str,
) -> Vec<ChatMessage> {
    let mut out = Vec::with_capacity(history.len().saturating_add(2));
    out.push(ChatMessage::system(system));
    let mut saw_user = false;
    for message in history {
        if message.role == "system" {
            continue;
        }
        if message.role == "user" {
            saw_user = true;
        }
        out.push(message.clone());
    }
    if !saw_user {
        let trimmed = user_task.trim();
        if !trimmed.is_empty() {
            out.push(ChatMessage::user(trimmed));
        }
    }
    out
}

/// Keep the original graph task when a follow-up continues the same session.
#[must_use]
pub fn merge_graph_user_task(previous: Option<&str>, current: &str) -> String {
    let current = current.trim();
    let Some(prev) = previous.map(str::trim).filter(|s| !s.is_empty()) else {
        return current.to_string();
    };
    if prev == current {
        return current.to_string();
    }
    if current.is_empty() {
        return prev.to_string();
    }
    let merged = format!("{prev}\n\nFollow-up:\n{current}");
    if merged.chars().count() <= GRAPH_USER_TASK_MAX_CHARS {
        return merged;
    }
    let follow = format!("\n\nFollow-up:\n{current}");
    let budget = GRAPH_USER_TASK_MAX_CHARS.saturating_sub(follow.chars().count());
    let head: String = prev.chars().take(budget).collect();
    format!("{head}{follow}")
}

pub fn split_one_node_system_prompt() -> String {
    format!(
        "You already chose a 1-node work graph. Reply with ONLY a linear DAG JSON object using the planner rules below, no markdown.\n\
If USER TASK has more than one independently verifiable deliverable, emit 2 to 8 nodes, one per deliverable.\n\
If the task is one atomic result, a 1-node graph is allowed.\n\
Do not use path or mode labels. Do not use chat_only.\n\n{DAG_PLAN_SYSTEM_PROMPT}"
    )
}

pub const TURN_OBSERVE_SYSTEM_PROMPT: &str = "\
You judge whether the last assistant work advanced the USER TASK. Reply with ONLY one JSON object.\n\
{\"verdict\":\"continue\"} — on track; remaining DAG nodes must run when remaining_nodes > 0.\n\
{\"verdict\":\"replan_remaining\"} — last output lacked required evidence or went off-goal. Remaining work should be replanned.\n\
{\"verdict\":\"stop\"} — the user task is done. Use stop only when remaining_nodes is 0 (or omitted). If remaining_nodes > 0 you MUST NOT use stop.\n\
node_count is the live graph size (0 if this turn is chat_only). If remaining_nodes > 0 you MUST NOT use stop.\n\
This observe is mid-graph only. Do not choose replan_remaining to invent nodes after the last hop; the host skips observe when remaining_nodes is 0.\n\
If the user task requires local/repo/host evidence and the assistant reply has no tool or observation evidence, you MUST choose replan_remaining.\n";

const OBSERVE_REPLY_CHARS: usize = 4000;
const OBSERVE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// First live hop: the JSON already contains the greeting or the DAG.
#[derive(Debug)]
pub enum LiveFirstHop {
    ChatOnly { reply: String },
    SingleWork,
    Plan(PlannedLiveDag),
}

impl LiveFirstHop {
    #[must_use]
    pub fn is_plan(&self) -> bool {
        matches!(self, Self::Plan(_))
    }
}

/// Parse first-hop JSON. Invalid / empty chat_only / `single_work` →
/// [`LiveFirstHop::SingleWork`] (host maps that to a 1-node DAG, not a second tool loop).
#[must_use]
pub fn parse_live_first_hop(text: &str, fallback_json: &str) -> LiveFirstHop {
    let Some(obj) = extract_json_object(text) else {
        return LiveFirstHop::SingleWork;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&obj) else {
        return LiveFirstHop::SingleWork;
    };
    let path = v
        .get("path")
        .or_else(|| v.get("mode"))
        .and_then(|m| m.as_str());
    match path {
        Some("chat_only") => {
            let reply = v.get("reply").and_then(|m| m.as_str()).unwrap_or("").trim();
            if reply.is_empty() {
                LiveFirstHop::SingleWork
            } else {
                LiveFirstHop::ChatOnly {
                    reply: reply.to_string(),
                }
            }
        }
        Some("single_work") => LiveFirstHop::SingleWork,
        _ => match resolve_planned_manifest(&obj, fallback_json) {
            Ok(plan) if !plan.used_fallback => LiveFirstHop::Plan(plan),
            _ => LiveFirstHop::SingleWork,
        },
    }
}

/// Parse observe JSON. Invalid → `fail_closed`.
#[must_use]
pub fn parse_observe_verdict(text: &str, fail_closed: ObserveVerdict) -> ObserveVerdict {
    let Some(obj) = extract_json_object(text) else {
        return fail_closed;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&obj) else {
        return fail_closed;
    };
    match v.get("verdict").and_then(|m| m.as_str()) {
        Some("continue") => ObserveVerdict::Continue,
        Some("replan_remaining") => ObserveVerdict::ReplanRemaining,
        Some("stop") => ObserveVerdict::Stop,
        _ => fail_closed,
    }
}

async fn structured_json_chat(
    provider: &dyn Provider,
    model: &str,
    temperature: f64,
    system: &str,
    user: &str,
) -> Result<String> {
    structured_json_chat_messages(
        provider,
        model,
        temperature,
        &[ChatMessage::system(system), ChatMessage::user(user)],
    )
    .await
}

async fn structured_json_chat_messages(
    provider: &dyn Provider,
    model: &str,
    temperature: f64,
    messages: &[ChatMessage],
) -> Result<String> {
    let request = ChatRequest {
        messages,
        tools: None,
    };
    let response = provider.chat(request, model, temperature).await?;
    Ok(response.text_or_empty().to_string())
}

/// Synthetic linear DAG for a tool turn that did not emit a graph.
#[must_use]
pub fn one_node_live_dag(user_task: &str) -> PlannedLiveDag {
    let description: String = user_task.chars().take(200).collect();
    let json = serde_json::json!({
        "schema_version": "0.1.0",
        "id": "one-node",
        "description": description,
        "entry": "work",
        "max_steps": 8,
        "nodes": [{
            "id": "work",
            "task_type": "execute",
            "model_selector": { "capabilities": ["tool_calling"] },
            "context_requirements": { "layers": [0, 1] },
            "next": serde_json::Value::Null
        }]
    })
    .to_string();
    let dag = parse_dag_json(&json).expect("one-node fixture is valid");
    let order = linear_node_ids(&dag).expect("one-node fixture is linear");
    PlannedLiveDag {
        dag,
        order,
        used_fallback: false,
        source: "one_node",
        resume_from: 0,
        graph_task_override: None,
    }
}

fn hop_kind(hop: &LiveFirstHop) -> &'static str {
    match hop {
        LiveFirstHop::ChatOnly { .. } => "chat_only",
        LiveFirstHop::SingleWork => "single_work",
        LiveFirstHop::Plan(plan) if plan.source == "split_refine" => "refined",
        LiveFirstHop::Plan(plan) if plan.order.len() == 1 => "one_node",
        LiveFirstHop::Plan(_) => "dag",
    }
}

/// One extra structured hop when the first graph has a single work node.
async fn refine_one_node_plan(
    provider: &dyn Provider,
    planner_model: &str,
    temperature: f64,
    user_task: &str,
    seed: PlannedLiveDag,
) -> Result<PlannedLiveDag> {
    let json = planned_store_json(&seed, CODE_FIX_TEMPLATE_JSON);
    let user = format!("USER TASK:\n{user_task}\n\nCurrent 1-node graph:\n{json}\n");
    let text = structured_json_chat(
        provider,
        planner_model,
        temperature,
        &split_one_node_system_prompt(),
        &user,
    )
    .await?;
    match parse_live_first_hop(&text, CODE_FIX_TEMPLATE_JSON) {
        LiveFirstHop::Plan(mut plan)
            if !plan.used_fallback && (1..=8).contains(&plan.order.len()) =>
        {
            if plan.order.len() >= 2 {
                plan.source = "split_refine";
                Ok(plan)
            } else {
                Ok(seed)
            }
        }
        _ => Ok(seed),
    }
}

pub async fn live_first_hop(
    agent: &crate::config::AgentConfig,
    mem: &dyn Memory,
    session_id: &str,
    provider: &dyn Provider,
    planner_model: &str,
    user_task: &str,
    history: &[ChatMessage],
    temperature: f64,
    host_phase: HostPhase,
) -> Result<LiveFirstHop> {
    if !agent.bounded_dag_live {
        return Ok(LiveFirstHop::SingleWork);
    }
    if load_dag_fail(mem, session_id)
        .await
        .ok()
        .flatten()
        .is_some()
        || host_phase == HostPhase::Plan
        || operator_fixed_live_graph(agent)?.is_some()
    {
        let planned = prepare_session_live_dag(
            agent,
            mem,
            session_id,
            provider,
            planner_model,
            user_task,
            temperature,
            host_phase,
        )
        .await?;
        return Ok(LiveFirstHop::Plan(planned));
    }
    let fallback = fallback_template_json(agent)?;
    let hop_messages = first_hop_chat_messages(&live_first_hop_system_prompt(), history, user_task);
    let text =
        structured_json_chat_messages(provider, planner_model, temperature, &hop_messages).await?;
    let hop = parse_live_first_hop(&text, &fallback);
    let hop = match hop {
        LiveFirstHop::ChatOnly { reply } => LiveFirstHop::ChatOnly { reply },
        LiveFirstHop::Plan(plan) if plan.order.len() >= 2 => LiveFirstHop::Plan(plan),
        LiveFirstHop::Plan(plan) => LiveFirstHop::Plan(
            refine_one_node_plan(provider, planner_model, temperature, user_task, plan).await?,
        ),
        LiveFirstHop::SingleWork => {
            let seed = one_node_live_dag(user_task);
            LiveFirstHop::Plan(
                refine_one_node_plan(provider, planner_model, temperature, user_task, seed).await?,
            )
        }
    };
    if let LiveFirstHop::Plan(plan) = &hop {
        let json = planned_store_json(plan, &fallback);
        let _ = store_planned_json(mem, session_id, &json).await;
        let previous = load_graph_user_task(mem, session_id).await.ok().flatten();
        let merged = merge_graph_user_task(previous.as_deref(), user_task);
        let _ = store_graph_user_task(mem, session_id, &merged).await;
    }
    tracing::info!(
        target: "bounded_dag_live",
        kind = hop_kind(&hop),
        plan = hop.is_plan(),
        nodes = match &hop {
            LiveFirstHop::Plan(plan) => plan.order.len(),
            _ => 0,
        },
        "first hop"
    );
    Ok(hop)
}

#[allow(clippy::too_many_arguments)]
pub async fn observe_turn_outcome(
    provider: &dyn Provider,
    planner_model: &str,
    temperature: f64,
    user_task: &str,
    last_reply: &str,
    node_id: Option<&str>,
    remaining_nodes: usize,
    node_count: usize,
    fail_closed: ObserveVerdict,
) -> Result<ObserveVerdict> {
    observe_turn_outcome_timed(
        provider,
        planner_model,
        temperature,
        user_task,
        last_reply,
        node_id,
        remaining_nodes,
        node_count,
        fail_closed,
        OBSERVE_TIMEOUT,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn observe_turn_outcome_timed(
    provider: &dyn Provider,
    planner_model: &str,
    temperature: f64,
    user_task: &str,
    last_reply: &str,
    node_id: Option<&str>,
    remaining_nodes: usize,
    node_count: usize,
    fail_closed: ObserveVerdict,
    timeout: std::time::Duration,
) -> Result<ObserveVerdict> {
    let node = node_id.unwrap_or("(none)");
    tracing::info!(
        target: "bounded_dag_live",
        node_id = node,
        remaining_nodes,
        node_count,
        "turn observe start"
    );
    match tokio::time::timeout(
        timeout,
        observe_turn_outcome_inner(
            provider,
            planner_model,
            temperature,
            user_task,
            last_reply,
            node_id,
            remaining_nodes,
            node_count,
            fail_closed,
        ),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => {
            let exhausted =
                velaclaw_agent_runtime::looks_like_tool_format_exhausted_notice(last_reply);
            tracing::warn!(
                target: "bounded_dag_live",
                node_id = node,
                remaining_nodes,
                node_count,
                exhausted,
                "observe timed out"
            );
            if exhausted {
                Ok(ObserveVerdict::Stop)
            } else {
                Ok(fail_closed)
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn observe_turn_outcome_inner(
    provider: &dyn Provider,
    planner_model: &str,
    temperature: f64,
    user_task: &str,
    last_reply: &str,
    node_id: Option<&str>,
    remaining_nodes: usize,
    node_count: usize,
    fail_closed: ObserveVerdict,
) -> Result<ObserveVerdict> {
    let reply = if last_reply.chars().count() > OBSERVE_REPLY_CHARS {
        let clipped: String = last_reply.chars().take(OBSERVE_REPLY_CHARS).collect();
        format!("{clipped}\n…[truncated]")
    } else {
        last_reply.to_string()
    };
    let node = node_id.unwrap_or("(none)");
    let user = format!(
        "USER TASK:\n{user_task}\n\nLast node id: {node}\nremaining_nodes: {remaining_nodes}\nnode_count: {node_count}\n\nAssistant reply:\n{reply}"
    );
    let text = structured_json_chat(
        provider,
        planner_model,
        temperature,
        TURN_OBSERVE_SYSTEM_PROMPT,
        &user,
    )
    .await?;
    let parsed = parse_observe_verdict(&text, fail_closed);
    let verdict = coerce_observe_remaining(parsed, remaining_nodes, node_count);
    tracing::info!(
        target: "bounded_dag_live",
        node_id = node,
        remaining_nodes,
        node_count,
        parsed = ?parsed,
        verdict = ?verdict,
        "turn observe"
    );
    Ok(verdict)
}

/// Stop is only valid when no DAG nodes remain. Mid-graph Stop is Continue.
fn coerce_observe_remaining(
    verdict: ObserveVerdict,
    remaining_nodes: usize,
    node_count: usize,
) -> ObserveVerdict {
    if remaining_nodes > 0 && verdict == ObserveVerdict::Stop {
        tracing::info!(
            target: "bounded_dag_live",
            remaining_nodes,
            "observe stop coerced to continue"
        );
        ObserveVerdict::Continue
    } else if node_count > 0 && remaining_nodes == 0 && verdict == ObserveVerdict::ReplanRemaining {
        tracing::info!(
            target: "bounded_dag_live",
            node_count,
            "observe replan_remaining coerced to stop on last hop"
        );
        ObserveVerdict::Stop
    } else {
        verdict
    }
}

/// Replan remaining nodes after observe (prefix includes the completed node).
#[allow(clippy::too_many_arguments)]
pub async fn replan_remaining_after_observe(
    agent: &crate::config::AgentConfig,
    mem: &dyn Memory,
    session_id: &str,
    provider: &dyn Provider,
    planner_model: &str,
    temperature: f64,
    stored: &PlannedLiveDag,
    completed_node_id: &str,
    completed_index: usize,
    user_task: &str,
    observe_note: &str,
) -> Result<Option<PlannedLiveDag>> {
    let original = load_graph_user_task(mem, session_id)
        .await?
        .unwrap_or_else(|| user_task.to_string());
    let fail = DagFailCursor {
        node_id: completed_node_id.to_string(),
        index: completed_index.saturating_add(1),
        err: observe_note.to_string(),
        dag_id: stored.dag.id.clone(),
        auto_replan_count: 1,
        fail_class: "observe_off_goal".into(),
    };
    let completed: Vec<String> = stored.order.iter().take(fail.index).cloned().collect();
    let repair_user = repair_planner_user_prompt(&original, &fail, &completed, observe_note);
    let fallback = fallback_template_json(agent)?;
    let remaining = run_live_planner_chat(
        provider,
        planner_model,
        &repair_user,
        temperature,
        &fallback,
    )
    .await?;
    if remaining.used_fallback {
        tracing::info!(
            target: "bounded_dag_live",
            "observe replan used fallback; keeping remaining nodes"
        );
        return Ok(None);
    }
    let mut spliced = splice_remaining_plan(stored, &fail, remaining);
    if linear_node_ids(&spliced.dag).is_err() {
        return Ok(None);
    }
    spliced.graph_task_override = Some(repair_graph_task(
        &original,
        completed_node_id,
        observe_note,
    ));
    let json = planned_store_json(&spliced, &fallback);
    let _ = store_planned_json(mem, session_id, &json).await;
    Ok(Some(spliced))
}

/// Cursor so the next user message can replan the failed node (not a Build approval).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct DagFailCursor {
    pub node_id: String,
    pub index: usize,
    pub err: String,
    pub dag_id: String,
    #[serde(default)]
    pub auto_replan_count: u32,
    #[serde(default)]
    pub fail_class: String,
}

/// Same-turn retry vs stop (VL-NA-024). Dist default off via config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkNodeFailDecision {
    RetrySame { force_default: bool },
    Stop,
}

#[must_use]
pub fn decide_work_node_fail(
    auto_enabled: bool,
    auto_used: bool,
    err: &str,
) -> WorkNodeFailDecision {
    if !auto_enabled || auto_used {
        return WorkNodeFailDecision::Stop;
    }
    match crate::providers::hint_peer::classify_hop_error(err) {
        crate::providers::hint_peer::HopFailClass::Unavailable
        | crate::providers::hint_peer::HopFailClass::Quota => WorkNodeFailDecision::RetrySame {
            force_default: true,
        },
        crate::providers::hint_peer::HopFailClass::Policy
        | crate::providers::hint_peer::HopFailClass::Other => WorkNodeFailDecision::RetrySame {
            force_default: false,
        },
        crate::providers::hint_peer::HopFailClass::Transport => WorkNodeFailDecision::Stop,
    }
}

/// Persist the original user task for work-node USER TASK slots.
pub async fn store_graph_user_task(
    mem: &dyn Memory,
    session_id: &str,
    user_task: &str,
) -> Result<()> {
    let trimmed = user_task.trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    mem.store(
        &graph_user_key(session_id),
        trimmed,
        crate::memory::MemoryCategory::Daily,
        Some(session_id),
    )
    .await
}

pub async fn load_graph_user_task(mem: &dyn Memory, session_id: &str) -> Result<Option<String>> {
    Ok(mem
        .get(&graph_user_key(session_id))
        .await?
        .map(|e| e.content))
}

/// Prefer stored original task, else last user in history.
pub fn user_task_from_history(history: &[ChatMessage], current: &str) -> String {
    let current = current.trim();
    if !current.is_empty() {
        return current.to_string();
    }
    for message in history.iter().rev() {
        if message.role != "user" {
            continue;
        }
        let body = message.content.trim();
        if body.is_empty() || body.starts_with("[dag_artifact") || body.starts_with("USER TASK") {
            continue;
        }
        return body.to_string();
    }
    current.to_string()
}

/// Stored original task, else last user in history.
pub async fn work_node_user_task(
    mem: &dyn Memory,
    session_id: &str,
    history: &[ChatMessage],
    current: &str,
) -> String {
    if let Ok(Some(stored)) = load_graph_user_task(mem, session_id).await {
        let trimmed = stored.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    user_task_from_history(history, current)
}

pub async fn store_dag_fail(
    mem: &dyn Memory,
    session_id: &str,
    cursor: &DagFailCursor,
) -> Result<()> {
    let json = serde_json::to_string(cursor)?;
    mem.store(
        &dag_fail_key(session_id),
        &json,
        crate::memory::MemoryCategory::Daily,
        Some(session_id),
    )
    .await
}

pub async fn load_dag_fail(mem: &dyn Memory, session_id: &str) -> Result<Option<DagFailCursor>> {
    let Some(entry) = mem.get(&dag_fail_key(session_id)).await? else {
        return Ok(None);
    };
    Ok(serde_json::from_str(&entry.content).ok())
}

pub async fn clear_dag_fail(mem: &dyn Memory, session_id: &str) -> Result<()> {
    let _ = mem.forget(&dag_fail_key(session_id)).await;
    Ok(())
}

/// Drop cached plan, fail cursor, and node artifacts for this session.
pub async fn clear_session_dag_state(mem: &dyn Memory, session_id: &str) -> Result<()> {
    let plan_key = planned_dag_key(session_id);
    let user_key = graph_user_key(session_id);
    let fail_key = dag_fail_key(session_id);
    let _ = mem.forget(&plan_key).await;
    let _ = mem.forget(&user_key).await;
    let _ = mem.forget(&fail_key).await;
    let prefix = format!("dag_art:{session_id}:");
    let listed = mem
        .list(Some(&MemoryCategory::Daily), Some(session_id))
        .await
        .unwrap_or_default();
    for (i, entry) in listed.into_iter().enumerate() {
        if i >= MAX_SESSION_DAG_FORGET {
            break;
        }
        if entry.key == plan_key
            || entry.key == user_key
            || entry.key == fail_key
            || entry.key.starts_with(&prefix)
        {
            let _ = mem.forget(&entry.key).await;
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct PlannedLiveDag {
    pub dag: DagManifest,
    pub order: Vec<String>,
    pub used_fallback: bool,
    pub source: &'static str,
    /// Skip already-finished nodes when retrying after a fail cursor.
    pub resume_from: usize,
    /// Original task plus user guidance for the failed node.
    pub graph_task_override: Option<String>,
}

impl PlannedLiveDag {
    pub fn preview_text(&self) -> String {
        self.preview_with_contact("", &[])
    }

    /// Short operator-facing step list (chat), not the debug Plan chrome dump.
    pub fn brief_outline(&self, user_message: &str) -> String {
        brief_dag_outline(
            user_message,
            &self.dag,
            &self.order,
            self.used_fallback,
            self.resume_from,
        )
    }
}

/// Prefer CJK copy when the user prompt contains Han characters.
#[must_use]
pub fn user_prefers_cjk(text: &str) -> bool {
    text.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
}

#[must_use]
pub fn prettify_node_id(id: &str) -> String {
    id.replace(['_', '-'], " ")
}

/// Chat-facing outline: numbered steps in the user's script, no Contact dump.
#[must_use]
pub fn brief_dag_outline(
    user_message: &str,
    dag: &crate::agent::dag_runner::DagManifest,
    order: &[String],
    used_fallback: bool,
    resume_from: usize,
) -> String {
    let cjk = user_prefers_cjk(user_message);
    let by_id: std::collections::HashMap<&str, _> =
        dag.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    let mut lines = Vec::new();
    if cjk {
        if used_fallback {
            lines.push("规划未能得到有效步骤，改用手写回退图。".to_string());
        }
        if resume_from > 0 {
            lines.push(format!(
                "按你的新说明，从第 {} 步起重新规划并继续（共 {} 步）：",
                resume_from + 1,
                order.len()
            ));
        } else {
            lines.push(format!("将分 {} 步处理：", order.len()));
        }
    } else {
        if used_fallback {
            lines.push("Planner did not produce a valid step list; using a fallback graph.".into());
        }
        if resume_from > 0 {
            lines.push(format!(
                "Replanning from step {} of {} with your new guidance:",
                resume_from + 1,
                order.len()
            ));
        } else {
            lines.push(format!("Working in {} step(s):", order.len()));
        }
    }
    for (i, id) in order.iter().enumerate() {
        let label = prettify_node_id(id);
        let task = by_id
            .get(id.as_str())
            .map(|n| n.task_type.as_str())
            .unwrap_or("-");
        if cjk {
            lines.push(format!("{}. {}（{}）", i + 1, label, task));
        } else {
            lines.push(format!("{}. {label} ({task})", i + 1));
        }
    }
    if cjk {
        lines.push("开始执行。".into());
    } else {
        lines.push("Starting now.".into());
    }
    lines.join("\n")
}

/// One-line operator gist after the live plan is accepted (VL-NA-037). Not a second planner.
#[must_use]
pub fn operator_plan_gist(
    user_message: &str,
    dag: &crate::agent::dag_runner::DagManifest,
    order: &[String],
    used_fallback: bool,
) -> String {
    let cjk = user_prefers_cjk(user_message);
    let by_id: std::collections::HashMap<&str, _> =
        dag.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    let hops: Vec<String> = order
        .iter()
        .map(|id| {
            by_id
                .get(id.as_str())
                .and_then(|n| {
                    n.artifact
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(ToOwned::to_owned)
                })
                .unwrap_or_else(|| prettify_node_id(id))
        })
        .collect();
    let list = hops.join(" → ");
    if cjk {
        if used_fallback {
            format!("改用回退图，将按 {} 步：{}。", order.len(), list)
        } else {
            format!("将按 {} 步：{}。", order.len(), list)
        }
    } else if used_fallback {
        format!("Using a fallback graph in {} step(s): {list}.", order.len())
    } else {
        format!("Working in {} step(s): {list}.", order.len())
    }
}

/// Persistable stop line when a work node fails (does not dump the graph).
#[must_use]
pub fn format_work_node_stop(
    user_message: &str,
    node_id: &str,
    err: &str,
    completed: usize,
    total: usize,
) -> String {
    let pretty = prettify_node_id(node_id);
    let err = err.trim();
    if user_prefers_cjk(user_message) {
        format!(
            "已在第 {completed}/{total} 步停住（`{pretty}`）。\n{err}\n针对这一步发送新说明，即可重新规划该步并继续。"
        )
    } else {
        format!(
            "Stopped at step {completed}/{total} (`{pretty}`).\n{err}\nSend guidance for this step to replan it and continue."
        )
    }
}

/// Rail snapshot: one row per graph order, with pending/running/ok/error.
#[must_use]
pub fn live_dag_node_rows<S: BuildHasher>(
    dag: &crate::agent::dag_runner::DagManifest,
    order: &[String],
    running: Option<&str>,
    completed: &HashSet<String, S>,
    failed: Option<&str>,
) -> Vec<LiveDagNodeRow> {
    let by_id: std::collections::HashMap<&str, _> =
        dag.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    order
        .iter()
        .map(|id| {
            let node = by_id.get(id.as_str());
            let status = if failed == Some(id.as_str()) {
                "error"
            } else if completed.contains(id) {
                "ok"
            } else if running == Some(id.as_str()) {
                "running"
            } else {
                "pending"
            };
            LiveDagNodeRow {
                id: id.clone(),
                label: prettify_node_id(id),
                task_type: node
                    .map(|n| n.task_type.clone())
                    .unwrap_or_else(|| "-".into()),
                caps: node
                    .map(|n| n.model_selector.capabilities.join(","))
                    .unwrap_or_default(),
                contact: String::new(),
                status,
            }
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct LiveDagNodeRow {
    pub id: String,
    pub label: String,
    pub task_type: String,
    pub caps: String,
    pub contact: String,
    pub status: &'static str,
}

/// Resolved hop labels for the live rail (`RouterProvider` pin after peer fallback).
#[must_use]
pub fn dag_contact_labels(
    provider: &dyn Provider,
    dag: &DagManifest,
    order: &[String],
    session_model: &str,
    hints: &[String],
) -> std::collections::HashMap<String, String> {
    let mut out = HashMap::new();
    let by_id: HashMap<&str, _> = dag.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    for id in order {
        let Some(node) = by_id.get(id.as_str()) else {
            continue;
        };
        let contact = contact_for_node(node, session_model, hints, None);
        out.insert(id.clone(), provider.routed_model_label(&contact.model));
    }
    out
}

/// Structured WS/CLI progress for the live rail.
#[must_use]
pub fn live_dag_progress<S, C>(
    dag_id: &str,
    fallback: bool,
    outline: &str,
    dag: &crate::agent::dag_runner::DagManifest,
    order: &[String],
    running: Option<&str>,
    completed: &HashSet<String, S>,
    failed: Option<&str>,
    contacts: Option<&HashMap<String, String, C>>,
) -> crate::agent::turn_progress::TurnProgress
where
    S: BuildHasher,
    C: BuildHasher,
{
    use crate::agent::turn_progress::{DagNodeProgress, TurnProgress};
    TurnProgress::Dag {
        dag_id: dag_id.to_string(),
        fallback,
        outline: outline.to_string(),
        nodes: live_dag_node_rows(dag, order, running, completed, failed)
            .into_iter()
            .map(|r| {
                let contact = contacts
                    .and_then(|m| m.get(&r.id).cloned())
                    .unwrap_or_default();
                DagNodeProgress {
                    id: r.id,
                    label: r.label,
                    task_type: r.task_type,
                    caps: r.caps,
                    contact,
                    status: r.status.to_string(),
                }
            })
            .collect(),
    }
}

impl PlannedLiveDag {
    /// Plan chrome: graph plus per-node Contact (hint → provider family).
    pub fn preview_with_contact(&self, default_model: &str, available_hints: &[String]) -> String {
        let mut out = String::new();
        if self.used_fallback {
            out.push_str(
                "Planner output was not a valid linear L2 DAG; using handwritten fallback.\n\n",
            );
        } else {
            let _ = write!(out, "Planner accepted linear DAG `{}`.\n\n", self.dag.id);
        }
        out.push_str(&format_preview(&self.dag, &self.order));
        if default_model.is_empty() && available_hints.is_empty() {
            return out;
        }
        out.push_str("\nContact (capability → route; planner stays on session default):\n");
        let by_id: std::collections::HashMap<&str, _> =
            self.dag.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        for id in &self.order {
            let Some(node) = by_id.get(id.as_str()) else {
                continue;
            };
            let contact = contact_for_node(node, default_model, available_hints, None);
            let _ = writeln!(out, "- {}  {}", node.id, contact.observe_line());
        }
        out
    }
}

/// Path override or previously stored plan. `None` means the caller should run the planner node.
pub async fn try_cached_or_fixed_live_dag(
    agent: &crate::config::AgentConfig,
    mem: &dyn Memory,
    session_id: &str,
) -> Result<Option<PlannedLiveDag>> {
    if !agent.bounded_dag_live {
        return Ok(None);
    }
    if let Some((dag, order)) = operator_fixed_live_graph(agent)? {
        return Ok(Some(PlannedLiveDag {
            dag,
            order,
            used_fallback: false,
            source: "operator_path",
            resume_from: 0,
            graph_task_override: None,
        }));
    }
    let fallback = fallback_template_json(agent)?;
    load_stored_planned_dag(mem, session_id, &fallback).await
}

/// Insert planner system prompt immediately before the current user turn.
pub fn wrap_chat_history_for_planner(
    history: &mut Vec<ChatMessage>,
    prompt: &str,
) -> (usize, ChatMessage) {
    let prefix = history.len().saturating_sub(1);
    let user = history.pop().unwrap_or_else(|| ChatMessage::user(""));
    history.push(ChatMessage::system(prompt));
    history.push(user.clone());
    (prefix, user)
}

pub fn restore_chat_history_after_planner(
    history: &mut Vec<ChatMessage>,
    prefix: usize,
    user: ChatMessage,
) {
    history.truncate(prefix);
    history.push(user);
}

/// Cached/operator graph, or run the tool-free planner and persist successful plans only.
///
/// `use_cache`: reuse `dag_plan:<session>` when already loaded for a repair.
/// New tasks pass false after `clear_session_dag_state`.
pub async fn obtain_planned_live_dag_with_provider(
    agent: &crate::config::AgentConfig,
    mem: &dyn Memory,
    session_id: &str,
    provider: &dyn Provider,
    planner_model: &str,
    user_task: &str,
    temperature: f64,
    use_cache: bool,
) -> Result<PlannedLiveDag> {
    if use_cache {
        if let Some(planned) = try_cached_or_fixed_live_dag(agent, mem, session_id).await? {
            return Ok(planned);
        }
    } else if let Some((dag, order)) = operator_fixed_live_graph(agent)? {
        let _ = store_graph_user_task(mem, session_id, user_task).await;
        return Ok(PlannedLiveDag {
            dag,
            order,
            used_fallback: false,
            source: "operator_path",
            resume_from: 0,
            graph_task_override: None,
        });
    }
    let fallback = fallback_template_json(agent)?;
    let planned =
        run_live_planner_chat(provider, planner_model, user_task, temperature, &fallback).await?;
    if planned.used_fallback {
        tracing::info!(
            target: "bounded_dag_live",
            session_id = %session_id,
            "planner used fallback template; not caching for session"
        );
    } else {
        let json = planned_store_json(&planned, &fallback);
        if let Err(err) = store_planned_json(mem, session_id, &json).await {
            tracing::debug!(error = %err, "bounded DAG plan store skipped");
        }
    }
    let _ = store_graph_user_task(mem, session_id, user_task).await;
    Ok(planned)
}

/// Combine original graph task with the user's failed-node guidance.
#[must_use]
pub fn repair_graph_task(original: &str, node_id: &str, guidance: &str) -> String {
    let original = original.trim();
    let guidance = guidance.trim();
    format!("{original}\n\nFailed node `{node_id}` — user guidance for this step:\n{guidance}")
}

#[must_use]
pub fn repair_planner_user_prompt(
    original: &str,
    fail: &DagFailCursor,
    completed: &[String],
    guidance: &str,
) -> String {
    let completed = if completed.is_empty() {
        "(none)".to_string()
    } else {
        completed.join(", ")
    };
    format!(
        "Replan remaining work after a failed node. Reply with ONLY one linear DAG JSON object \
(schema_version 0.1.0). Include 1 to 6 remaining nodes. Do not redo completed nodes.\n\n\
Original user task:\n{original}\n\n\
Graph id: {}\nCompleted node ids: {completed}\n\
Failed node: {} (0-based index {})\nFail class: {}\nFailure:\n{}\n\n\
User guidance for this node (and remaining if needed):\n{guidance}",
        fail.dag_id,
        fail.node_id,
        fail.index,
        if fail.fail_class.is_empty() {
            "unspecified"
        } else {
            fail.fail_class.as_str()
        },
        fail.err.trim()
    )
}

/// Prefix completed nodes from `stored`, append remaining from `repair`.
#[must_use]
pub fn splice_remaining_plan(
    stored: &PlannedLiveDag,
    fail: &DagFailCursor,
    remaining: PlannedLiveDag,
) -> PlannedLiveDag {
    let resume_from = fail.index.min(stored.order.len());
    let prefix: Vec<String> = stored.order.iter().take(resume_from).cloned().collect();
    let prefix_set: std::collections::HashSet<&str> = prefix.iter().map(String::as_str).collect();
    let mut nodes: Vec<_> = stored
        .dag
        .nodes
        .iter()
        .filter(|n| prefix_set.contains(n.id.as_str()))
        .cloned()
        .collect();
    let mut rest_order: Vec<String> = Vec::new();
    for id in &remaining.order {
        if prefix_set.contains(id.as_str()) {
            continue;
        }
        let Some(node) = remaining.dag.nodes.iter().find(|n| n.id == *id) else {
            continue;
        };
        rest_order.push(id.clone());
        nodes.push(node.clone());
    }
    if rest_order.is_empty() {
        rest_order = stored.order.iter().skip(resume_from).cloned().collect();
        for id in &rest_order {
            if nodes.iter().any(|n| n.id == *id) {
                continue;
            }
            if let Some(node) = stored.dag.nodes.iter().find(|n| n.id == *id) {
                nodes.push(node.clone());
            }
        }
    }
    if let (Some(last_prefix), Some(first_rest)) = (prefix.last(), rest_order.first()) {
        for node in &mut nodes {
            if node.id == *last_prefix {
                node.next = Some(first_rest.clone());
            }
        }
    }
    for i in 0..rest_order.len() {
        let nxt = rest_order.get(i + 1).cloned();
        if let Some(n) = nodes.iter_mut().find(|n| n.id == rest_order[i]) {
            n.next = nxt;
        }
    }
    let order: Vec<String> = prefix
        .iter()
        .cloned()
        .chain(rest_order.iter().cloned())
        .collect();
    let mut dag = stored.dag.clone();
    dag.nodes = nodes;
    if !prefix.is_empty() {
        dag.entry = stored.dag.entry.clone();
    } else if let Some(first) = order.first() {
        dag.entry = first.clone();
    }
    PlannedLiveDag {
        dag,
        order,
        used_fallback: remaining.used_fallback,
        source: "repair",
        resume_from,
        graph_task_override: remaining.graph_task_override,
    }
}

/// New task → full plan. Failed node + new prompt → replan remaining from that node.
pub async fn prepare_session_live_dag(
    agent: &crate::config::AgentConfig,
    mem: &dyn Memory,
    session_id: &str,
    provider: &dyn Provider,
    planner_model: &str,
    user_task: &str,
    temperature: f64,
    host_phase: HostPhase,
) -> Result<PlannedLiveDag> {
    if host_phase == HostPhase::Plan {
        clear_session_dag_state(mem, session_id).await?;
        return obtain_planned_live_dag_with_provider(
            agent,
            mem,
            session_id,
            provider,
            planner_model,
            user_task,
            temperature,
            false,
        )
        .await;
    }

    if let Some(fail) = load_dag_fail(mem, session_id).await? {
        if let Some(mut stored) = try_cached_or_fixed_live_dag(agent, mem, session_id).await? {
            let original = load_graph_user_task(mem, session_id)
                .await?
                .unwrap_or_else(|| user_task.to_string());
            let completed: Vec<String> = stored.order.iter().take(fail.index).cloned().collect();
            let repair_user = repair_planner_user_prompt(&original, &fail, &completed, user_task);
            let fallback = fallback_template_json(agent)?;
            let remaining = run_live_planner_chat(
                provider,
                planner_model,
                &repair_user,
                temperature,
                &fallback,
            )
            .await?;
            let override_task = repair_graph_task(&original, &fail.node_id, user_task);
            if remaining.used_fallback {
                stored.resume_from = fail.index.min(stored.order.len());
                stored.graph_task_override = Some(override_task);
                stored.source = "repair_keep";
                return Ok(stored);
            }
            let mut spliced = splice_remaining_plan(&stored, &fail, remaining);
            if linear_node_ids(&spliced.dag).is_err() {
                stored.resume_from = fail.index.min(stored.order.len());
                stored.graph_task_override = Some(override_task);
                stored.source = "repair_keep";
                return Ok(stored);
            }
            spliced.graph_task_override = Some(override_task);
            let json = planned_store_json(&spliced, &fallback);
            let _ = store_planned_json(mem, session_id, &json).await;
            return Ok(spliced);
        }
        let _ = clear_dag_fail(mem, session_id).await;
    }

    clear_session_dag_state(mem, session_id).await?;
    obtain_planned_live_dag_with_provider(
        agent,
        mem,
        session_id,
        provider,
        planner_model,
        user_task,
        temperature,
        false,
    )
    .await
}

/// Tool-free planner chat with one validation retry before L2 fallback.
pub async fn run_live_planner_chat(
    provider: &dyn Provider,
    planner_model: &str,
    user_task: &str,
    temperature: f64,
    fallback_json: &str,
) -> Result<PlannedLiveDag> {
    let text = planner_chat_text(provider, planner_model, user_task, temperature).await?;
    let planned = resolve_planned_manifest(&text, fallback_json)?;
    if !planned.used_fallback {
        return Ok(planned);
    }

    let extracted = extract_json_object(&text).unwrap_or_else(|| text.trim().to_string());
    let report = validate_candidate_dag_json(&extracted);
    tracing::info!(
        target: "bounded_dag_live",
        category = %report.category,
        message = %report.message,
        "planner output invalid; retrying once"
    );

    let retry_user = format!(
        "Your previous reply failed validation ({}: {}). Reply with ONLY one corrected JSON object — no markdown fences, no prose.\n\nOriginal user task:\n{user_task}",
        report.category, report.message
    );
    let messages = [
        ChatMessage::system(DAG_PLAN_SYSTEM_PROMPT),
        ChatMessage::user(retry_user),
    ];
    let request = ChatRequest {
        messages: &messages,
        tools: None,
    };
    let response = provider.chat(request, planner_model, temperature).await?;
    let retry_text = response.text_or_empty();
    resolve_planned_manifest(retry_text, fallback_json)
}

pub fn operator_fixed_live_graph(
    agent: &crate::config::AgentConfig,
) -> Result<Option<(DagManifest, Vec<String>)>> {
    if !agent.bounded_dag_live {
        return Ok(None);
    }
    let Some(raw) = agent
        .bounded_dag_path
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Ok(None);
    };
    let dag = load_bounded_dag(Some(Path::new(raw)))?;
    let order = linear_node_ids(&dag)?;
    Ok(Some((dag, order)))
}

pub fn fallback_template_json(agent: &crate::config::AgentConfig) -> Result<String> {
    if let Some(raw) = agent
        .bounded_dag_path
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Ok(std::fs::read_to_string(raw)?);
    }
    Ok(CODE_FIX_TEMPLATE_JSON.to_string())
}

/// Parse planner model text into a linear DAG, or the L2 fallback template.
pub fn resolve_planned_manifest(planner_text: &str, fallback_json: &str) -> Result<PlannedLiveDag> {
    let extracted =
        extract_json_object(planner_text).unwrap_or_else(|| planner_text.trim().to_string());
    let report = validate_candidate_dag_json(&extracted);
    if report.valid {
        if let Some(dag) = report.dag {
            if let Ok(order) = linear_node_ids(&dag) {
                return Ok(PlannedLiveDag {
                    dag,
                    order,
                    used_fallback: false,
                    source: "planner",
                    resume_from: 0,
                    graph_task_override: None,
                });
            }
        }
    }
    let dag = super::dag_runner::parse_dag_json(fallback_json)?;
    let order = linear_node_ids(&dag)?;
    Ok(PlannedLiveDag {
        dag,
        order,
        used_fallback: true,
        source: "fallback_template",
        resume_from: 0,
        graph_task_override: None,
    })
}

pub async fn store_planned_json(mem: &dyn Memory, session_id: &str, json: &str) -> Result<()> {
    mem.store(
        &planned_dag_key(session_id),
        json,
        crate::memory::MemoryCategory::Daily,
        Some(session_id),
    )
    .await
}

pub async fn load_planned_json(mem: &dyn Memory, session_id: &str) -> Result<Option<String>> {
    Ok(mem
        .get(&planned_dag_key(session_id))
        .await?
        .map(|e| e.content))
}

/// Persist whichever graph we will execute (planner JSON or fallback file text).
pub fn planned_store_json(plan: &PlannedLiveDag, fallback_json: &str) -> String {
    if plan.used_fallback {
        fallback_json.to_string()
    } else {
        // Round-trip via parse-able copy from nodes we already validated.
        serde_json::json!({
            "schema_version": plan.dag.schema_version,
            "id": plan.dag.id,
            "description": plan.dag.description,
            "entry": plan.dag.entry,
            "max_steps": plan.dag.max_steps,
            "timeout_secs": plan.dag.timeout_secs,
            "nodes": plan.dag.nodes.iter().map(|n| {
                serde_json::json!({
                    "id": n.id,
                    "task_type": n.task_type,
                    "model_selector": { "capabilities": n.model_selector.capabilities },
                    "context_requirements": {
                        "layers": n.context_requirements.layers,
                        "retrieve": n.context_requirements.retrieve.iter().map(|r| {
                            serde_json::json!({
                                "kind": r.kind,
                                "query": r.query,
                                "alias": r.alias,
                            })
                        }).collect::<Vec<_>>(),
                    },
                    "max_steps": n.max_steps,
                    "next": n.next,
                })
            }).collect::<Vec<_>>(),
        })
        .to_string()
    }
}

pub async fn load_stored_planned_dag(
    mem: &dyn Memory,
    session_id: &str,
    fallback_json: &str,
) -> Result<Option<PlannedLiveDag>> {
    let Some(json) = load_planned_json(mem, session_id).await? else {
        return Ok(None);
    };
    Ok(Some(resolve_planned_manifest(&json, fallback_json)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAPER_JSON: &str = r#"{
      "schema_version": "0.1.0",
      "id": "paper-slides",
      "entry": "read",
      "max_steps": 8,
      "nodes": [
        {"id":"read","task_type":"summarize","model_selector":{"capabilities":["document_understanding"]},"next":"slides"},
        {"id":"slides","task_type":"write","model_selector":{"capabilities":["speed"]},"next":null}
      ]
    }"#;

    #[test]
    fn valid_linear_planner_json_is_accepted() {
        let plan = resolve_planned_manifest(PAPER_JSON, CODE_FIX_TEMPLATE_JSON).unwrap();
        assert!(!plan.used_fallback);
        assert_eq!(plan.order, vec!["read", "slides"]);
        assert_eq!(plan.dag.id, "paper-slides");
    }

    #[test]
    fn garbage_falls_back_to_code_fix() {
        let plan = resolve_planned_manifest("not json", CODE_FIX_TEMPLATE_JSON).unwrap();
        assert!(plan.used_fallback);
        assert_eq!(plan.order, vec!["locate", "patch", "verify"]);
    }

    #[test]
    fn skip_session_prepare_only_for_plan_phase() {
        assert!(!skip_session_prepare_for_live(true, HostPhase::Build));
        assert!(skip_session_prepare_for_live(true, HostPhase::Plan));
        assert!(!skip_session_prepare_for_live(false, HostPhase::Plan));
    }

    #[test]
    fn first_hop_chat_messages_keep_session_and_skip_product_system() {
        let history = vec![
            ChatMessage::system("product"),
            ChatMessage::user("check gProxy on piubt"),
            ChatMessage::assistant("google-route-review: gProxy is up"),
            ChatMessage::user("你忘了在piubt上"),
        ];
        let msgs = first_hop_chat_messages(&live_first_hop_system_prompt(), &history, "ignored");
        assert_eq!(msgs[0].role, "system");
        assert!(msgs[0].content.contains("same session"));
        assert!(msgs.iter().all(|m| m.content != "product"));
        assert!(msgs
            .iter()
            .any(|m| m.content.contains("google-route-review")));
        assert!(msgs.iter().any(|m| m.content.contains("你忘了在piubt上")));
    }

    #[test]
    fn merge_graph_user_task_keeps_original() {
        let merged = merge_graph_user_task(
            Some("check xray then gProxy on piubt"),
            "你是不是忘了都在piubt上",
        );
        assert!(merged.contains("check xray then gProxy on piubt"));
        assert!(merged.contains("Follow-up:"));
        assert!(merged.contains("piubt"));
        assert_eq!(merge_graph_user_task(None, "hello"), "hello");
        assert_eq!(merge_graph_user_task(Some("same"), "same"), "same");
    }

    #[test]
    fn parse_live_first_hop_in_band_or_single_work() {
        match parse_live_first_hop(
            r#"{"path":"chat_only","reply":"Hi — ready."}"#,
            CODE_FIX_TEMPLATE_JSON,
        ) {
            LiveFirstHop::ChatOnly { reply } => assert!(reply.contains("Hi")),
            other => panic!("expected chat_only, got {other:?}"),
        }
        assert!(matches!(
            parse_live_first_hop(r#"{"path":"chat_only"}"#, CODE_FIX_TEMPLATE_JSON),
            LiveFirstHop::SingleWork
        ));
        assert!(matches!(
            parse_live_first_hop(r#"{"path":"single_work"}"#, CODE_FIX_TEMPLATE_JSON),
            LiveFirstHop::SingleWork
        ));
        assert!(matches!(
            parse_live_first_hop(CODE_FIX_TEMPLATE_JSON, CODE_FIX_TEMPLATE_JSON),
            LiveFirstHop::Plan(_)
        ));
        assert!(matches!(
            parse_live_first_hop(r#"{"mode":"plan_dag"}"#, CODE_FIX_TEMPLATE_JSON),
            LiveFirstHop::SingleWork
        ));
        assert!(matches!(
            parse_live_first_hop("", CODE_FIX_TEMPLATE_JSON),
            LiveFirstHop::SingleWork
        ));
        match parse_live_first_hop(
            r#"{"mode":"chat_only","reply":"Hi — ready."}"#,
            CODE_FIX_TEMPLATE_JSON,
        ) {
            LiveFirstHop::ChatOnly { reply } => assert!(reply.contains("Hi")),
            other => panic!("mode alias should still carry in-band reply, got {other:?}"),
        }
    }

    #[test]
    fn observe_stop_coerced_when_nodes_remain() {
        assert_eq!(
            coerce_observe_remaining(ObserveVerdict::Stop, 2, 3),
            ObserveVerdict::Continue
        );
        assert_eq!(
            coerce_observe_remaining(ObserveVerdict::Stop, 0, 3),
            ObserveVerdict::Stop
        );
        assert_eq!(
            coerce_observe_remaining(ObserveVerdict::ReplanRemaining, 3, 4),
            ObserveVerdict::ReplanRemaining
        );
        assert_eq!(
            coerce_observe_remaining(ObserveVerdict::ReplanRemaining, 0, 3),
            ObserveVerdict::Stop
        );
        assert_eq!(
            coerce_observe_remaining(ObserveVerdict::ReplanRemaining, 0, 0),
            ObserveVerdict::ReplanRemaining
        );
    }

    #[test]
    fn parse_observe_fail_closed() {
        assert_eq!(
            parse_observe_verdict("nope", ObserveVerdict::Continue),
            ObserveVerdict::Continue
        );
        assert_eq!(
            parse_observe_verdict(
                r#"{"verdict":"replan_remaining"}"#,
                ObserveVerdict::Continue
            ),
            ObserveVerdict::ReplanRemaining
        );
        assert_eq!(
            parse_observe_verdict(r#"{"verdict":"stop"}"#, ObserveVerdict::Continue),
            ObserveVerdict::Stop
        );
    }

    #[test]
    fn operator_path_skips_empty_path() {
        let agent = crate::config::AgentConfig {
            bounded_dag_live: true,
            bounded_dag_path: None,
            ..crate::config::AgentConfig::default()
        };
        assert!(operator_fixed_live_graph(&agent).unwrap().is_none());
    }

    #[test]
    fn fenced_json_is_extracted() {
        let text = format!("Sure.\n```json\n{PAPER_JSON}\n```\n");
        let plan = resolve_planned_manifest(&text, CODE_FIX_TEMPLATE_JSON).unwrap();
        assert!(!plan.used_fallback);
        assert_eq!(plan.dag.id, "paper-slides");
    }

    #[test]
    fn preview_lists_contact_for_hints() {
        let plan = resolve_planned_manifest(PAPER_JSON, CODE_FIX_TEMPLATE_JSON).unwrap();
        let text = plan.preview_with_contact(
            "deepseek/deepseek-v4-flash",
            &["document".into(), "fast".into()],
        );
        assert!(text.contains("hint:document"), "{text}");
        assert!(text.contains("hint:fast"), "{text}");
    }

    struct TwoShotPlanner {
        responses: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl Provider for TwoShotPlanner {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Ok(String::new())
        }

        async fn chat(
            &self,
            _request: ChatRequest<'_>,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<crate::providers::ChatResponse> {
            let mut guard = self.responses.lock().unwrap();
            let text = if guard.is_empty() {
                "not json".into()
            } else {
                guard.remove(0)
            };
            Ok(crate::providers::ChatResponse {
                text: Some(text),
                tool_calls: vec![],
            })
        }
    }

    fn live_mem() -> crate::memory::NoneMemory {
        crate::memory::NoneMemory::new()
    }

    fn live_agent_cfg() -> crate::config::AgentConfig {
        crate::config::AgentConfig {
            bounded_dag_live: true,
            ..crate::config::AgentConfig::default()
        }
    }

    #[test]
    fn one_node_live_dag_is_linear_not_code_fix() {
        let plan = one_node_live_dag("check piubt then sync velaclaw");
        assert_eq!(plan.order, vec!["work"]);
        assert!(!plan.used_fallback);
        assert_eq!(plan.source, "one_node");
        assert_ne!(plan.order, vec!["locate", "patch", "verify"]);
    }

    #[test]
    fn first_hop_and_split_prompts_use_canonical_dag_plan() {
        let first = live_first_hop_system_prompt();
        let split = split_one_node_system_prompt();
        assert!(first.contains("chat_only"));
        assert!(first.contains("one node per deliverable"));
        assert!(split.contains("one node per deliverable"));
        assert!(split.contains("independently verifiable deliverable"));
        assert!(first.contains(DAG_PLAN_SYSTEM_PROMPT));
        assert!(split.contains(DAG_PLAN_SYSTEM_PROMPT));
        assert!(!TURN_OBSERVE_SYSTEM_PROMPT.contains("even when remaining_nodes is 0"));
    }

    #[tokio::test]
    async fn live_first_hop_refines_single_work_into_multi_node() {
        let provider = TwoShotPlanner {
            responses: std::sync::Mutex::new(vec![
                r#"{"path":"single_work"}"#.into(),
                PAPER_JSON.into(),
            ]),
        };
        let hop = live_first_hop(
            &live_agent_cfg(),
            &live_mem(),
            "sess",
            &provider,
            "m",
            "check piubt git then sync velaclaw",
            &[],
            0.0,
            HostPhase::Build,
        )
        .await
        .unwrap();
        match hop {
            LiveFirstHop::Plan(plan) => {
                assert_eq!(plan.order, vec!["read", "slides"]);
                assert_eq!(plan.source, "split_refine");
            }
            other => panic!("expected refined plan, got {other:?}"),
        }
        assert!(provider.responses.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn live_first_hop_skips_refine_for_multi_node() {
        let provider = TwoShotPlanner {
            responses: std::sync::Mutex::new(vec![
                CODE_FIX_TEMPLATE_JSON.to_string(),
                "MUST_NOT_REFINE".into(),
            ]),
        };
        let hop = live_first_hop(
            &live_agent_cfg(),
            &live_mem(),
            "sess",
            &provider,
            "m",
            "fix the compiler error",
            &[],
            0.0,
            HostPhase::Build,
        )
        .await
        .unwrap();
        match hop {
            LiveFirstHop::Plan(plan) => assert_eq!(plan.order.len(), 3),
            other => panic!("expected 3-node plan, got {other:?}"),
        }
        assert_eq!(provider.responses.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn live_first_hop_chat_only_skips_refine() {
        let provider = TwoShotPlanner {
            responses: std::sync::Mutex::new(vec![
                r#"{"path":"chat_only","reply":"Hi — ready."}"#.into(),
                "MUST_NOT_REFINE".into(),
            ]),
        };
        let hop = live_first_hop(
            &live_agent_cfg(),
            &live_mem(),
            "sess",
            &provider,
            "m",
            "hello",
            &[],
            0.0,
            HostPhase::Build,
        )
        .await
        .unwrap();
        assert!(matches!(hop, LiveFirstHop::ChatOnly { .. }));
        assert_eq!(provider.responses.lock().unwrap().len(), 1);
    }

    struct HangPlanner;

    #[async_trait::async_trait]
    impl Provider for HangPlanner {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            std::future::pending().await
        }

        async fn chat(
            &self,
            _request: ChatRequest<'_>,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<crate::providers::ChatResponse> {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn observe_timeout_fail_opens() {
        let verdict = observe_turn_outcome_timed(
            &HangPlanner,
            "m",
            0.0,
            "hello",
            "Hi",
            None,
            0,
            0,
            ObserveVerdict::Continue,
            std::time::Duration::from_millis(20),
        )
        .await
        .unwrap();
        assert_eq!(verdict, ObserveVerdict::Continue);
    }

    #[tokio::test]
    async fn observe_timeout_stops_when_hop_already_exhausted() {
        let verdict = observe_turn_outcome_timed(
            &HangPlanner,
            "m",
            0.0,
            "hello",
            "VelaClaw notice: tool-format recovery exhausted for model `x`.",
            None,
            2,
            3,
            ObserveVerdict::Continue,
            std::time::Duration::from_millis(20),
        )
        .await
        .unwrap();
        assert_eq!(verdict, ObserveVerdict::Stop);
    }

    #[tokio::test]
    async fn planner_retry_accepts_second_response() {
        let provider = TwoShotPlanner {
            responses: std::sync::Mutex::new(vec![PAPER_JSON.into()]),
        };
        let plan = run_live_planner_chat(
            &provider,
            "deepseek/deepseek-v4-flash",
            "read paper",
            0.0,
            CODE_FIX_TEMPLATE_JSON,
        )
        .await
        .unwrap();
        assert!(!plan.used_fallback);
        assert_eq!(plan.dag.id, "paper-slides");
    }

    #[test]
    fn splice_keeps_prefix_and_resumes_at_failed_node() {
        let stored = resolve_planned_manifest(
            r#"{
              "schema_version":"0.1.0","id":"ops","entry":"a","max_steps":8,
              "nodes":[
                {"id":"a","task_type":"ops-check","model_selector":{"capabilities":["coding"]},"next":"b"},
                {"id":"b","task_type":"ops-check","model_selector":{"capabilities":["coding"]},"next":"c"},
                {"id":"c","task_type":"summarize","model_selector":{"capabilities":["speed"]},"next":null}
              ]
            }"#,
            CODE_FIX_TEMPLATE_JSON,
        )
        .unwrap();
        let remaining = resolve_planned_manifest(
            r#"{
              "schema_version":"0.1.0","id":"ops-repair","entry":"b","max_steps":8,
              "nodes":[
                {"id":"b","task_type":"ops-check","model_selector":{"capabilities":["coding"]},"next":"c"},
                {"id":"c","task_type":"summarize","model_selector":{"capabilities":["document_understanding"]},"next":null}
              ]
            }"#,
            CODE_FIX_TEMPLATE_JSON,
        )
        .unwrap();
        let fail = DagFailCursor {
            node_id: "b".into(),
            index: 1,
            err: "timeout".into(),
            dag_id: "ops".into(),
            ..Default::default()
        };
        let spliced = splice_remaining_plan(&stored, &fail, remaining);
        assert_eq!(spliced.resume_from, 1);
        assert_eq!(spliced.order, vec!["a", "b", "c"]);
        assert_eq!(spliced.source, "repair");
        let b = spliced.dag.nodes.iter().find(|n| n.id == "b").unwrap();
        assert_eq!(b.task_type, "ops-check");
        assert_eq!(b.next.as_deref(), Some("c"));
        let c = spliced.dag.nodes.iter().find(|n| n.id == "c").unwrap();
        assert!(c.next.is_none());
        assert!(linear_node_ids(&spliced.dag).is_ok());
    }

    #[test]
    fn repair_prompt_asks_for_remaining_not_approval() {
        let fail = DagFailCursor {
            node_id: "check_install".into(),
            index: 0,
            err: "timeout".into(),
            dag_id: "opcencode-check-upgrade".into(),
            ..Default::default()
        };
        let prompt = repair_planner_user_prompt(
            "请检查 opcencode",
            &fail,
            &[],
            "不要用 find /，改查 which 和版本号",
        );
        assert!(prompt.contains("Failed node: check_install"));
        assert!(prompt.contains("不要用 find /"));
        assert!(!prompt.contains("Approve Build"));
        assert!(!prompt.contains("继续"));
    }

    #[test]
    fn work_node_stop_invites_step_guidance() {
        let zh = format_work_node_stop("请检查", "check_install", "timeout", 1, 3);
        assert!(zh.contains("已在第 1/3 步停住"));
        assert!(zh.contains("针对这一步发送新说明"));
        let en = format_work_node_stop("please check", "check_install", "timeout", 1, 3);
        assert!(en.contains("guidance for this step"));
    }

    #[test]
    fn brief_outline_uses_cjk_when_prompt_has_han() {
        let plan = PlannedLiveDag {
            dag: crate::agent::dag_runner::parse_dag_json(
                r#"{
                  "schema_version":"0.1.0","id":"opcencode-check-upgrade","entry":"check_install",
                  "max_steps":6,"nodes":[
                    {"id":"check_install","task_type":"ops-check","model_selector":{"capabilities":["coding"]},"next":"upgrade"},
                    {"id":"upgrade","task_type":"upgrade","model_selector":{"capabilities":["coding"]},"next":null}
                  ]
                }"#,
            )
            .unwrap(),
            order: vec!["check_install".into(), "upgrade".into()],
            used_fallback: false,
            source: "test",
            resume_from: 0,
            graph_task_override: None,
        };
        let out = plan.brief_outline("请检查 opcencode 是否要升级");
        assert!(out.contains("将分 2 步处理"));
        assert!(out.contains("check install"));
        assert!(!out.contains("Bounded task DAG"));
        assert!(!out.contains("contact model="));
    }

    #[test]
    fn operator_plan_gist_lists_artifacts_without_handoff() {
        let dag = parse_dag_json(
            r#"{
              "schema_version":"0.1.0","id":"t","entry":"research-official-upgrade-method","max_steps":4,
              "nodes":[
                {"id":"research-official-upgrade-method","task_type":"research","artifact":"官方升级路径","model_selector":{"capabilities":["coding"]},"next":"apply-upgrade"},
                {"id":"apply-upgrade","task_type":"ops","artifact":"执行升级","model_selector":{"capabilities":["coding"]},"next":null}
              ]
            }"#,
        )
        .unwrap();
        let gist = operator_plan_gist(
            "检查 openclaw 升级",
            &dag,
            &[
                "research-official-upgrade-method".into(),
                "apply-upgrade".into(),
            ],
            false,
        );
        assert!(gist.contains("将按 2 步"), "{gist}");
        assert!(gist.contains("官方升级路径"), "{gist}");
        assert!(gist.contains("执行升级"), "{gist}");
        assert!(!gist.to_ascii_lowercase().contains("handoff"), "{gist}");
        assert!(!gist.contains('\n'), "{gist}");
    }

    #[test]
    fn auto_replan_retries_unavailable_not_dns() {
        assert_eq!(
            decide_work_node_fail(true, false, "HTTP 404 (not_found): Function missing"),
            WorkNodeFailDecision::RetrySame {
                force_default: true
            }
        );
        assert_eq!(
            decide_work_node_fail(true, false, "Network transport error: dns error"),
            WorkNodeFailDecision::Stop
        );
        assert_eq!(
            decide_work_node_fail(false, false, "HTTP 410 Gone"),
            WorkNodeFailDecision::Stop
        );
        assert_eq!(
            decide_work_node_fail(true, true, "HTTP 410 Gone"),
            WorkNodeFailDecision::Stop
        );
    }
}
