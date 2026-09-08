//! 代理引擎模块，实现自主循环、分类与任务分发。
//!
//! ## Turn unification (ORCH cleanup / VL-CTX-001 / VL-CTX-002)
//! - **Shared:** [`crate::orchestration::resolve_turn_model`] (CLI `loop_` + Web
//!   [`agent::Agent::turn`]), [`context_orch::prepare_turn_history`] (compact + layered;
//!   live first hop consumes that history — VL-NA-030),
//!   L2 tool_dispatcher merge, and tool iteration via [`loop_::run_tool_call_loop`]
//!   (Web ApprovalHub / HITL still injected as `ToolBatchGateExtras` adapters).
//! - **Still dual:** approval backend *adapters* (stdin vs ApprovalHub) and CLI
//!   fold/render — not a second policy or tool-loop body.
//!
//! ## Bootstrap unification (VL-REVIEW2-A0 / GOV-007)
//! - **Canonical:** [`assemble::assemble_runtime`] (Config → security/memory/tools/
//!   provider/dispatcher). Web `Agent::from_config`, CLI `loop_::run` /
//!   `process_message`, and Channel `start_channels` call this entry.
//! - **Adapters only:** peripherals merge, stdin vs ApprovalHub, channel listeners,
//!   CLI fold/render.
#[allow(clippy::module_inception)]
pub mod agent;
pub mod assemble;
#[cfg(feature = "ai-protocol")]
pub mod bounded_dag;
#[cfg(feature = "ai-protocol")]
pub mod bounded_dag_context;
#[cfg(feature = "ai-protocol")]
pub mod bounded_dag_delivery;
#[cfg(feature = "ai-protocol")]
pub mod bounded_dag_live;
#[cfg(feature = "ai-protocol")]
pub mod candidate_dag;
pub mod classifier;
#[cfg(feature = "ai-protocol")]
pub mod context_contract;
pub mod context_orch;
#[cfg(feature = "ai-protocol")]
pub mod dag_runner;
pub mod dispatcher;
pub mod double_esc;
#[cfg(feature = "ai-protocol")]
pub mod envelope_pilot;
pub mod hop_stop;
pub mod host_phase;
#[cfg(feature = "ai-protocol")]
pub mod intent_route;
pub mod loop_;
pub mod memory_loader;
pub mod probe_dedup;
pub mod prompt;
pub mod prompt_composer;
pub mod session_resume;
pub mod subagent;
pub mod tool_batch;
pub mod turn_cancel;
pub mod turn_progress;
pub mod workspace_undo;

#[cfg(test)]
mod tests;

#[allow(unused_imports)]
pub use agent::{Agent, AgentBuilder};
#[allow(unused_imports)]
pub use loop_::{process_message, run, AgentRunOpts};
