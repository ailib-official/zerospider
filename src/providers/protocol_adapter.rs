//! Protocol-backed provider adapter.
//!
//! Bridges ai-lib-rust's AiClient to ZeroSpider's Provider trait,
//! enabling protocol-driven provider configuration.
//! 协议适配器负责将 ai-lib-rust 客户端桥接到本地 Provider 接口。
//!
//! Uses ai-lib-rust 0.8+ features:
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
const MAX_RETRIES: u32 = 2;

pub struct ProtocolBackedProvider {
    client: Arc<ai_lib_rust::AiClient>,
    provider_id: String,
    model_id: String,
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
            .map_err(|e| anyhow::anyhow!("Failed to build client for {}: {}", model, e))?;

        Ok(Self {
            client: Arc::new(client),
            provider_id: provider_id.to_string(),
            model_id: model_id.to_string(),
        })
    }

    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    fn convert_messages(messages: &[ChatMessage]) -> Vec<ai_lib_rust::Message> {
        messages
            .iter()
            .map(|m| match m.role.as_str() {
                "system" => ai_lib_rust::Message::system(&m.content),
                "user" => ai_lib_rust::Message::user(&m.content),
                "assistant" => ai_lib_rust::Message::assistant(&m.content),
                _ => ai_lib_rust::Message::user(&m.content),
            })
            .collect()
    }

    /// Run a chat execute with retry on retryable errors.
    async fn execute_chat_with_retry(
        client: &ai_lib_rust::AiClient,
        messages: Vec<ai_lib_rust::Message>,
        temperature: f64,
        tools: Option<Vec<serde_json::Value>>,
    ) -> Result<ai_lib_rust::client::UnifiedResponse, ai_lib_rust::Error> {
        let mut builder = client
            .chat()
            .messages(messages.clone())
            .temperature(temperature);
        if let Some(ref t) = tools {
            if !t.is_empty() {
                builder = builder.tools_json(t.clone());
            }
        }
        let mut last_err = match builder.execute().await {
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
                .temperature(temperature);
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
        _model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let mut messages = Vec::new();
        if let Some(sys) = system_prompt {
            messages.push(ai_lib_rust::Message::system(sys));
        }
        messages.push(ai_lib_rust::Message::user(message));

        let response =
            Self::execute_chat_with_retry(self.client.as_ref(), messages, temperature, None)
                .await
                .map_err(|e| anyhow::anyhow!("Protocol provider error: {}", e))?;

        Ok(response.content)
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        _model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let converted = Self::convert_messages(messages);

        let response =
            Self::execute_chat_with_retry(self.client.as_ref(), converted, temperature, None)
                .await
                .map_err(|e| anyhow::anyhow!("Protocol provider error: {}", e))?;

        Ok(response.content)
    }

    async fn chat(
        &self,
        request: ChatRequest<'_>,
        _model: &str,
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

        let response =
            Self::execute_chat_with_retry(self.client.as_ref(), converted, temperature, tools)
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
        _model: &str,
        temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        let converted = Self::convert_messages(messages);

        let tools_opt = if tools.is_empty() {
            None
        } else {
            Some(tools.to_vec())
        };

        let response =
            Self::execute_chat_with_retry(self.client.as_ref(), converted, temperature, tools_opt)
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
        _model: &str,
        temperature: f64,
        _options: StreamOptions,
    ) -> stream::BoxStream<'static, StreamResult<StreamChunk>> {
        let mut messages = Vec::new();
        if let Some(sys) = system_prompt {
            messages.push(ai_lib_rust::Message::system(sys));
        }
        messages.push(ai_lib_rust::Message::user(message));

        let client = Arc::clone(&self.client);

        async_stream::try_stream! {
            let mut stream = client.chat()
                .messages(messages)
                .temperature(temperature)
                .stream()
                .execute_stream()
                .await
                .map_err(|e| crate::providers::traits::StreamError::Provider(e.to_string()))?;

            while let Some(event) = stream.next().await {
                match event {
                    Ok(ai_lib_rust::StreamingEvent::PartialContentDelta { content, .. }) => {
                        if !content.is_empty() {
                            yield StreamChunk::delta(content).with_token_estimate();
                        }
                    }
                    Ok(ai_lib_rust::StreamingEvent::StreamEnd { .. }) => {
                        yield StreamChunk::final_chunk();
                        break;
                    }
                    Ok(_) => {}
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

    #[test]
    fn test_convert_messages() {
        let messages = vec![
            ChatMessage::system("You are helpful."),
            ChatMessage::user("Hello"),
        ];
        let converted = ProtocolBackedProvider::convert_messages(&messages);
        assert_eq!(converted.len(), 2);
    }
}
