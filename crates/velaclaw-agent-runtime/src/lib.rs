//! VelaClaw agent runtime — tool dispatcher + BYOK + loop helpers (VL-ARCH-007/008/010/A2).
//! Agent 运行时：工具分发、BYOK、loop 解析/历史还原辅助；`run_tool_call_loop` 仍在主 crate。

pub mod approval;
pub mod byok;
pub mod dispatcher;
pub mod execution_context;
pub mod history_roundtrip;
pub mod loop_parse;
pub mod provider;
pub mod telemetry;
pub mod tool_format;
pub mod tool_ir;
pub mod tool_util;
pub mod tools;

pub use approval::{
    is_shell_policy_tool, shell_command_from_args, ApprovalGate, GateDecision,
    HumanApprovalBackend, ShellPolicyHook,
};
pub use byok::{
    execute_chat_with_retry, init_ai_client_sync, resolve_ai_client, split_logical_model_id,
};
#[cfg(feature = "ai-protocol")]
pub use dispatcher::parse_manifest_text_tool_fallback;
pub use dispatcher::{
    build_tool_dispatcher, build_tool_dispatcher_for_logical_model, text_tool_parser_from_manifest,
    NativeToolDispatcher, ParsedToolCall, ToolDispatcher, ToolExecutionResult, XmlToolDispatcher,
};
pub use execution_context::ToolExecutionContext;
pub use history_roundtrip::{conversation_from_tool_loop_history, reintegrate_prepared_chat};
pub use loop_parse::{
    build_tool_instructions, is_tool_loop_cancelled, parse_tool_calls, tools_to_openai_format,
    trim_history, ToolLoopCancelled, DEFAULT_MAX_HISTORY_MESSAGES, DEFAULT_MAX_TOOL_ITERATIONS,
};
pub use provider::{
    ChatMessage, ChatRequest, ChatResponse, ConversationMessage, NativeToolCapable, ToolCall,
    ToolResultMessage,
};
pub use tool_format::{
    append_tool_format_exhausted_notice, host_decide_failover_announce, looks_like_model_retired,
    looks_like_provider_limit, looks_like_tool_format_exhausted_notice,
    needs_tool_format_correction, parse_repaired_tool_calls, provider_limit_user_message,
    provider_retired_user_message, repair_extract_system_prompt,
    strip_tool_format_exhausted_notice, tool_format_correction_message,
    tool_format_recovery_message, truncate_repair_blob, RepairedToolCall, SoftFailSurface,
    ToolFormatLadder, ToolFormatRecoveryStrategy,
};
pub use tool_ir::{
    append_unregistered_ir_notice, decode_unwrapped_ir, is_line_isolated_json_segment,
    is_tool_call_payload, is_tool_result_payload, sanitize_tool_json_value,
    strip_isolated_tool_json_artifacts, UnwrappedIrDecode,
};
pub use tool_util::{normalize_tool_arguments, scrub_credentials};
pub use tools::{Tool, ToolResult, ToolSpec};
