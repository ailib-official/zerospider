//! Protocol-backed provider adapter.
//!
//! Bridges ai-lib-rust's AiClient to ZeroSpider's Provider trait,
//! enabling protocol-driven provider configuration.
//! 协议适配器负责将 ai-lib-rust 客户端桥接到本地 Provider 接口。
//!
//! Uses ai-lib-rust 0.9+ features:
//! - `tools_json()` for tool/function calling
//! - `Error::is_retryable()` / `retry_after()` for automatic retries on rate limits

use crate::providers::traits::{
    ChatMessage, ChatRequest, ChatResponse, Provider, ProviderCapabilities, StreamChunk,
    StreamOptions, StreamResult, ToolCall, ToolsPayload,
};
use crate::tools::ToolSpec;
use async_trait::async_trait;
use futures_util::{stream, StreamExt};
use std::sync::Arc;
use std::time::Duration;

/// Max retries for retryable protocol errors.
///
/// Retry policy: only `ai_lib_rust::Error::is_retryable` / `retry_after` drive retries here.
/// Higher-level resilience (circuit breakers, etc.) belongs in ai-lib contact/policy — do not duplicate.
const MAX_RETRIES: u32 = 2;

fn effective_model_id(provider_id: &str, model_id: &str, override_model: &str) -> String {
    let t = override_model.trim();
    if t.is_empty() {
        format!("{provider_id}/{model_id}")
    } else {
        t.to_string()
    }
}

pub struct ProtocolBackedProvider {
    client: Arc<ai_lib_rust::AiClient>,
    provider_id: String,
    model_id: String,
}

/// Build an [`ai_lib_rust::AiClient`] for a logical model id (`provider/model`).
pub async fn resolve_ai_client(model_id: &str) -> anyhow::Result<ai_lib_rust::AiClient> {
    ai_lib_rust::AiClient::new(model_id).await.map_err(|e| {
        let base = format!("AiClient::new({model_id}): {e}");
        if model_id.contains('/') {
            anyhow::anyhow!(
                "{base}\n\
                 Hint: logical `provider/model` ids need a local ai-protocol checkout — set `AI_PROTOCOL_DIR` \
                 to the repository root (a directory on disk, not a URL). See `docs/migration-legacy-to-protocol.md`."
            )
        } else {
            anyhow::anyhow!(base)
        }
    })
}

impl ProtocolBackedProvider {
    pub fn new(
        provider_id: &str,
        model_id: &str,
        _credential: Option<&str>,
    ) -> anyhow::Result<Self> {
        let model = format!("{}/{}", provider_id, model_id);

        let client = tokio::runtime::Handle::try_current()
            .map(|h| h.block_on(async { ai_lib_rust::AiClient::new(&model).await }))
            .unwrap_or_else(|_| {
                let rt = tokio::runtime::Runtime::new()?;
                rt.block_on(async { ai_lib_rust::AiClient::new(&model).await })
            })
            .map_err(|e| {
                let base = format!("Failed to build client for {model}: {e}");
                anyhow::anyhow!(
                    "{base}\n\
                     Hint: set `AI_PROTOCOL_DIR` to a local ai-protocol checkout when using `provider/model` ids. \
See `docs/migration-legacy-to-protocol.md`."
                )
            })?;

        Ok(Self {
            client: Arc::new(client),
            provider_id: provider_id.to_string(),
            model_id: model_id.to_string(),
        })
    }

    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    /// Logical model id for `ChatRequestBuilder::model` — uses `provider/model` unless caller overrides.
    fn effective_model(&self, override_model: &str) -> String {
        effective_model_id(&self.provider_id, &self.model_id, override_model)
    }

    fn convert_messages(messages: &[ChatMessage]) -> Vec<ai_lib_rust::Message> {
        messages
            .iter()
            .map(|m| match m.role.as_str() {
                "system" => ai_lib_rust::Message::system(&m.content),
                "assistant" => ai_lib_rust::Message::assistant(&m.content),
                "tool" => {
                    if let Some(ref id) = m.tool_call_id {
                        ai_lib_rust::Message::tool(id.as_str(), &m.content)
                    } else {
                        ai_lib_rust::Message::user(format!(
                            "[tool role without tool_call_id] {}",
                            m.content
                        ))
                    }
                }
                _ => ai_lib_rust::Message::user(&m.content),
            })
            .collect()
    }

    fn stream_event_to_chunk(event: ai_lib_rust::StreamingEvent) -> Option<StreamChunk> {
        match event {
            ai_lib_rust::StreamingEvent::PartialContentDelta { content, .. } => {
                (!content.is_empty()).then(|| StreamChunk::delta(content).with_token_estimate())
            }
            ai_lib_rust::StreamingEvent::ThinkingDelta { thinking, .. } => (!thinking.is_empty())
                .then(|| {
                    StreamChunk::delta(format!("[thinking] {thinking}")).with_token_estimate()
                }),
            ai_lib_rust::StreamingEvent::ToolCallStarted {
                tool_call_id,
                tool_name,
                index,
            } => Some(StreamChunk::tool_call_started(
                tool_call_id,
                tool_name,
                index,
            )),
            ai_lib_rust::StreamingEvent::PartialToolCall {
                tool_call_id,
                arguments,
                index,
                is_complete,
            } => Some(StreamChunk::tool_call_arguments(
                tool_call_id,
                arguments,
                index,
                is_complete,
            )),
            ai_lib_rust::StreamingEvent::ToolCallEnded {
                tool_call_id,
                index,
            } => Some(StreamChunk::tool_call_ended(tool_call_id, index)),
            ai_lib_rust::StreamingEvent::StreamEnd { .. } => Some(StreamChunk::final_chunk()),
            ai_lib_rust::StreamingEvent::StreamError { error, .. } => Some(StreamChunk::error(
                format!("Protocol stream error: {error}"),
            )),
            ai_lib_rust::StreamingEvent::Metadata { .. }
            | ai_lib_rust::StreamingEvent::FinalCandidate { .. } => None,
        }
    }

    /// Run a chat execute with retry on retryable errors.
    ///
    /// **Boundary (migration plan Phase 4–5):** this is **transport-level** retry only
    /// (`Error::is_retryable` / `retry_after` inside the ai-lib stack). It does **not**
    /// replace `[reliability]` fallback chains or per-config `ReliableProvider` switching;
    /// those stay in the app layer. Optional `ai-lib-rust` `routing_mvp` affects client
    /// construction upstream; we do not duplicate routing policy here.
    async fn execute_chat_with_retry(
        client: &ai_lib_rust::AiClient,
        logical_model: &str,
        messages: Vec<ai_lib_rust::Message>,
        temperature: f64,
        tools: Option<Vec<serde_json::Value>>,
    ) -> Result<ai_lib_rust::client::UnifiedResponse, ai_lib_rust::Error> {
        let mut builder = client
            .chat()
            .messages(messages.clone())
            .temperature(temperature)
            .model(logical_model);
        if let Some(ref t) = tools {
            if !t.is_empty() {
                builder = builder.tools_json(t.clone());
            }
        }
        let mut last_err: ai_lib_rust::Error = match builder.execute().await {
            Ok(r) => return Ok(r),
            Err(e) => e,
        };
        for attempt in 1..=MAX_RETRIES {
            if !last_err.is_retryable() {
                break;
            }
            if let Some(delay) = last_err.retry_after() {
                tracing::debug!(
                    "Protocol retry attempt {} after {:?} (retry_after)",
                    attempt,
                    delay
                );
                tokio::time::sleep(delay).await;
            } else {
                let backoff = Duration::from_millis(500 * (1 << attempt));
                tracing::debug!(
                    "Protocol retry attempt {} after {:?} (exponential backoff)",
                    attempt,
                    backoff
                );
                tokio::time::sleep(backoff).await;
            }
            let mut builder = client
                .chat()
                .messages(messages.clone())
                .temperature(temperature)
                .model(logical_model);
            if let Some(ref t) = tools {
                if !t.is_empty() {
                    builder = builder.tools_json(t.clone());
                }
            }
            last_err = match builder.execute().await {
                Ok(r) => return Ok(r),
                Err(e) => e,
            };
        }
        Err(last_err)
    }
}

#[async_trait]
impl Provider for ProtocolBackedProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: true,
            vision: true,
        }
    }

    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let mut messages = Vec::new();
        if let Some(sys) = system_prompt {
            messages.push(ai_lib_rust::Message::system(sys));
        }
        messages.push(ai_lib_rust::Message::user(message));

        let logical = self.effective_model(model);
        let response = Self::execute_chat_with_retry(
            self.client.as_ref(),
            &logical,
            messages,
            temperature,
            None,
        )
        .await
        .map_err(|e| anyhow::anyhow!("Protocol provider error: {}", e))?;

        Ok(response.content)
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let converted = Self::convert_messages(messages);

        let logical = self.effective_model(model);
        let response = Self::execute_chat_with_retry(
            self.client.as_ref(),
            &logical,
            converted,
            temperature,
            None,
        )
        .await
        .map_err(|e| anyhow::anyhow!("Protocol provider error: {}", e))?;

        Ok(response.content)
    }

    async fn chat(
        &self,
        request: ChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        let converted = Self::convert_messages(request.messages);

        let tools = request.tools.map(|tools| {
            tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters
                        }
                    })
                })
                .collect::<Vec<_>>()
        });

        let logical = self.effective_model(model);
        let response = Self::execute_chat_with_retry(
            self.client.as_ref(),
            &logical,
            converted,
            temperature,
            tools,
        )
        .await
        .map_err(|e| anyhow::anyhow!("Protocol provider error: {}", e))?;

        Ok(ChatResponse {
            text: Some(response.content),
            tool_calls: response
                .tool_calls
                .into_iter()
                .map(|tc| ToolCall {
                    id: tc.id,
                    name: tc.name,
                    arguments: tc.arguments.to_string(),
                })
                .collect(),
        })
    }

    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: &[serde_json::Value],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        let converted = Self::convert_messages(messages);

        let tools_opt = if tools.is_empty() {
            None
        } else {
            Some(tools.to_vec())
        };

        let logical = self.effective_model(model);
        let response = Self::execute_chat_with_retry(
            self.client.as_ref(),
            &logical,
            converted,
            temperature,
            tools_opt,
        )
        .await
        .map_err(|e| anyhow::anyhow!("Protocol provider error: {}", e))?;

        Ok(ChatResponse {
            text: Some(response.content),
            tool_calls: response
                .tool_calls
                .into_iter()
                .map(|tc| ToolCall {
                    id: tc.id,
                    name: tc.name,
                    arguments: tc.arguments.to_string(),
                })
                .collect(),
        })
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    fn stream_chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
        _options: StreamOptions,
    ) -> stream::BoxStream<'static, StreamResult<StreamChunk>> {
        let mut messages = Vec::new();
        if let Some(sys) = system_prompt {
            messages.push(ai_lib_rust::Message::system(sys));
        }
        messages.push(ai_lib_rust::Message::user(message));

        let client = Arc::clone(&self.client);
        let logical = self.effective_model(model);

        async_stream::try_stream! {
            let mut stream = client.chat()
                .messages(messages)
                .temperature(temperature)
                .model(&logical)
                .stream()
                .execute_stream()
                .await
                .map_err(|e| crate::providers::traits::StreamError::Provider(e.to_string()))?;

            while let Some(event) = stream.next().await {
                match event {
                    Ok(event) => {
                        if let Some(chunk) = Self::stream_event_to_chunk(event) {
                            let done = chunk.is_final;
                            yield chunk;
                            if done {
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        yield StreamChunk::error(e.to_string());
                        break;
                    }
                }
            }
        }
        .boxed()
    }

    fn convert_tools(&self, tools: &[ToolSpec]) -> ToolsPayload {
        let tools_json: Vec<serde_json::Value> = tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters
                    }
                })
            })
            .collect();

        ToolsPayload::OpenAI { tools: tools_json }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::traits::StreamToolCallDelta;

    #[test]
    fn test_convert_messages() {
        let messages = vec![
            ChatMessage::system("You are helpful."),
            ChatMessage::user("Hello"),
        ];
        let converted = ProtocolBackedProvider::convert_messages(&messages);
        assert_eq!(converted.len(), 2);
    }

    #[test]
    fn test_convert_messages_tool_role_with_call_id() {
        let messages = vec![ChatMessage::tool_with_call_id("call_1", "result json")];
        let converted = ProtocolBackedProvider::convert_messages(&messages);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].tool_call_id.as_deref(), Some("call_1"));
    }

    #[test]
    fn convert_messages_preserves_multi_turn_tool_conversation() {
        let messages = vec![
            ChatMessage::user("Find the current status"),
            ChatMessage::assistant("I will call a tool."),
            ChatMessage::tool_with_call_id("call_1", r#"{"status":"ok"}"#),
            ChatMessage::assistant("The status is ok."),
        ];
        let converted = ProtocolBackedProvider::convert_messages(&messages);

        assert_eq!(converted.len(), 4);
        assert_eq!(converted[2].tool_call_id.as_deref(), Some("call_1"));

        let serialized = serde_json::to_value(&converted).expect("serialize messages");
        assert_eq!(serialized[0]["role"], "user");
        assert_eq!(serialized[1]["role"], "assistant");
        assert_eq!(serialized[2]["role"], "tool");
        assert_eq!(serialized[3]["role"], "assistant");
    }

    #[test]
    fn stream_event_to_chunk_maps_content_and_thinking() {
        let content = ProtocolBackedProvider::stream_event_to_chunk(
            ai_lib_rust::StreamingEvent::PartialContentDelta {
                content: "hello".to_string(),
                sequence_id: Some(1),
            },
        )
        .expect("content chunk");
        assert_eq!(content.delta, "hello");
        assert!(content.tool_call_delta.is_none());
        assert!(!content.is_final);
        assert!(content.token_count > 0);

        let thinking = ProtocolBackedProvider::stream_event_to_chunk(
            ai_lib_rust::StreamingEvent::ThinkingDelta {
                thinking: "checking".to_string(),
                tool_consideration: None,
            },
        )
        .expect("thinking chunk");
        assert_eq!(thinking.delta, "[thinking] checking");
        assert!(thinking.tool_call_delta.is_none());
    }

    #[test]
    fn stream_event_to_chunk_emits_tool_call_deltas() {
        let started = ProtocolBackedProvider::stream_event_to_chunk(
            ai_lib_rust::StreamingEvent::ToolCallStarted {
                tool_call_id: "call_1".to_string(),
                tool_name: "lookup".to_string(),
                index: Some(0),
            },
        )
        .expect("tool start chunk");
        assert_eq!(
            started.tool_call_delta,
            Some(StreamToolCallDelta::Started {
                id: "call_1".to_string(),
                name: "lookup".to_string(),
                index: Some(0),
            })
        );

        let partial = ProtocolBackedProvider::stream_event_to_chunk(
            ai_lib_rust::StreamingEvent::PartialToolCall {
                tool_call_id: "call_1".to_string(),
                arguments: r#"{"query":"zerospider"}"#.to_string(),
                index: Some(0),
                is_complete: Some(false),
            },
        )
        .expect("tool arguments chunk");
        assert_eq!(
            partial.tool_call_delta,
            Some(StreamToolCallDelta::Arguments {
                id: "call_1".to_string(),
                arguments: r#"{"query":"zerospider"}"#.to_string(),
                index: Some(0),
                is_complete: Some(false),
            })
        );

        let ended = ProtocolBackedProvider::stream_event_to_chunk(
            ai_lib_rust::StreamingEvent::ToolCallEnded {
                tool_call_id: "call_1".to_string(),
                index: Some(0),
            },
        )
        .expect("tool end chunk");
        assert_eq!(
            ended.tool_call_delta,
            Some(StreamToolCallDelta::Ended {
                id: "call_1".to_string(),
                index: Some(0),
            })
        );
    }

    #[test]
    fn stream_event_to_chunk_handles_end_and_ignores_metadata() {
        let final_chunk =
            ProtocolBackedProvider::stream_event_to_chunk(ai_lib_rust::StreamingEvent::StreamEnd {
                finish_reason: Some("stop".to_string()),
            })
            .expect("final chunk");
        assert!(final_chunk.is_final);

        let metadata =
            ProtocolBackedProvider::stream_event_to_chunk(ai_lib_rust::StreamingEvent::Metadata {
                usage: Some(serde_json::json!({"input_tokens": 1})),
                finish_reason: None,
                stop_reason: None,
            });
        assert!(metadata.is_none());
    }

    #[test]
    fn effective_model_id_empty_override_is_provider_slash_model() {
        assert_eq!(effective_model_id("openai", "gpt-4o", ""), "openai/gpt-4o");
    }

    #[test]
    fn effective_model_id_non_empty_override_wins() {
        assert_eq!(
            effective_model_id("openai", "gpt-4o", "anthropic/claude-3-5-sonnet"),
            "anthropic/claude-3-5-sonnet"
        );
    }
}
