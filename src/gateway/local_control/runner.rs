//! Agent-loop chat execution for Local Control API (VL-ARCH-001).
//! 本地控制 API 的 agent 循环对话执行（VL-ARCH-001）。

use super::session_title::SessionTitleHub;
use super::sessions::ChatSessionStore;
use super::types::{ChatApiRequest, ChatApiResponse, ChatMessageInput};
use crate::agent::agent::Agent;
use crate::config::Config;
use crate::protocol_registry::{
    provider_id_from_logical, resolve_local_protocol_root, scan_protocol_root,
};
use crate::providers::ChatMessage;
use anyhow::{anyhow, Context, Result};
use std::sync::Arc;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Config for one Web/API turn. Live bounded DAG keeps session `default_model`
/// (UI picker must not send first-hop/observe to a hung aggregator id).
pub fn effective_chat_config(config: &Config, req: &ChatApiRequest) -> Config {
    if config.agent.bounded_dag_live {
        config.clone()
    } else {
        apply_chat_overrides(config.clone(), req)
    }
}

/// Apply per-request model/temperature overrides onto a config clone.
pub fn apply_chat_overrides(mut config: Config, req: &ChatApiRequest) -> Config {
    if let Some(model_id) = &req.model_id {
        let trimmed = model_id.trim();
        // Only honor protocol `provider/model` ids. Bare labels like `deepseek-chat`
        // from older UI session metadata must not clobber the configured default.
        if trimmed.contains('/') {
            let (logical_id, provider) = resolve_chat_model_override(trimmed);
            config.default_model = Some(logical_id);
            config.default_provider = Some(provider);
        }
    }
    if let Some(temp) = req.temperature {
        config.default_temperature = temp;
    }
    config
}

/// Map a chat picker/session model id to `(logical_id, provider)`.
///
/// Composed logical ids under a known provider stay as-is. Bare aggregator
/// wire ids (e.g. `deepseek-ai/deepseek-v4-flash`) remap uniquely via the
/// local protocol registry when possible.
fn resolve_chat_model_override(raw: &str) -> (String, String) {
    let first = provider_id_from_logical(raw).to_string();
    let Some(root) = resolve_local_protocol_root() else {
        return (raw.to_string(), first);
    };
    let Ok(snap) = scan_protocol_root(&root) else {
        return (raw.to_string(), first);
    };
    if snap.provider_by_id(&first).is_some() {
        return (raw.to_string(), first);
    }
    if let Some(entry) = snap.resolve_chat_model_id(raw) {
        return (entry.logical_id.clone(), entry.provider.clone());
    }
    (raw.to_string(), first)
}

/// Returns the last non-empty user message from the chat history payload.
pub fn extract_last_user_message(messages: &[ChatMessageInput]) -> Result<String> {
    for msg in messages.iter().rev() {
        if msg.role == "user" {
            let trimmed = msg.content.trim();
            if !trimmed.is_empty() {
                return Ok(trimmed.to_string());
            }
        }
    }
    Err(anyhow!(
        "messages must include at least one non-empty user message"
    ))
}

/// Extract explicit Web picker model id (`provider/model` only).
pub fn explicit_model_from_request(req: &ChatApiRequest) -> Option<String> {
    req.model_id.as_ref().and_then(|raw| {
        let trimmed = raw.trim();
        if trimmed.contains('/') {
            let (logical_id, _) = resolve_chat_model_override(trimmed);
            Some(logical_id)
        } else {
            None
        }
    })
}

/// Run a single agent turn via `Agent::from_config` + `turn` (full tool loop).
pub async fn run_agent_chat(
    config: &Config,
    req: &ChatApiRequest,
    approval_hub: Option<&Arc<crate::approval::ApprovalHub>>,
    human_input_hub: Option<&Arc<crate::approval::HumanInputHub>>,
    cancellation: Option<CancellationToken>,
    progress_tx: Option<Sender<crate::agent::turn_progress::TurnProgress>>,
) -> Result<ChatApiResponse> {
    let user_message = extract_last_user_message(&req.messages)?;
    let explicit_model = if config.agent.bounded_dag_live {
        None
    } else {
        explicit_model_from_request(req)
    };
    let effective_config = effective_chat_config(config, req);

    let mut agent = Agent::from_config(&effective_config).context("failed to build agent")?;
    if let Some(sid) = req
        .session_id
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        agent.set_session_id(sid.to_string());
    }
    agent.set_explicit_model(explicit_model);
    agent.set_host_phase(crate::agent::host_phase::HostPhase::parse_opt(
        req.host_phase.as_deref(),
    ));
    if let Some(hub) = approval_hub {
        agent
            .enable_gateway_approval(Arc::clone(hub), &effective_config)
            .context("wire gateway approval manager")?;
    }
    if let Some(hub) = human_input_hub {
        agent.enable_gateway_hitl(Arc::clone(hub));
    }
    // Seed prior turns so multi-step Web UI chat keeps context (UI sends full history).
    seed_prior_messages(&mut agent, &req.messages)?;
    agent.set_cancellation_token(cancellation);
    agent.set_progress_tx(progress_tx);
    let content = Box::pin(agent.turn(&user_message))
        .await
        .context("agent turn failed")?;

    #[cfg(feature = "ai-protocol")]
    let (selected_model, model_selection_reason) = match agent.last_turn_model() {
        Some(d) => (Some(d.model.clone()), Some(d.reason.clone())),
        None => (None, None),
    };
    #[cfg(not(feature = "ai-protocol"))]
    let (selected_model, model_selection_reason) = (None, None);

    Ok(ChatApiResponse {
        id: format!("chat_{}", Uuid::new_v4()),
        content,
        usage: None,
        cost: None,
        selected_model,
        model_selection_reason,
    })
}

/// Inject all messages before the last user turn into a fresh agent (system prompt first).
fn seed_prior_messages(agent: &mut Agent, messages: &[ChatMessageInput]) -> Result<()> {
    let Some(last_user_idx) = messages
        .iter()
        .rposition(|m| m.role == "user" && !m.content.trim().is_empty())
    else {
        return Ok(());
    };
    let prior = &messages[..last_user_idx];
    if prior.is_empty() {
        return Ok(());
    }
    agent.ensure_system_prompt()?;
    for msg in prior {
        let content = msg.content.trim();
        if content.is_empty() {
            continue;
        }
        match msg.role.as_str() {
            "user" => agent.push_chat_message(ChatMessage::user(content)),
            "assistant" => agent.push_chat_message(ChatMessage::assistant(content)),
            // Agent already owns the system prompt; ignore prior system turns.
            _ => {}
        }
    }
    Ok(())
}

/// Append the latest user turn and assistant reply to a persisted session, if `session_id` is set.
///
/// User text is persisted at turn start (Web) so a cancel still keeps the original prompt.
/// Completed turns append the assistant; cancelled turns append `Stopped.`.
///
/// After the first persisted user turn, schedules a **background** title completion (does not
/// block the chat `done` frame). Model preference: local (ollama / llamacpp /
/// lmstudio) → `nvidia/nemotron-3-super-120b-a12b` → `nvidia/nemotron-mini-4b-instruct`.
pub async fn persist_chat_turn(
    config: &Config,
    session_id: Option<&str>,
    req: &ChatApiRequest,
    assistant_content: &str,
    title_hub: Option<Arc<SessionTitleHub>>,
) -> Result<()> {
    persist_user_message(config, session_id, req, title_hub.clone()).await?;
    persist_assistant_message(config, session_id, req, assistant_content).await
}

pub async fn persist_user_message(
    config: &Config,
    session_id: Option<&str>,
    req: &ChatApiRequest,
    title_hub: Option<Arc<SessionTitleHub>>,
) -> Result<()> {
    let Some(id) = session_id.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(());
    };

    let user_message = extract_last_user_message(&req.messages)?;
    let store = ChatSessionStore::new(&config.workspace_dir);
    let to_store = vec![ChatMessageInput {
        role: "user".into(),
        content: user_message,
    }];
    let append = store
        .append_messages(id, &to_store, req.model_id.as_deref())
        .await?;

    if append.needs_title_refine && store.mark_title_refined(id).await.is_ok() {
        let config = config.clone();
        let session_id = id.to_string();
        tokio::spawn(async move {
            refine_session_title_background(config, session_id, title_hub.clone()).await;
        });
    }
    Ok(())
}

pub async fn persist_assistant_message(
    config: &Config,
    session_id: Option<&str>,
    req: &ChatApiRequest,
    assistant_content: &str,
) -> Result<()> {
    let Some(id) = session_id.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(());
    };
    let store = ChatSessionStore::new(&config.workspace_dir);
    let to_store = vec![ChatMessageInput {
        role: "assistant".into(),
        content: assistant_content.to_string(),
    }];
    store
        .append_messages(id, &to_store, req.model_id.as_deref())
        .await?;
    Ok(())
}

const TITLE_SYSTEM: &str = "You name chat sessions. Reply with ONLY a concise title \
(max ~40 characters). No quotes, no punctuation wrapper, no explanation.";

/// Primary NVIDIA Nemotron for background title tasks (free tier, strong instruction following).
const TITLE_NEMOTRON_PRIMARY: &str = "nvidia/nemotron-3-super-120b-a12b";
/// Last-resort smallest Nemotron when primary is unavailable.
const TITLE_NEMOTRON_FALLBACK: &str = "nvidia/nemotron-mini-4b-instruct";

fn is_local_title_provider(provider: &str) -> bool {
    matches!(
        provider.to_ascii_lowercase().as_str(),
        "ollama" | "llamacpp" | "lmstudio"
    )
}

fn tcp_open_quick(host_port: &str) -> bool {
    use std::net::{TcpStream, ToSocketAddrs};
    use std::time::Duration;
    let Ok(mut addrs) = host_port.to_socket_addrs() else {
        return false;
    };
    let Some(addr) = addrs.next() else {
        return false;
    };
    TcpStream::connect_timeout(&addr, Duration::from_millis(150)).is_ok()
}

/// Strip optional `http(s)://` so `OLLAMA_HOST` can be probed over TCP.
fn host_port_from_urlish(raw: &str) -> String {
    let trimmed = raw.trim();
    let without_scheme = trimmed
        .strip_prefix("http://")
        .or_else(|| trimmed.strip_prefix("https://"))
        .unwrap_or(trimmed);
    without_scheme
        .split('/')
        .next()
        .unwrap_or(without_scheme)
        .to_string()
}

/// Prefer a reachable local runtime; do not treat keyless providers as "present".
fn detected_local_title_model() -> Option<String> {
    let ollama_host = std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "127.0.0.1:11434".into());
    if tcp_open_quick(&host_port_from_urlish(&ollama_host)) {
        return Some("ollama/llama3.2".into());
    }
    // LM Studio default OpenAI-compatible port.
    if tcp_open_quick("127.0.0.1:1234") {
        return Some("lmstudio/local-model".into());
    }
    None
}

/// Ordered candidates: local (configured or detected) → smallest Nemotron.
#[must_use]
pub(crate) fn title_refine_model_candidates(config: &Config) -> Vec<String> {
    let mut out = Vec::new();

    let configured = crate::execution::logical_model_id_from_config(config);
    let configured_provider = provider_id_from_logical(&configured);
    if is_local_title_provider(configured_provider) {
        out.push(configured);
    } else if let Some(local) = detected_local_title_model() {
        out.push(local);
    }

    if !out.iter().any(|m| m == TITLE_NEMOTRON_PRIMARY) {
        out.push(TITLE_NEMOTRON_PRIMARY.to_string());
    }
    if !out.iter().any(|m| m == TITLE_NEMOTRON_FALLBACK) {
        out.push(TITLE_NEMOTRON_FALLBACK.to_string());
    }
    out
}

async fn refine_session_title_background(
    config: Config,
    session_id: String,
    title_hub: Option<Arc<SessionTitleHub>>,
) {
    let store = ChatSessionStore::new(&config.workspace_dir);
    let Some(session) = (match store.get(&session_id).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "title refine: load session failed");
            return;
        }
    }) else {
        return;
    };

    let seed = super::sessions::title_refine_seed(&session.messages);
    if seed.trim().is_empty() {
        return;
    }

    let user_prompt = format!(
        "Name this chat from the original user task only. Ignore stop/interrupt follow-ups.\n\n{seed}"
    );
    let candidates = title_refine_model_candidates(&config);

    for logical in candidates {
        let mut effective = config.clone();
        effective.default_model = Some(logical.clone());
        effective.default_provider = Some(provider_id_from_logical(&logical).to_string());

        let assembled = match crate::agent::assemble::assemble_runtime(
            &effective,
            crate::config::BootstrapOptions {
                with_embedding_routes: false,
            },
        ) {
            Ok(a) => a,
            Err(e) => {
                tracing::debug!(
                    model = %logical,
                    error = %format!("{e:#}"),
                    "title refine: assemble skipped candidate"
                );
                continue;
            }
        };

        match assembled
            .provider
            .chat_with_system(Some(TITLE_SYSTEM), &user_prompt, &assembled.model_name, 0.2)
            .await
        {
            Ok(raw) => {
                let cleaned = super::sessions::sanitize_generated_title(&raw);
                if cleaned.is_empty() || !super::sessions::is_acceptable_generated_title(&cleaned) {
                    tracing::debug!(
                        model = %logical,
                        raw = %raw,
                        "title refine: rejected weak title"
                    );
                    continue;
                }
                if let Err(e) = store.set_refined_title(&session_id, &cleaned).await {
                    tracing::warn!(error = %format!("{e:#}"), "title refine: save failed");
                } else {
                    tracing::info!(model = %logical, "title refine: updated session title");
                    if let Some(hub) = &title_hub {
                        hub.publish(&session_id, &cleaned);
                    }
                }
                return;
            }
            Err(e) => {
                tracing::debug!(
                    model = %logical,
                    error = %format!("{e:#}"),
                    "title refine: candidate failed"
                );
            }
        }
    }

    tracing::warn!("title refine: all candidates failed; keeping provisional title");
}

/// User-visible text for a failed Web/API chat turn.
///
/// `anyhow::Error::to_string()` only prints the outermost `.context()`, which
/// hid quota/limit notices behind `"agent turn failed"`.
pub fn user_facing_turn_error(err: &anyhow::Error, model: Option<&str>) -> String {
    for cause in err.chain() {
        let s = cause.to_string();
        if s.contains("VelaClaw notice:") {
            return s;
        }
    }
    let full = format!("{err:#}");
    let sanitized = crate::providers::sanitize_api_error(&full);
    if velaclaw_agent_runtime::looks_like_model_retired(&full) {
        let model = model
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("the selected model");
        return velaclaw_agent_runtime::provider_retired_user_message(
            &sanitized,
            model,
            velaclaw_agent_runtime::SoftFailSurface::Web,
        );
    }
    if velaclaw_agent_runtime::looks_like_provider_limit(&full) {
        let model = model
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("the selected model");
        velaclaw_agent_runtime::provider_limit_user_message(
            &sanitized,
            model,
            velaclaw_agent_runtime::SoftFailSurface::Web,
        )
    } else {
        sanitized
    }
}

/// Split assistant text into stream-sized chunks for WebSocket `delta` frames.
/// Phase 1 emits post-turn chunks; token-level streaming arrives with EVO-001.
pub fn chunk_text_for_stream(text: &str, chunk_size: usize) -> Vec<String> {
    let size = chunk_size.max(1);
    if text.is_empty() {
        return Vec::new();
    }
    text.chars()
        .collect::<Vec<_>>()
        .chunks(size)
        .map(|chunk| chunk.iter().collect())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        key: &'static str,
        old: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: Option<&str>) -> Self {
            let old = std::env::var(key).ok();
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
            Self { key, old }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.old.as_ref() {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[test]
    fn extract_last_user_message_picks_latest_user() {
        let messages = vec![
            ChatMessageInput {
                role: "user".into(),
                content: "first".into(),
            },
            ChatMessageInput {
                role: "assistant".into(),
                content: "ok".into(),
            },
            ChatMessageInput {
                role: "user".into(),
                content: "second".into(),
            },
        ];
        assert_eq!(
            extract_last_user_message(&messages).expect("user"),
            "second"
        );
    }

    #[test]
    fn extract_last_user_message_rejects_empty() {
        let messages = vec![ChatMessageInput {
            role: "assistant".into(),
            content: "only assistant".into(),
        }];
        assert!(extract_last_user_message(&messages).is_err());
    }

    #[test]
    fn chunk_text_splits_unicode() {
        let chunks = chunk_text_for_stream("hello world", 5);
        assert_eq!(chunks, vec!["hello", " worl", "d"]);
    }

    #[test]
    fn apply_chat_overrides_sets_model_and_provider() {
        let mut base = Config::default();
        base.default_model = Some("old/model".into());
        let req = ChatApiRequest {
            messages: vec![],
            session_id: None,
            model_id: Some("deepseek/deepseek-v4-pro".into()),
            temperature: Some(0.2),
            max_tokens: None,
            host_phase: None,
        };
        let updated = apply_chat_overrides(base, &req);
        assert_eq!(
            updated.default_model.as_deref(),
            Some("deepseek/deepseek-v4-pro")
        );
        assert_eq!(updated.default_provider.as_deref(), Some("deepseek"));
        assert!((updated.default_temperature - 0.2).abs() < f64::EPSILON);
    }

    #[test]
    fn live_effective_chat_config_keeps_session_default() {
        let mut base = Config::default();
        base.default_model = Some("deepseek/deepseek-v4-flash".into());
        base.default_provider = Some("deepseek".into());
        base.agent.bounded_dag_live = true;
        let req = ChatApiRequest {
            messages: vec![],
            session_id: None,
            model_id: Some("nvidia/nemotron-3-ultra-550b-a55b".into()),
            temperature: Some(0.2),
            max_tokens: None,
            host_phase: None,
        };
        let updated = effective_chat_config(&base, &req);
        assert_eq!(
            updated.default_model.as_deref(),
            Some("deepseek/deepseek-v4-flash")
        );
        assert_eq!(updated.default_provider.as_deref(), Some("deepseek"));
    }

    #[test]
    fn apply_chat_overrides_ignores_bare_model_label() {
        let mut base = Config::default();
        base.default_provider = Some("nvidia/nemotron-3-super-120b-a12b".into());
        base.default_model = Some("nvidia/nemotron-3-super-120b-a12b".into());
        let req = ChatApiRequest {
            messages: vec![],
            session_id: None,
            model_id: Some("deepseek-chat".into()),
            temperature: None,
            max_tokens: None,
            host_phase: None,
        };
        let updated = apply_chat_overrides(base, &req);
        assert_eq!(
            updated.default_model.as_deref(),
            Some("nvidia/nemotron-3-super-120b-a12b")
        );
        assert_eq!(
            updated.default_provider.as_deref(),
            Some("nvidia/nemotron-3-super-120b-a12b")
        );
    }

    #[test]
    fn apply_chat_overrides_remaps_bare_aggregator_wire_id() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        let providers = dir.path().join("v2").join("providers");
        fs::create_dir_all(&providers).expect("provider dir");
        fs::write(
            providers.join("nvidia.yaml"),
            r#"
id: nvidia
name: NVIDIA
metadata:
  models:
    deepseek-ai/deepseek-v4-flash:
      context_window: 1000000
"#,
        )
        .expect("manifest");
        let _proto = EnvGuard::set(
            "AI_PROTOCOL_DIR",
            Some(dir.path().to_str().expect("utf8 path")),
        );
        let _path = EnvGuard::set("AI_PROTOCOL_PATH", None);

        let mut base = Config::default();
        base.default_provider = Some("deepseek".into());
        base.default_model = Some("deepseek/deepseek-v4-flash".into());
        let req = ChatApiRequest {
            messages: vec![],
            session_id: None,
            model_id: Some("deepseek-ai/deepseek-v4-flash".into()),
            temperature: None,
            max_tokens: None,
            host_phase: None,
        };
        let updated = apply_chat_overrides(base, &req);
        assert_eq!(
            updated.default_model.as_deref(),
            Some("nvidia/deepseek-ai/deepseek-v4-flash")
        );
        assert_eq!(updated.default_provider.as_deref(), Some("nvidia"));
    }

    #[test]
    fn seed_prior_messages_index_excludes_last_user() {
        let messages = [
            ChatMessageInput {
                role: "user".into(),
                content: "hi".into(),
            },
            ChatMessageInput {
                role: "assistant".into(),
                content: "hello".into(),
            },
            ChatMessageInput {
                role: "user".into(),
                content: "run ls".into(),
            },
        ];
        let last_user_idx = messages
            .iter()
            .rposition(|m| m.role == "user" && !m.content.trim().is_empty())
            .expect("user");
        assert_eq!(last_user_idx, 2);
        assert_eq!(messages[..last_user_idx].len(), 2);
    }

    #[test]
    fn host_port_from_urlish_strips_scheme_and_path() {
        assert_eq!(
            host_port_from_urlish("http://127.0.0.1:11434"),
            "127.0.0.1:11434"
        );
        assert_eq!(
            host_port_from_urlish("https://localhost:11434/"),
            "localhost:11434"
        );
        assert_eq!(
            host_port_from_urlish("192.168.1.2:11434"),
            "192.168.1.2:11434"
        );
    }

    #[test]
    fn title_refine_candidates_prefer_nvidia_super_then_mini() {
        let mut cfg = Config::default();
        cfg.default_model = Some("deepseek/deepseek-v4-flash".into());
        cfg.default_provider = Some("deepseek".into());
        let c = title_refine_model_candidates(&cfg);
        assert_eq!(c.first().map(String::as_str), Some(TITLE_NEMOTRON_PRIMARY));
        assert_eq!(c.last().map(String::as_str), Some(TITLE_NEMOTRON_FALLBACK));
    }

    #[test]
    fn title_refine_candidates_prefer_configured_local() {
        let mut cfg = Config::default();
        cfg.default_model = Some("ollama/llama3.2".into());
        cfg.default_provider = Some("ollama".into());
        let c = title_refine_model_candidates(&cfg);
        assert_eq!(c.first().map(String::as_str), Some("ollama/llama3.2"));
        assert!(c.iter().any(|m| m == TITLE_NEMOTRON_PRIMARY));
        assert_eq!(c.last().map(String::as_str), Some(TITLE_NEMOTRON_FALLBACK));
    }

    #[test]
    fn user_facing_turn_error_unwraps_quota_notice_from_context() {
        let err = anyhow::anyhow!(
            "VelaClaw notice: provider limit or quota failure for model `deepseek/deepseek-v4-pro`."
        )
        .context("agent turn failed");
        let msg = user_facing_turn_error(&err, Some("deepseek/deepseek-v4-pro"));
        assert!(msg.contains("VelaClaw notice:"));
        assert!(!msg.starts_with("agent turn failed"));
        assert_eq!(err.to_string(), "agent turn failed");
    }

    #[test]
    fn user_facing_turn_error_maps_raw_402_quota() {
        let err = anyhow::anyhow!(
            "Protocol provider error: Remote error: HTTP 402 (insufficient_quota): Insufficient Balance"
        )
        .context("agent turn failed");
        let msg = user_facing_turn_error(&err, Some("deepseek/deepseek-v4-pro"));
        assert!(msg.contains("VelaClaw notice:"));
        assert!(msg.contains("deepseek/deepseek-v4-pro"));
        assert!(msg.contains("model picker"));
    }
}
