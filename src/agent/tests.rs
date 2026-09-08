//! Comprehensive agent-loop test suite.
//!
//! Tests exercise the full `Agent.turn()` cycle with mock providers and tools,
//! covering every edge case an agentic tool loop must handle:
//!
//!   1. Simple text response (no tools)
//!   2. Single tool call → final response
//!   3. Multi-step tool chain (tool A → tool B → response)
//!   4. Max-iteration bailout
//!   5. Unknown tool name recovery
//!   6. Tool execution failure recovery
//!   7. Parallel tool dispatch
//!   8. History trimming during long conversations
//!   9. Memory auto-save round-trip
//!  10. Native vs XML dispatcher integration
//!  11. Empty / whitespace-only LLM responses
//!  12. Mixed text + tool call responses
//!  13. Multi-tool batch in a single response
//!  14. System prompt generation & tool instructions
//!  15. Context enrichment from memory loader
//!  16. ConversationMessage serialization round-trip
//!  17. Tool call with stringified JSON arguments
//!  18. Conversation history fidelity (tool call → tool result → assistant)
//!  19. Builder validation (missing required fields)
//!  20. Idempotent system prompt insertion

use crate::agent::agent::Agent;
use crate::agent::dispatcher::{
    NativeToolDispatcher, ToolDispatcher, ToolExecutionResult, XmlToolDispatcher,
};
use crate::config::{AgentConfig, MemoryConfig};
use crate::memory::{self, Memory};
use crate::observability::{NoopObserver, Observer};
use crate::providers::{
    ChatMessage, ChatRequest, ChatResponse, ConversationMessage, Provider, ToolCall,
    ToolResultMessage,
};
use crate::security::PolicyHandle;
use crate::tools::{Tool, ToolExecutionContext, ToolResult};
use anyhow::Result;
use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

// ═══════════════════════════════════════════════════════════════════════════
// Test Helpers — Mock Provider, Mock Tool, Mock Memory
// ═══════════════════════════════════════════════════════════════════════════

/// A mock LLM provider that returns pre-scripted responses in order.
/// When the queue is exhausted it returns a simple "done" text response.
struct ScriptedProvider {
    responses: Mutex<Vec<ChatResponse>>,
    /// Records every request for assertion.
    requests: Arc<Mutex<Vec<Vec<ChatMessage>>>>,
    models: Arc<Mutex<Vec<String>>>,
    call_count: Arc<AtomicUsize>,
}

impl ScriptedProvider {
    fn new(responses: Vec<ChatResponse>) -> Self {
        Self {
            responses: Mutex::new(responses),
            requests: Arc::new(Mutex::new(Vec::new())),
            models: Arc::new(Mutex::new(Vec::new())),
            call_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    fn call_counter(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.call_count)
    }

    fn recorded_requests(&self) -> Arc<Mutex<Vec<Vec<ChatMessage>>>> {
        Arc::clone(&self.requests)
    }

    fn recorded_models(&self) -> Arc<Mutex<Vec<String>>> {
        Arc::clone(&self.models)
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> Result<String> {
        Ok("fallback".into())
    }

    async fn chat(
        &self,
        request: ChatRequest<'_>,
        model: &str,
        _temperature: f64,
    ) -> Result<ChatResponse> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        self.models.lock().unwrap().push(model.to_string());
        self.requests
            .lock()
            .unwrap()
            .push(request.messages.to_vec());

        let mut guard = self.responses.lock().unwrap();
        if guard.is_empty() {
            return Ok(ChatResponse {
                text: Some("done".into()),
                tool_calls: vec![],
            });
        }
        Ok(guard.remove(0))
    }
}

/// A mock provider that always returns an error.
struct FailingProvider;

#[async_trait]
impl Provider for FailingProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> Result<String> {
        anyhow::bail!("provider error")
    }

    async fn chat(
        &self,
        _request: ChatRequest<'_>,
        _model: &str,
        _temperature: f64,
    ) -> Result<ChatResponse> {
        anyhow::bail!("provider error")
    }
}

/// A simple echo tool that returns its arguments as output.
struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "Echoes the input"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "message": {"type": "string"}
            }
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolExecutionContext,
    ) -> Result<ToolResult> {
        let msg = args
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("(empty)")
            .to_string();
        Ok(ToolResult {
            success: true,
            output: msg,
            error: None,
        })
    }
}

/// A tool that always fails execution.
struct FailingTool;

#[async_trait]
impl Tool for FailingTool {
    fn name(&self) -> &str {
        "fail"
    }

    fn description(&self) -> &str {
        "Always fails"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolExecutionContext,
    ) -> Result<ToolResult> {
        Ok(ToolResult {
            success: false,
            output: String::new(),
            error: Some("intentional failure".into()),
        })
    }
}

/// A tool that panics (tests error propagation).
struct PanickingTool;

#[async_trait]
impl Tool for PanickingTool {
    fn name(&self) -> &str {
        "panicker"
    }

    fn description(&self) -> &str {
        "Panics on execution"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolExecutionContext,
    ) -> Result<ToolResult> {
        anyhow::bail!("catastrophic tool failure")
    }
}

/// A tool that tracks how many times it was called.
struct CountingTool {
    count: Arc<Mutex<usize>>,
}

impl CountingTool {
    fn new() -> (Self, Arc<Mutex<usize>>) {
        let count = Arc::new(Mutex::new(0));
        (
            Self {
                count: count.clone(),
            },
            count,
        )
    }
}

#[async_trait]
impl Tool for CountingTool {
    fn name(&self) -> &str {
        "counter"
    }

    fn description(&self) -> &str {
        "Counts calls"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolExecutionContext,
    ) -> Result<ToolResult> {
        let mut c = self.count.lock().unwrap();
        *c += 1;
        Ok(ToolResult {
            success: true,
            output: format!("call #{}", *c),
            error: None,
        })
    }
}

fn make_memory() -> Arc<dyn Memory> {
    let cfg = MemoryConfig {
        backend: "none".into(),
        ..MemoryConfig::default()
    };
    Arc::from(memory::create_memory(&cfg, &std::env::temp_dir(), None).unwrap())
}

fn make_sqlite_memory() -> (Arc<dyn Memory>, tempfile::TempDir) {
    let tmp = tempfile::TempDir::new().unwrap();
    let cfg = MemoryConfig {
        backend: "sqlite".into(),
        ..MemoryConfig::default()
    };
    let mem = Arc::from(memory::create_memory(&cfg, tmp.path(), None).unwrap());
    (mem, tmp)
}

fn make_observer() -> Arc<dyn Observer> {
    Arc::from(NoopObserver {})
}

fn test_security() -> PolicyHandle {
    PolicyHandle::from_config(
        &crate::config::AutonomyConfig::default(),
        &std::env::temp_dir(),
    )
}

fn build_agent_with(
    provider: Box<dyn Provider>,
    tools: Vec<Box<dyn Tool>>,
    dispatcher: Box<dyn ToolDispatcher>,
) -> Agent {
    Agent::builder()
        .provider(provider)
        .tools(tools)
        .memory(make_memory())
        .observer(make_observer())
        .tool_dispatcher(dispatcher)
        .workspace_dir(std::env::temp_dir())
        .security(test_security())
        .build()
        .unwrap()
}

fn build_agent_with_memory(
    provider: Box<dyn Provider>,
    tools: Vec<Box<dyn Tool>>,
    mem: Arc<dyn Memory>,
    auto_save: bool,
) -> Agent {
    Agent::builder()
        .provider(provider)
        .tools(tools)
        .memory(mem)
        .observer(make_observer())
        .tool_dispatcher(Box::new(NativeToolDispatcher::default()))
        .workspace_dir(std::env::temp_dir())
        .auto_save(auto_save)
        .security(test_security())
        .build()
        .unwrap()
}

fn build_agent_with_config(
    provider: Box<dyn Provider>,
    tools: Vec<Box<dyn Tool>>,
    config: AgentConfig,
) -> Agent {
    Agent::builder()
        .provider(provider)
        .tools(tools)
        .memory(make_memory())
        .observer(make_observer())
        .tool_dispatcher(Box::new(NativeToolDispatcher::default()))
        .workspace_dir(std::env::temp_dir())
        .config(config)
        .security(test_security())
        .build()
        .unwrap()
}

/// Helper: create a ChatResponse with tool calls (native format).
fn tool_response(calls: Vec<ToolCall>) -> ChatResponse {
    ChatResponse {
        text: Some(String::new()),
        tool_calls: calls,
    }
}

/// Helper: create a plain text ChatResponse.
fn text_response(text: &str) -> ChatResponse {
    ChatResponse {
        text: Some(text.into()),
        tool_calls: vec![],
    }
}

/// Helper: create an XML-style tool call response.
fn xml_tool_response(name: &str, args: &str) -> ChatResponse {
    ChatResponse {
        text: Some(format!(
            "<tool_call>\n{{\"name\": \"{name}\", \"arguments\": {args}}}\n</tool_call>"
        )),
        tool_calls: vec![],
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// 1. Simple text response (no tools)
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn turn_returns_text_when_no_tools_called() {
    let provider = Box::new(ScriptedProvider::new(vec![text_response("Hello world")]));
    let mut agent = build_agent_with(
        provider,
        vec![Box::new(EchoTool)],
        Box::new(NativeToolDispatcher::default()),
    );

    let response = agent.turn("hi").await.unwrap();
    assert!(
        !response.is_empty(),
        "Expected non-empty text response from provider"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 2. Single tool call → final response
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn turn_executes_single_tool_then_returns() {
    let provider = Box::new(ScriptedProvider::new(vec![
        tool_response(vec![ToolCall {
            id: "tc1".into(),
            name: "echo".into(),
            arguments: r#"{"message": "hello from tool"}"#.into(),
        }]),
        text_response("I ran the tool"),
    ]));

    let mut agent = build_agent_with(
        provider,
        vec![Box::new(EchoTool)],
        Box::new(NativeToolDispatcher::default()),
    );

    let response = agent.turn("run echo").await.unwrap();
    assert!(
        !response.is_empty(),
        "Expected non-empty response after tool execution"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 3. Multi-step tool chain (tool A → tool B → response)
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn turn_handles_multi_step_tool_chain() {
    let (counting_tool, count) = CountingTool::new();

    let provider = Box::new(ScriptedProvider::new(vec![
        tool_response(vec![ToolCall {
            id: "tc1".into(),
            name: "counter".into(),
            arguments: "{}".into(),
        }]),
        tool_response(vec![ToolCall {
            id: "tc2".into(),
            name: "counter".into(),
            arguments: "{}".into(),
        }]),
        tool_response(vec![ToolCall {
            id: "tc3".into(),
            name: "counter".into(),
            arguments: "{}".into(),
        }]),
        text_response("Done after 3 calls"),
    ]));

    let mut agent = build_agent_with(
        provider,
        vec![Box::new(counting_tool)],
        Box::new(NativeToolDispatcher::default()),
    );

    let response = agent.turn("count 3 times").await.unwrap();
    assert!(
        !response.is_empty(),
        "Expected non-empty response after multi-step chain"
    );
    assert_eq!(*count.lock().unwrap(), 3);
}

// ═══════════════════════════════════════════════════════════════════════════
// 4. Max-iteration bailout
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn turn_bails_out_at_max_iterations() {
    // Create more tool calls than max_tool_iterations allows.
    let max_iters = 3;
    let mut responses = Vec::new();
    for i in 0..max_iters + 5 {
        responses.push(tool_response(vec![ToolCall {
            id: format!("tc{i}"),
            name: "echo".into(),
            arguments: r#"{"message": "loop"}"#.into(),
        }]));
    }

    let provider = Box::new(ScriptedProvider::new(responses));

    let config = AgentConfig {
        max_tool_iterations: max_iters,
        ..AgentConfig::default()
    };

    let mut agent = build_agent_with_config(provider, vec![Box::new(EchoTool)], config);

    let result = agent.turn("infinite loop").await;
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("maximum tool iterations"),
        "Expected max iterations error, got: {err}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 5. Unknown tool name recovery
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn turn_handles_unknown_tool_gracefully() {
    let provider = Box::new(ScriptedProvider::new(vec![
        tool_response(vec![ToolCall {
            id: "tc1".into(),
            name: "nonexistent_tool".into(),
            arguments: "{}".into(),
        }]),
        text_response("I couldn't find that tool"),
    ]));

    let mut agent = build_agent_with(
        provider,
        vec![Box::new(EchoTool)],
        Box::new(NativeToolDispatcher::default()),
    );

    let response = agent.turn("use nonexistent").await.unwrap();
    assert!(
        !response.is_empty(),
        "Expected non-empty response after unknown tool recovery"
    );

    // Verify the tool result mentioned "Unknown tool"
    let has_tool_result = agent.history().iter().any(|msg| match msg {
        ConversationMessage::ToolResults(results) => {
            results.iter().any(|r| r.content.contains("Unknown tool"))
        }
        _ => false,
    });
    assert!(
        has_tool_result,
        "Expected tool result with 'Unknown tool' message"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 6. Tool execution failure recovery
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn turn_recovers_from_tool_failure() {
    let provider = Box::new(ScriptedProvider::new(vec![
        tool_response(vec![ToolCall {
            id: "tc1".into(),
            name: "fail".into(),
            arguments: "{}".into(),
        }]),
        text_response("Tool failed but I recovered"),
    ]));

    let mut agent = build_agent_with(
        provider,
        vec![Box::new(FailingTool)],
        Box::new(NativeToolDispatcher::default()),
    );

    let response = agent.turn("try failing tool").await.unwrap();
    assert!(
        !response.is_empty(),
        "Expected non-empty response after tool failure recovery"
    );
}

#[tokio::test]
async fn turn_recovers_from_tool_error() {
    let provider = Box::new(ScriptedProvider::new(vec![
        tool_response(vec![ToolCall {
            id: "tc1".into(),
            name: "panicker".into(),
            arguments: "{}".into(),
        }]),
        text_response("I recovered from the error"),
    ]));

    let mut agent = build_agent_with(
        provider,
        vec![Box::new(PanickingTool)],
        Box::new(NativeToolDispatcher::default()),
    );

    let response = agent.turn("try panicking").await.unwrap();
    assert!(
        !response.is_empty(),
        "Expected non-empty response after tool error recovery"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 7. Provider error propagation
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn turn_propagates_provider_error() {
    let mut agent = build_agent_with(
        Box::new(FailingProvider),
        vec![],
        Box::new(NativeToolDispatcher::default()),
    );

    let result = agent.turn("hello").await;
    assert!(result.is_err(), "Expected provider error to propagate");
}

// ═══════════════════════════════════════════════════════════════════════════
// 8. History trimming during long conversations
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn history_trims_after_max_messages() {
    let max_history = 6;
    let mut responses = vec![];
    for _ in 0..max_history + 5 {
        responses.push(text_response("ok"));
    }

    let provider = Box::new(ScriptedProvider::new(responses));
    let config = AgentConfig {
        max_history_messages: max_history,
        ..AgentConfig::default()
    };

    let mut agent = build_agent_with_config(provider, vec![], config);

    for i in 0..max_history + 5 {
        let _ = agent.turn(&format!("msg {i}")).await.unwrap();
    }

    // System prompt (1) + trimmed messages
    // Should not exceed max_history + 1 (system prompt)
    assert!(
        agent.history().len() <= max_history + 1,
        "History length {} exceeds max {} + 1 (system)",
        agent.history().len(),
        max_history,
    );

    // System prompt should always be preserved
    let first = &agent.history()[0];
    assert!(matches!(first, ConversationMessage::Chat(c) if c.role == "system"));
}

// ═══════════════════════════════════════════════════════════════════════════
// 9. Memory auto-save round-trip
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn auto_save_stores_only_user_messages_in_memory() {
    let (mem, _tmp) = make_sqlite_memory();
    let provider = Box::new(ScriptedProvider::new(vec![text_response(
        "I remember everything",
    )]));

    let mut agent = build_agent_with_memory(
        provider,
        vec![],
        mem.clone(),
        true, // auto_save enabled
    );

    let _ = agent.turn("Remember this fact").await.unwrap();

    // Auto-save only persists user-stated input, never assistant-generated summaries.
    let count = mem.count().await.unwrap();
    assert_eq!(
        count, 1,
        "Expected exactly 1 user memory entry, got {count}"
    );

    let stored = mem.get("user_msg").await.unwrap();
    assert!(stored.is_some(), "Expected user_msg key to be present");
    assert_eq!(
        stored.unwrap().content,
        "Remember this fact",
        "Stored memory should match the original user message"
    );

    let assistant = mem.get("assistant_resp").await.unwrap();
    assert!(
        assistant.is_none(),
        "assistant_resp should not be auto-saved anymore"
    );
}

#[tokio::test]
async fn auto_save_disabled_does_not_store() {
    let (mem, _tmp) = make_sqlite_memory();
    let provider = Box::new(ScriptedProvider::new(vec![text_response("hello")]));

    let mut agent = build_agent_with_memory(
        provider,
        vec![],
        mem.clone(),
        false, // auto_save disabled
    );

    let _ = agent.turn("test message").await.unwrap();

    let count = mem.count().await.unwrap();
    assert_eq!(count, 0, "Expected 0 memory entries with auto_save off");
}

// ═══════════════════════════════════════════════════════════════════════════
// 10. Native vs XML dispatcher integration
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn xml_dispatcher_parses_and_loops() {
    let provider = Box::new(ScriptedProvider::new(vec![
        xml_tool_response("echo", r#"{"message": "xml-test"}"#),
        text_response("XML tool completed"),
    ]));

    let mut agent = build_agent_with(
        provider,
        vec![Box::new(EchoTool)],
        Box::new(XmlToolDispatcher::default()),
    );

    let response = agent.turn("test xml").await.unwrap();
    assert!(
        !response.is_empty(),
        "Expected non-empty response from XML dispatcher"
    );
}

#[tokio::test]
async fn native_dispatcher_sends_tool_specs() {
    let provider = Box::new(ScriptedProvider::new(vec![text_response("ok")]));
    let mut agent = build_agent_with(
        provider,
        vec![Box::new(EchoTool)],
        Box::new(NativeToolDispatcher::default()),
    );

    let _ = agent.turn("hi").await.unwrap();

    // NativeToolDispatcher.should_send_tool_specs() returns true
    let dispatcher = NativeToolDispatcher::default();
    assert!(dispatcher.should_send_tool_specs());
}

#[tokio::test]
async fn xml_dispatcher_does_not_send_tool_specs() {
    let dispatcher = XmlToolDispatcher::default();
    assert!(!dispatcher.should_send_tool_specs());
}

// ═══════════════════════════════════════════════════════════════════════════
// 11. Empty / whitespace-only LLM responses
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn turn_handles_empty_text_response() {
    let provider = Box::new(ScriptedProvider::new(vec![ChatResponse {
        text: Some(String::new()),
        tool_calls: vec![],
    }]));

    let mut agent = build_agent_with(provider, vec![], Box::new(NativeToolDispatcher::default()));

    let response = agent.turn("hi").await.unwrap();
    assert!(response.is_empty());
}

#[tokio::test]
async fn turn_handles_none_text_response() {
    let provider = Box::new(ScriptedProvider::new(vec![ChatResponse {
        text: None,
        tool_calls: vec![],
    }]));

    let mut agent = build_agent_with(provider, vec![], Box::new(NativeToolDispatcher::default()));

    // Should not panic — falls back to empty string
    let response = agent.turn("hi").await.unwrap();
    assert!(response.is_empty());
}

// ═══════════════════════════════════════════════════════════════════════════
// 12. Mixed text + tool call responses
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn turn_preserves_text_alongside_tool_calls() {
    let provider = Box::new(ScriptedProvider::new(vec![
        ChatResponse {
            text: Some("Let me check...".into()),
            tool_calls: vec![ToolCall {
                id: "tc1".into(),
                name: "echo".into(),
                arguments: r#"{"message": "hi"}"#.into(),
            }],
        },
        text_response("Here are the results"),
    ]));

    let mut agent = build_agent_with(
        provider,
        vec![Box::new(EchoTool)],
        Box::new(NativeToolDispatcher::default()),
    );

    let response = agent.turn("check something").await.unwrap();
    assert!(
        !response.is_empty(),
        "Expected non-empty final response after mixed text+tool"
    );

    // Intermediate assistant text lives on AssistantToolCalls for the native path
    // (text-tool path uses Chat assistant + user [Tool results]).
    let has_intermediate = agent.history().iter().any(|msg| match msg {
        ConversationMessage::Chat(c) => c.role == "assistant" && c.content.contains("Let me check"),
        ConversationMessage::AssistantToolCalls { text: Some(t), .. } => t.contains("Let me check"),
        _ => false,
    });
    assert!(has_intermediate, "Intermediate text should be in history");
}

// ═══════════════════════════════════════════════════════════════════════════
// 13. Multi-tool batch in a single response
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn turn_handles_multiple_tools_in_one_response() {
    let (counting_tool, count) = CountingTool::new();

    let provider = Box::new(ScriptedProvider::new(vec![
        tool_response(vec![
            ToolCall {
                id: "tc1".into(),
                name: "counter".into(),
                arguments: "{}".into(),
            },
            ToolCall {
                id: "tc2".into(),
                name: "counter".into(),
                arguments: "{}".into(),
            },
            ToolCall {
                id: "tc3".into(),
                name: "counter".into(),
                arguments: "{}".into(),
            },
        ]),
        text_response("All 3 done"),
    ]));

    let mut agent = build_agent_with(
        provider,
        vec![Box::new(counting_tool)],
        Box::new(NativeToolDispatcher::default()),
    );

    let response = agent.turn("batch").await.unwrap();
    assert!(
        !response.is_empty(),
        "Expected non-empty response after multi-tool batch"
    );
    assert_eq!(
        *count.lock().unwrap(),
        3,
        "All 3 tools should have been called"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 14. System prompt generation & tool instructions
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn system_prompt_injected_on_first_turn() {
    let provider = Box::new(ScriptedProvider::new(vec![text_response("ok")]));
    let mut agent = build_agent_with(
        provider,
        vec![Box::new(EchoTool)],
        Box::new(NativeToolDispatcher::default()),
    );

    assert!(agent.history().is_empty(), "History should start empty");

    let _ = agent.turn("hi").await.unwrap();

    // First message should be the system prompt
    let first = &agent.history()[0];
    assert!(
        matches!(first, ConversationMessage::Chat(c) if c.role == "system"),
        "First history entry should be system prompt"
    );
}

#[tokio::test]
async fn system_prompt_not_duplicated_on_second_turn() {
    let provider = Box::new(ScriptedProvider::new(vec![
        text_response("first"),
        text_response("second"),
    ]));
    let mut agent = build_agent_with(
        provider,
        vec![Box::new(EchoTool)],
        Box::new(NativeToolDispatcher::default()),
    );

    let _ = agent.turn("hi").await.unwrap();
    let _ = agent.turn("hello again").await.unwrap();

    let system_count = agent
        .history()
        .iter()
        .filter(|msg| matches!(msg, ConversationMessage::Chat(c) if c.role == "system"))
        .count();
    assert_eq!(system_count, 1, "System prompt should appear exactly once");
}

// ═══════════════════════════════════════════════════════════════════════════
// 15. Conversation history fidelity
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn history_contains_all_expected_entries_after_tool_loop() {
    let provider = Box::new(ScriptedProvider::new(vec![
        tool_response(vec![ToolCall {
            id: "tc1".into(),
            name: "echo".into(),
            arguments: r#"{"message": "tool-out"}"#.into(),
        }]),
        text_response("final answer"),
    ]));

    let mut agent = build_agent_with(
        provider,
        vec![Box::new(EchoTool)],
        Box::new(NativeToolDispatcher::default()),
    );

    let _ = agent.turn("test").await.unwrap();

    // Expected history entries:
    //   0: system prompt
    //   1: user message "test"
    //   2: AssistantToolCalls
    //   3: ToolResults
    //   4: assistant "final answer"
    let history = agent.history();
    assert!(
        history.len() >= 5,
        "Expected at least 5 history entries, got {}",
        history.len()
    );

    assert!(matches!(&history[0], ConversationMessage::Chat(c) if c.role == "system"));
    assert!(matches!(&history[1], ConversationMessage::Chat(c) if c.role == "user"));
    assert!(matches!(
        &history[2],
        ConversationMessage::AssistantToolCalls { .. }
    ));
    assert!(matches!(&history[3], ConversationMessage::ToolResults(_)));
    assert!(
        matches!(&history[4], ConversationMessage::Chat(c) if c.role == "assistant" && c.content == "final answer")
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 16. Builder validation
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn builder_fails_without_provider() {
    let result = Agent::builder()
        .tools(vec![])
        .memory(make_memory())
        .observer(make_observer())
        .tool_dispatcher(Box::new(NativeToolDispatcher::default()))
        .workspace_dir(std::path::PathBuf::from("/tmp"))
        .build();

    assert!(result.is_err(), "Building without provider should fail");
}

// ═══════════════════════════════════════════════════════════════════════════
// 17. Multi-turn conversation maintains context
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn multi_turn_maintains_growing_history() {
    let provider = Box::new(ScriptedProvider::new(vec![
        text_response("response 1"),
        text_response("response 2"),
        text_response("response 3"),
    ]));

    let mut agent = build_agent_with(provider, vec![], Box::new(NativeToolDispatcher::default()));

    let r1 = agent.turn("msg 1").await.unwrap();
    let len_after_1 = agent.history().len();

    let r2 = agent.turn("msg 2").await.unwrap();
    let len_after_2 = agent.history().len();

    let r3 = agent.turn("msg 3").await.unwrap();
    let len_after_3 = agent.history().len();

    assert_eq!(r1, "response 1");
    assert_eq!(r2, "response 2");
    assert_eq!(r3, "response 3");

    // History should grow with each turn (user + assistant per turn)
    assert!(
        len_after_2 > len_after_1,
        "History should grow after turn 2"
    );
    assert!(
        len_after_3 > len_after_2,
        "History should grow after turn 3"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 18. Tool call with stringified JSON arguments (common LLM pattern)
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn native_dispatcher_handles_stringified_arguments() {
    let dispatcher = NativeToolDispatcher::default();
    let response = ChatResponse {
        text: Some(String::new()),
        tool_calls: vec![ToolCall {
            id: "tc1".into(),
            name: "echo".into(),
            arguments: r#"{"message": "hello"}"#.into(),
        }],
    };

    let (_, calls) = dispatcher.parse_response(&response);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "echo");
    assert_eq!(
        calls[0].arguments.get("message").unwrap().as_str().unwrap(),
        "hello"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 19. XML dispatcher edge cases
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn xml_dispatcher_handles_nested_json() {
    let response = ChatResponse {
        text: Some(
            r#"<tool_call>
{"name": "file_write", "arguments": {"path": "test.json", "content": "{\"key\": \"value\"}"}}
</tool_call>"#
                .into(),
        ),
        tool_calls: vec![],
    };

    let dispatcher = XmlToolDispatcher::default();
    let (_, calls) = dispatcher.parse_response(&response);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "file_write");
    assert_eq!(
        calls[0].arguments.get("path").unwrap().as_str().unwrap(),
        "test.json"
    );
}

#[test]
fn xml_dispatcher_handles_empty_tool_call_tag() {
    let response = ChatResponse {
        text: Some("<tool_call>\n</tool_call>\nSome text".into()),
        tool_calls: vec![],
    };

    let dispatcher = XmlToolDispatcher::default();
    let (text, calls) = dispatcher.parse_response(&response);
    assert!(calls.is_empty());
    assert!(text.contains("Some text"));
}

#[test]
fn xml_dispatcher_handles_unclosed_tool_call() {
    let response = ChatResponse {
        text: Some("Before\n<tool_call>\n{\"name\": \"shell\"}".into()),
        tool_calls: vec![],
    };

    let dispatcher = XmlToolDispatcher::default();
    let (text, calls) = dispatcher.parse_response(&response);
    // Should not panic — just treat as text
    assert!(calls.is_empty());
    assert!(text.contains("Before"));
}

// ═══════════════════════════════════════════════════════════════════════════
// 20. ConversationMessage serialization round-trip
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn conversation_message_serialization_roundtrip() {
    let messages = vec![
        ConversationMessage::Chat(ChatMessage::system("system")),
        ConversationMessage::Chat(ChatMessage::user("hello")),
        ConversationMessage::AssistantToolCalls {
            text: Some("checking".into()),
            tool_calls: vec![ToolCall {
                id: "tc1".into(),
                name: "shell".into(),
                arguments: "{}".into(),
            }],
        },
        ConversationMessage::ToolResults(vec![ToolResultMessage {
            tool_call_id: "tc1".into(),
            content: "ok".into(),
        }]),
        ConversationMessage::Chat(ChatMessage::assistant("done")),
    ];

    for msg in &messages {
        let json = serde_json::to_string(msg).unwrap();
        let parsed: ConversationMessage = serde_json::from_str(&json).unwrap();

        // Verify the variant type matches
        match (msg, &parsed) {
            (ConversationMessage::Chat(a), ConversationMessage::Chat(b)) => {
                assert_eq!(a.role, b.role);
                assert_eq!(a.content, b.content);
            }
            (
                ConversationMessage::AssistantToolCalls {
                    text: a_text,
                    tool_calls: a_calls,
                },
                ConversationMessage::AssistantToolCalls {
                    text: b_text,
                    tool_calls: b_calls,
                },
            ) => {
                assert_eq!(a_text, b_text);
                assert_eq!(a_calls.len(), b_calls.len());
            }
            (ConversationMessage::ToolResults(a), ConversationMessage::ToolResults(b)) => {
                assert_eq!(a.len(), b.len());
            }
            _ => panic!("Variant mismatch after serialization"),
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// 21. Tool dispatcher format_results
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn xml_format_results_includes_status_and_output() {
    let dispatcher = XmlToolDispatcher::default();
    let results = vec![
        ToolExecutionResult {
            name: "shell".into(),
            output: "file1.txt\nfile2.txt".into(),
            success: true,
            tool_call_id: None,
        },
        ToolExecutionResult {
            name: "file_read".into(),
            output: "Error: file not found".into(),
            success: false,
            tool_call_id: None,
        },
    ];

    let msg = dispatcher.format_results(&results);
    let content = match msg {
        ConversationMessage::Chat(c) => c.content,
        _ => panic!("Expected Chat variant"),
    };

    assert!(content.contains("file1.txt"));
    assert!(content.contains("is_error"));
    assert!(content.contains("not found"));
    assert!(content.contains("<tool_result"));
}

#[test]
fn native_format_results_maps_tool_call_ids() {
    let dispatcher = NativeToolDispatcher::default();
    let results = vec![
        ToolExecutionResult {
            name: "a".into(),
            output: "out1".into(),
            success: true,
            tool_call_id: Some("tc-001".into()),
        },
        ToolExecutionResult {
            name: "b".into(),
            output: "out2".into(),
            success: true,
            tool_call_id: Some("tc-002".into()),
        },
    ];

    let msg = dispatcher.format_results(&results);
    match msg {
        ConversationMessage::ToolResults(r) => {
            assert_eq!(r.len(), 2);
            assert_eq!(r[0].tool_call_id, "tc-001");
            assert_eq!(r[0].content, "out1");
            assert_eq!(r[1].tool_call_id, "tc-002");
            assert_eq!(r[1].content, "out2");
        }
        _ => panic!("Expected ToolResults"),
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// 22. to_provider_messages conversion
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn xml_dispatcher_converts_history_to_provider_messages() {
    let dispatcher = XmlToolDispatcher::default();
    let history = vec![
        ConversationMessage::Chat(ChatMessage::system("sys")),
        ConversationMessage::Chat(ChatMessage::user("hi")),
        ConversationMessage::AssistantToolCalls {
            text: Some("checking".into()),
            tool_calls: vec![ToolCall {
                id: "tc1".into(),
                name: "shell".into(),
                arguments: "{}".into(),
            }],
        },
        ConversationMessage::ToolResults(vec![ToolResultMessage {
            tool_call_id: "tc1".into(),
            content: "ok".into(),
        }]),
        ConversationMessage::Chat(ChatMessage::assistant("done")),
    ];

    let messages = dispatcher.to_provider_messages(&history);

    // Should have: system, user, assistant (from tool calls), user (tool results), assistant
    assert!(messages.len() >= 4);
    assert_eq!(messages[0].role, "system");
    assert_eq!(messages[1].role, "user");
}

#[test]
fn native_dispatcher_converts_tool_results_to_tool_messages() {
    let dispatcher = NativeToolDispatcher::default();
    let history = vec![ConversationMessage::ToolResults(vec![
        ToolResultMessage {
            tool_call_id: "tc1".into(),
            content: "output1".into(),
        },
        ToolResultMessage {
            tool_call_id: "tc2".into(),
            content: "output2".into(),
        },
    ])];

    let messages = dispatcher.to_provider_messages(&history);
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role, "tool");
    assert_eq!(messages[1].role, "tool");
}

// ═══════════════════════════════════════════════════════════════════════════
// 23. XML tool instructions generation
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn xml_dispatcher_generates_tool_instructions() {
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(EchoTool)];
    let dispatcher = XmlToolDispatcher::default();
    let instructions = dispatcher.prompt_instructions(&tools);

    assert!(instructions.contains("## Tool Use Protocol"));
    assert!(instructions.contains("<tool_call>"));
    assert!(instructions.contains("echo"));
    assert!(instructions.contains("Echoes the input"));
}

#[test]
fn native_dispatcher_includes_text_fallback_instructions() {
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(EchoTool)];
    let dispatcher = NativeToolDispatcher::default();
    let instructions = dispatcher.prompt_instructions(&tools);
    assert!(instructions.contains("<tool_call>"));
    assert!(instructions.contains("Tool Use Protocol") || instructions.contains("工具调用协议"));
}

// ═══════════════════════════════════════════════════════════════════════════
// 24. Clear history
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn clear_history_resets_conversation() {
    let provider = Box::new(ScriptedProvider::new(vec![
        text_response("first"),
        text_response("second"),
    ]));

    let mut agent = build_agent_with(provider, vec![], Box::new(NativeToolDispatcher::default()));

    let _ = agent.turn("hi").await.unwrap();
    assert!(!agent.history().is_empty());

    agent.clear_history();
    assert!(agent.history().is_empty());

    // Next turn should re-inject system prompt
    let _ = agent.turn("hello again").await.unwrap();
    assert!(matches!(
        &agent.history()[0],
        ConversationMessage::Chat(c) if c.role == "system"
    ));
}

// ═══════════════════════════════════════════════════════════════════════════
// 25. run_single delegates to turn
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn run_single_delegates_to_turn() {
    let provider = Box::new(ScriptedProvider::new(vec![text_response("via run_single")]));
    let mut agent = build_agent_with(provider, vec![], Box::new(NativeToolDispatcher::default()));

    let response = agent.run_single("test").await.unwrap();
    assert!(
        !response.is_empty(),
        "Expected non-empty response from run_single"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// VL-NA-011: bounded linear DAG live (opt-in)
// ═══════════════════════════════════════════════════════════════════════════

const LIVE_MODE_CHAT: &str = r#"{"path":"chat_only","reply":"Hi — ready."}"#;
const LIVE_MODE_CHAT_EMPTY: &str =
    r#"{"path":"chat_only","reply":"I compared agent runtimes from memory."}"#;
const LIVE_OBS_CONTINUE: &str = r#"{"verdict":"continue"}"#;
const LIVE_OBS_REPLAN: &str = r#"{"verdict":"replan_remaining"}"#;

#[cfg(feature = "ai-protocol")]
#[tokio::test]
async fn bounded_dag_plan_runs_planner_then_preview() {
    let provider = Box::new(ScriptedProvider::new(vec![text_response("not a dag")]));
    let mut agent = build_agent_with_config(
        provider,
        vec![],
        AgentConfig {
            bounded_dag_live: true,
            envelope_assemble: false,
            ..AgentConfig::default()
        },
    );
    agent.set_host_phase(crate::agent::host_phase::HostPhase::Plan);
    let out = agent.turn("fix the compiler error").await.unwrap();
    assert!(out.contains("Planner output was not a valid"), "{out}");
    assert!(out.contains("locate"), "{out}");
    assert!(out.contains("patch"), "{out}");
    assert!(out.contains("verify"), "{out}");
    assert!(out.contains("Approve Build"), "{out}");
}

#[cfg(feature = "ai-protocol")]
#[tokio::test]
async fn bounded_dag_plan_accepts_planner_json() {
    let json = r#"{"schema_version":"0.1.0","id":"paper-slides","entry":"read","max_steps":8,"nodes":[{"id":"read","task_type":"summarize","model_selector":{"capabilities":["document_understanding"]},"next":"slides"},{"id":"slides","task_type":"write","model_selector":{"capabilities":["speed"]},"next":null}]}"#;
    let mut agent = build_agent_with_config(
        Box::new(ScriptedProvider::new(vec![text_response(json)])),
        vec![],
        AgentConfig {
            bounded_dag_live: true,
            envelope_assemble: false,
            ..AgentConfig::default()
        },
    );
    agent.set_host_phase(crate::agent::host_phase::HostPhase::Plan);
    let out = agent
        .turn("read this paper, write intro slides")
        .await
        .unwrap();
    assert!(out.contains("paper-slides"), "{out}");
    assert!(out.contains("read"), "{out}");
    assert!(out.contains("slides"), "{out}");
    assert!(!out.contains("locate"), "{out}");
}

#[cfg(feature = "ai-protocol")]
#[tokio::test]
async fn bounded_dag_plan_path_skips_planner() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), crate::agent::dag_runner::CODE_FIX_TEMPLATE_JSON).unwrap();
    let mut agent = build_agent_with_config(
        Box::new(FailingProvider),
        vec![],
        AgentConfig {
            bounded_dag_live: true,
            bounded_dag_path: Some(tmp.path().to_string_lossy().into_owned()),
            envelope_assemble: false,
            ..AgentConfig::default()
        },
    );
    agent.set_host_phase(crate::agent::host_phase::HostPhase::Plan);
    let out = agent.turn("fix the compiler error").await.unwrap();
    assert!(out.contains("locate"), "{out}");
    assert!(out.contains("Approve Build"), "{out}");
}

#[cfg(feature = "ai-protocol")]
#[tokio::test]
async fn bounded_dag_build_one_loop_per_node() {
    let provider = ScriptedProvider::new(vec![
        text_response(crate::agent::dag_runner::CODE_FIX_TEMPLATE_JSON),
        text_response("located"),
        text_response(LIVE_OBS_CONTINUE),
        text_response("patched"),
        text_response(LIVE_OBS_CONTINUE),
        text_response("verified"),
    ]);
    let calls = provider.call_counter();
    let mut agent = build_agent_with_config(
        Box::new(provider),
        vec![],
        AgentConfig {
            bounded_dag_live: true,
            envelope_assemble: false,
            ..AgentConfig::default()
        },
    );
    agent.set_host_phase(crate::agent::host_phase::HostPhase::Build);
    let out = agent.turn("fix the compiler error").await.unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        6,
        "first hop DAG plus work+observe per mid node; last hop skips observe"
    );
    assert!(out.contains("verified"), "{out}");
    assert!(
        !out.contains("Working in 3 step(s)"),
        "plan gist is live-only, not persist: {out}"
    );
    assert!(
        !out.contains("### locate"),
        "mid-hop notes are live-only, not persist: {out}"
    );
    assert!(!out.contains("HANDOFF"), "{out}");
    assert!(!out.contains("Bounded task DAG"), "{out}");
}

#[cfg(feature = "ai-protocol")]
#[tokio::test]
async fn bounded_dag_hello_skips_planner() {
    let provider = ScriptedProvider::new(vec![
        text_response(LIVE_MODE_CHAT),
        text_response(LIVE_OBS_CONTINUE),
    ]);
    let calls = provider.call_counter();
    let mut agent = build_agent_with_config(
        Box::new(provider),
        vec![],
        AgentConfig {
            bounded_dag_live: true,
            envelope_assemble: false,
            ..AgentConfig::default()
        },
    );
    agent.set_host_phase(crate::agent::host_phase::HostPhase::Build);
    let out = agent.turn("hello").await.unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "greeting: in-band chat_only reply + observe; no planner"
    );
    assert!(out.contains("Hi"), "{out}");
}

#[cfg(feature = "ai-protocol")]
#[tokio::test]
async fn bounded_dag_follow_up_first_hop_sees_prior_report() {
    let provider = ScriptedProvider::new(vec![
        text_response(LIVE_MODE_CHAT),
        text_response(LIVE_OBS_CONTINUE),
        text_response(r#"{"path":"chat_only","reply":"On piubt we already checked gProxy."}"#),
        text_response(LIVE_OBS_CONTINUE),
    ]);
    let reqs = provider.recorded_requests();
    let calls = provider.call_counter();
    let mut agent = build_agent_with_config(
        Box::new(provider),
        vec![],
        AgentConfig {
            bounded_dag_live: true,
            envelope_assemble: false,
            ..AgentConfig::default()
        },
    );
    agent.set_host_phase(crate::agent::host_phase::HostPhase::Build);
    let first = agent.turn("hello").await.unwrap();
    assert!(first.contains("Hi"), "{first}");
    let _ = agent.turn("你忘了所有操作都在piubt上").await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 4, "two hops + two observe");
    let hop2 = reqs.lock().unwrap()[2].clone();
    assert!(
        hop2.iter().any(|m| m.content.contains("Hi — ready.")),
        "first hop of follow-up must see prior in-band reply, got {hop2:?}"
    );
}

#[cfg(feature = "ai-protocol")]
#[tokio::test]
async fn bounded_dag_single_work_refines_then_runs_nodes() {
    let json = r#"{"schema_version":"0.1.0","id":"paper-slides","entry":"read","max_steps":8,"nodes":[{"id":"read","task_type":"summarize","model_selector":{"capabilities":["document_understanding"]},"next":"slides"},{"id":"slides","task_type":"write","model_selector":{"capabilities":["speed"]},"next":null}]}"#;
    let provider = ScriptedProvider::new(vec![
        text_response(r#"{"path":"single_work"}"#),
        text_response(json),
        text_response("checked"),
        text_response(LIVE_OBS_CONTINUE),
        text_response("synced"),
    ]);
    let calls = provider.call_counter();
    let mut agent = build_agent_with_config(
        Box::new(provider),
        vec![],
        AgentConfig {
            bounded_dag_live: true,
            envelope_assemble: false,
            ..AgentConfig::default()
        },
    );
    agent.set_host_phase(crate::agent::host_phase::HostPhase::Build);
    let out = agent
        .turn("check piubt git then sync velaclaw")
        .await
        .unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        5,
        "single_work + split refine + mid-hop observe; last hop skips observe"
    );
    assert!(out.contains("synced"), "{out}");
}

#[cfg(feature = "ai-protocol")]
#[tokio::test]
async fn bounded_dag_chat_only_observe_upgrades_to_plan() {
    let provider = ScriptedProvider::new(vec![
        text_response(LIVE_MODE_CHAT_EMPTY),
        text_response(LIVE_OBS_REPLAN),
        text_response("not a dag"),
        text_response("not a dag"),
        text_response("located"),
        text_response(LIVE_OBS_CONTINUE),
        text_response("patched"),
        text_response(LIVE_OBS_CONTINUE),
        text_response("verified"),
    ]);
    let calls = provider.call_counter();
    let mut agent = build_agent_with_config(
        Box::new(provider),
        vec![],
        AgentConfig {
            bounded_dag_live: true,
            envelope_assemble: false,
            ..AgentConfig::default()
        },
    );
    agent.set_host_phase(crate::agent::host_phase::HostPhase::Build);
    let out = agent
        .turn("调研主流 agent 编排并对照本机 velaclaw 仓库")
        .await
        .unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        9,
        "in-band chat_only + observe replan + planner retry + mid work+observe; last hop skips observe"
    );
    assert!(out.contains("verified"), "{out}");
}

#[cfg(feature = "ai-protocol")]
#[tokio::test]
async fn bounded_dag_writeback_and_node_contact() {
    let (mem, _tmp) = make_sqlite_memory();
    let provider = ScriptedProvider::new(vec![
        text_response(crate::agent::dag_runner::CODE_FIX_TEMPLATE_JSON),
        text_response("LOCATE_UNIQUE_BODY"),
        text_response(LIVE_OBS_CONTINUE),
        text_response("PATCH_UNIQUE_BODY"),
        text_response(LIVE_OBS_CONTINUE),
        text_response("VERIFY_OK"),
    ]);
    let reqs = provider.recorded_requests();
    let models = provider.recorded_models();
    let mut agent = Agent::builder()
        .provider(Box::new(provider))
        .tools(Vec::<Box<dyn Tool>>::new())
        .memory(mem.clone())
        .observer(make_observer())
        .tool_dispatcher(Box::new(NativeToolDispatcher::default()))
        .workspace_dir(std::env::temp_dir())
        .security(test_security())
        .config(AgentConfig {
            bounded_dag_live: true,
            envelope_assemble: false,
            ..AgentConfig::default()
        })
        .available_hints(vec!["fast".into(), "code".into()])
        .build()
        .unwrap();
    agent.set_session_id("sess-dag");
    agent.set_host_phase(crate::agent::host_phase::HostPhase::Build);
    let out = agent.turn("fix the compiler error").await.unwrap();
    assert!(out.contains("VERIFY_OK"), "{out}");
    assert!(!out.contains("contact model="), "{out}");

    let locate = mem
        .get(&crate::agent::bounded_dag_context::artifact_memory_key(
            "sess-dag", "locate",
        ))
        .await
        .unwrap()
        .expect("locate artifact");
    assert!(
        locate.content.contains("LOCATE_UNIQUE_BODY"),
        "{}",
        locate.content
    );

    let captured = reqs.lock().unwrap();
    assert_eq!(captured.len(), 6);
    let patch_msgs = &captured[3];
    assert!(
        !patch_msgs
            .iter()
            .any(|m| m.role == "assistant" && m.content.contains("LOCATE_UNIQUE_BODY")),
        "patch node must not carry prior assistant dump: {patch_msgs:?}"
    );
    assert!(
        patch_msgs
            .iter()
            .any(|m| m.content.contains("dag_artifact") && m.content.contains("LOCATE_UNIQUE_BODY")),
        "patch node should retrieve clipped locate artifact: {patch_msgs:?}"
    );

    let used = models.lock().unwrap().clone();
    assert_eq!(
        used.len(),
        6,
        "first hop DAG + 3 work + 2 mid observe (last hop skips observe); got {used:?}"
    );
    assert_eq!(
        used[1],
        "hint:code".to_string(),
        "locate follows Contact; got {used:?}"
    );
    assert_eq!(
        used[3],
        "hint:code".to_string(),
        "patch follows Contact; got {used:?}"
    );
    assert_eq!(
        used[5],
        "hint:fast".to_string(),
        "verify follows Contact; got {used:?}"
    );
}

#[cfg(feature = "ai-protocol")]
#[tokio::test]
async fn bounded_dag_session_picker_runs_work_hops() {
    let provider = ScriptedProvider::new(vec![
        text_response(crate::agent::dag_runner::CODE_FIX_TEMPLATE_JSON),
        text_response("located"),
        text_response(LIVE_OBS_CONTINUE),
        text_response("patched"),
        text_response(LIVE_OBS_CONTINUE),
        text_response("verified"),
    ]);
    let models = provider.recorded_models();
    let mut agent = Agent::builder()
        .provider(Box::new(provider))
        .tools(Vec::<Box<dyn Tool>>::new())
        .memory(make_memory())
        .observer(make_observer())
        .tool_dispatcher(Box::new(NativeToolDispatcher::default()))
        .workspace_dir(std::env::temp_dir())
        .security(test_security())
        .config(AgentConfig {
            bounded_dag_live: true,
            envelope_assemble: false,
            ..AgentConfig::default()
        })
        .available_hints(vec!["fast".into(), "code".into()])
        .build()
        .unwrap();
    agent.set_explicit_model(Some("nvidia/nemotron-3-ultra-550b-a55b".into()));
    agent.set_host_phase(crate::agent::host_phase::HostPhase::Build);
    let out = agent.turn("fix the compiler error").await.unwrap();
    assert!(out.contains("verified"), "{out}");
    assert!(!out.contains("contact model="), "{out}");
    let used = models.lock().unwrap().clone();
    assert_eq!(
        used[1],
        "nvidia/nemotron-3-ultra-550b-a55b".to_string(),
        "work hops must use the session picker; got {used:?}"
    );
    assert_eq!(
        used[3],
        "nvidia/nemotron-3-ultra-550b-a55b".to_string(),
        "got {used:?}"
    );
    assert_eq!(
        used[5],
        "nvidia/nemotron-3-ultra-550b-a55b".to_string(),
        "got {used:?}"
    );
}
