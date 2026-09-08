//! Shared channel runtime state and helpers (VL-REVIEW-003).
//! Child module of `channels` — uses parent imports via `super::*`.

#![allow(clippy::wildcard_imports)]

use super::*;

pub(crate) type ConversationHistoryMap = Arc<Mutex<HashMap<String, Vec<ChatMessage>>>>;
/// Maximum history messages to keep per sender.
pub(crate) const MAX_CHANNEL_HISTORY: usize = 50;
/// Minimum user-message length (in chars) for auto-save to memory.
/// Messages shorter than this (e.g. "ok", "thanks") are not stored,
/// reducing noise in memory recall.
pub(crate) const AUTOSAVE_MIN_MESSAGE_CHARS: usize = 20;

/// Maximum characters per injected workspace file (matches `OpenClaw` default).
pub(crate) const BOOTSTRAP_MAX_CHARS: usize = 20_000;

pub(crate) const DEFAULT_CHANNEL_INITIAL_BACKOFF_SECS: u64 = 2;
pub(crate) const DEFAULT_CHANNEL_MAX_BACKOFF_SECS: u64 = 60;
pub(crate) const MIN_CHANNEL_MESSAGE_TIMEOUT_SECS: u64 = 30;
/// Default timeout for processing a single channel message (LLM + tools).
/// Used as fallback when not configured in channels_config.message_timeout_secs.
pub(crate) const CHANNEL_MESSAGE_TIMEOUT_SECS: u64 = 300;
/// Cap timeout scaling so large max_tool_iterations values do not create unbounded waits.
pub(crate) const CHANNEL_MESSAGE_TIMEOUT_SCALE_CAP: u64 = 4;
pub(crate) const CHANNEL_PARALLELISM_PER_CHANNEL: usize = 4;
pub(crate) const CHANNEL_MIN_IN_FLIGHT_MESSAGES: usize = 8;
pub(crate) const CHANNEL_MAX_IN_FLIGHT_MESSAGES: usize = 64;
pub(crate) const CHANNEL_TYPING_REFRESH_INTERVAL_SECS: u64 = 4;
pub(crate) const CHANNEL_HEALTH_HEARTBEAT_SECS: u64 = 30;
pub(crate) const MODEL_CACHE_FILE: &str = "models_cache.json";
pub(crate) const MODEL_CACHE_PREVIEW_LIMIT: usize = 10;
pub(crate) const MEMORY_CONTEXT_MAX_ENTRIES: usize = 4;
pub(crate) const MEMORY_CONTEXT_ENTRY_MAX_CHARS: usize = 800;
pub(crate) const MEMORY_CONTEXT_MAX_CHARS: usize = 4_000;
pub(crate) const CHANNEL_HISTORY_COMPACT_KEEP_MESSAGES: usize = 12;
pub(crate) const CHANNEL_HISTORY_COMPACT_CONTENT_CHARS: usize = 600;

pub(crate) type ProviderCacheMap = Arc<Mutex<HashMap<String, Arc<dyn Provider>>>>;
#[cfg(feature = "ai-protocol")]
#[derive(Clone)]
pub(crate) struct CachedToolDispatch {
    pub(crate) dispatcher: Arc<dyn ToolDispatcher>,
    pub(crate) text_tool_result_history: bool,
}

#[cfg(feature = "ai-protocol")]
pub(crate) type ToolDispatcherCacheMap = Arc<Mutex<HashMap<String, CachedToolDispatch>>>;
pub(crate) type RouteSelectionMap = Arc<Mutex<HashMap<String, ChannelRouteSelection>>>;

pub(crate) fn effective_channel_message_timeout_secs(configured: u64) -> u64 {
    configured.max(MIN_CHANNEL_MESSAGE_TIMEOUT_SECS)
}

pub(crate) fn channel_message_timeout_budget_secs(
    message_timeout_secs: u64,
    max_tool_iterations: usize,
) -> u64 {
    let iterations = max_tool_iterations.max(1) as u64;
    let scale = iterations.min(CHANNEL_MESSAGE_TIMEOUT_SCALE_CAP);
    message_timeout_secs.saturating_mul(scale)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChannelRouteSelection {
    pub(crate) provider: String,
    pub(crate) model: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ChannelRuntimeCommand {
    ShowProviders,
    SetProvider(String),
    ShowModel,
    SetModel(String),
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct ModelCacheState {
    pub(crate) entries: Vec<ModelCacheEntry>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct ModelCacheEntry {
    pub(crate) provider: String,
    pub(crate) models: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ChannelRuntimeDefaults {
    pub(crate) default_provider: String,
    pub(crate) model: String,
    pub(crate) temperature: f64,
    pub(crate) api_key: Option<String>,
    pub(crate) api_url: Option<String>,
    pub(crate) reliability: crate::config::ReliabilityConfig,
    /// Hot-applied from `[agent].max_tool_iterations` (0 normalizes to 10).
    pub(crate) max_tool_iterations: usize,
}

fn normalize_max_tool_iterations(value: usize) -> usize {
    if value == 0 {
        10
    } else {
        value
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ConfigFileStamp {
    pub(crate) modified: SystemTime,
    pub(crate) len: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct RuntimeConfigState {
    pub(crate) defaults: ChannelRuntimeDefaults,
    pub(crate) last_applied_stamp: Option<ConfigFileStamp>,
}

pub(crate) fn runtime_config_store() -> &'static Mutex<HashMap<PathBuf, RuntimeConfigState>> {
    static STORE: OnceLock<Mutex<HashMap<PathBuf, RuntimeConfigState>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) const SYSTEMD_STATUS_ARGS: [&str; 3] = ["--user", "is-active", "velaclaw.service"];
pub(crate) const SYSTEMD_RESTART_ARGS: [&str; 3] = ["--user", "restart", "velaclaw.service"];
pub(crate) const OPENRC_STATUS_ARGS: [&str; 2] = ["velaclaw", "status"];
pub(crate) const OPENRC_RESTART_ARGS: [&str; 2] = ["velaclaw", "restart"];

#[derive(Clone)]
pub(crate) struct ChannelRuntimeContext {
    pub(crate) channels_by_name: Arc<HashMap<String, Arc<dyn Channel>>>,
    pub(crate) provider: Arc<dyn Provider>,
    pub(crate) default_provider: Arc<String>,
    pub(crate) memory: Arc<dyn Memory>,
    pub(crate) tools_registry: Arc<Vec<Box<dyn Tool>>>,
    pub(crate) observer: Arc<dyn Observer>,
    pub(crate) system_prompt: Arc<String>,
    pub(crate) model: Arc<String>,
    pub(crate) peer_logical_ids: Arc<Vec<String>>,
    pub(crate) model_routes: Arc<Vec<crate::config::ModelRouteConfig>>,
    pub(crate) temperature: f64,
    pub(crate) auto_save_memory: bool,
    pub(crate) max_tool_iterations: usize,
    pub(crate) min_relevance_score: f64,
    pub(crate) conversation_histories: ConversationHistoryMap,
    pub(crate) provider_cache: ProviderCacheMap,
    pub(crate) route_overrides: RouteSelectionMap,
    pub(crate) api_key: Option<String>,
    pub(crate) api_url: Option<String>,
    pub(crate) reliability: Arc<crate::config::ReliabilityConfig>,
    pub(crate) provider_runtime_options: providers::ProviderRuntimeOptions,
    pub(crate) workspace_dir: Arc<PathBuf>,
    pub(crate) message_timeout_secs: u64,
    pub(crate) interrupt_on_new_message: bool,
    pub(crate) multimodal: crate::config::MultimodalConfig,
    #[cfg(feature = "ai-protocol")]
    pub(crate) tool_dispatcher_choice: Arc<String>,
    #[cfg(feature = "ai-protocol")]
    pub(crate) workspace_tool_dispatcher: Arc<Option<String>>,
    #[cfg(feature = "ai-protocol")]
    pub(crate) tool_dispatcher_cache: ToolDispatcherCacheMap,
    /// CR-L2-005: opt-in Envelope assemble on channel turns.
    #[cfg(feature = "ai-protocol")]
    pub(crate) envelope_pilot: EnvelopePilotConfig,
    pub(crate) security: PolicyHandle,
    pub(crate) channel_approval_hub: Arc<ChannelApprovalHub>,
    pub(crate) approval_managers: Arc<Mutex<HashMap<String, Arc<ApprovalManager>>>>,
    pub(crate) autonomy_config: Arc<crate::config::AutonomyConfig>,
    pub(crate) approval_wiring: Arc<crate::config::ApprovalManagerWiring>,
    pub(crate) telegram_approval: Option<(ChannelApprovalMode, u64)>,
}

/// Bundled CR-L1/L2/L3 Envelope pilot knobs for channel dispatch (keeps bool count low).
#[cfg(feature = "ai-protocol")]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct EnvelopePilotConfig {
    pub(crate) enabled: bool,
    /// CR-L3-003: use `AssemblePool` when `enabled` (default false → sync assemble).
    pub(crate) use_async_pool: bool,
    pub(crate) compact_context: bool,
}

#[derive(Clone)]
pub(crate) struct InFlightSenderTaskState {
    pub(crate) task_id: u64,
    pub(crate) cancellation: CancellationToken,
    pub(crate) completion: Arc<InFlightTaskCompletion>,
}

pub(crate) struct InFlightTaskCompletion {
    pub(crate) done: AtomicBool,
    pub(crate) notify: tokio::sync::Notify,
}

impl InFlightTaskCompletion {
    pub(crate) fn new() -> Self {
        Self {
            done: AtomicBool::new(false),
            notify: tokio::sync::Notify::new(),
        }
    }

    pub(crate) fn mark_done(&self) {
        self.done.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    pub(crate) async fn wait(&self) {
        if self.done.load(Ordering::Acquire) {
            return;
        }
        self.notify.notified().await;
    }
}

pub(crate) fn conversation_memory_key(msg: &traits::ChannelMessage) -> String {
    format!("{}_{}_{}", msg.channel, msg.sender, msg.id)
}

pub(crate) fn conversation_history_key(msg: &traits::ChannelMessage) -> String {
    format!("{}_{}", msg.channel, msg.sender)
}

pub(crate) fn get_or_create_approval_manager(
    ctx: &ChannelRuntimeContext,
    sender_key: &str,
) -> Arc<ApprovalManager> {
    let mut managers = ctx
        .approval_managers
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    managers
        .entry(sender_key.to_string())
        .or_insert_with(|| Arc::new(ctx.approval_wiring.spawn_manager(&ctx.autonomy_config)))
        .clone()
}

pub(crate) fn interruption_scope_key(msg: &traits::ChannelMessage) -> String {
    format!("{}_{}_{}", msg.channel, msg.reply_target, msg.sender)
}

pub(crate) fn channel_delivery_instructions(channel_name: &str) -> Option<&'static str> {
    match channel_name {
        "telegram" => Some(
            "When responding on Telegram, include media markers for files or URLs that should be sent as attachments. Use one marker per attachment with this exact syntax: [IMAGE:<path-or-url>], [DOCUMENT:<path-or-url>], [VIDEO:<path-or-url>], [AUDIO:<path-or-url>], or [VOICE:<path-or-url>]. Keep normal user-facing text outside markers and never wrap markers in code fences.",
        ),
        _ => None,
    }
}

pub(crate) fn build_channel_system_prompt(base_prompt: &str, channel_name: &str) -> String {
    if let Some(instructions) = channel_delivery_instructions(channel_name) {
        if base_prompt.is_empty() {
            instructions.to_string()
        } else {
            format!("{base_prompt}\n\n{instructions}")
        }
    } else {
        base_prompt.to_string()
    }
}

pub(crate) fn normalize_cached_channel_turns(turns: Vec<ChatMessage>) -> Vec<ChatMessage> {
    let mut normalized = Vec::with_capacity(turns.len());
    let mut expecting_user = true;

    for turn in turns {
        match (expecting_user, turn.role.as_str()) {
            (true, "user") => {
                normalized.push(turn);
                expecting_user = false;
            }
            (false, "assistant") => {
                normalized.push(turn);
                expecting_user = true;
            }
            // Interrupted channel turns can produce consecutive user messages
            // (no assistant persisted yet). Merge instead of dropping.
            (false, "user") | (true, "assistant") => {
                if let Some(last_turn) = normalized.last_mut() {
                    if !turn.content.is_empty() {
                        if !last_turn.content.is_empty() {
                            last_turn.content.push_str("\n\n");
                        }
                        last_turn.content.push_str(&turn.content);
                    }
                }
            }
            _ => {}
        }
    }

    normalized
}

pub(crate) fn supports_runtime_model_switch(channel_name: &str) -> bool {
    matches!(channel_name, "telegram" | "discord" | "cli")
}

pub(crate) fn parse_runtime_slash_command(content: &str) -> Option<ChannelRuntimeCommand> {
    let trimmed = content.trim();
    if !trimmed.starts_with('/') {
        return None;
    }

    let mut parts = trimmed.split_whitespace();
    let command_token = parts.next()?;
    let base_command = command_token
        .split('@')
        .next()
        .unwrap_or(command_token)
        .to_ascii_lowercase();

    match base_command.as_str() {
        "/models" => {
            if let Some(provider) = parts.next() {
                Some(ChannelRuntimeCommand::SetProvider(
                    provider.trim().to_string(),
                ))
            } else {
                Some(ChannelRuntimeCommand::ShowProviders)
            }
        }
        "/model" => {
            let model = parts.collect::<Vec<_>>().join(" ").trim().to_string();
            if model.is_empty() {
                Some(ChannelRuntimeCommand::ShowModel)
            } else {
                Some(ChannelRuntimeCommand::SetModel(model))
            }
        }
        _ => None,
    }
}

pub(crate) fn parse_runtime_command(
    channel_name: &str,
    content: &str,
) -> Option<ChannelRuntimeCommand> {
    if !supports_runtime_model_switch(channel_name) {
        return None;
    }
    parse_runtime_slash_command(content)
}

/// Handle `/models` and `/model` in standalone CLI agent mode (no channel context).
pub(crate) fn handle_cli_runtime_slash_command(
    input: &str,
    config: &crate::Config,
    current_provider: &str,
    current_model: &str,
) -> Option<(String, Option<String>)> {
    let command = parse_runtime_slash_command(input)?;
    let current = ChannelRouteSelection {
        provider: current_provider.to_string(),
        model: current_model.to_string(),
    };

    let response = match command {
        ChannelRuntimeCommand::ShowProviders => build_providers_help_response(&current),
        ChannelRuntimeCommand::SetProvider(raw_provider) => {
            match resolve_provider_alias(&raw_provider) {
                Some(provider_name) => format!(
                    "To switch provider to `{provider_name}`, restart with:\n  \
                     velaclaw agent --provider {provider_name}\n\n\
                     Current session: provider `{current_provider}`, model `{current_model}`."
                ),
                None => format!(
                    "Unknown provider `{raw_provider}`. Use `/models` to list valid providers."
                ),
            }
        }
        ChannelRuntimeCommand::ShowModel => {
            build_models_help_response(&current, &config.workspace_dir)
        }
        ChannelRuntimeCommand::SetModel(model) => {
            return Some((
                format!("Model switched to `{model}` for this CLI session."),
                Some(model),
            ));
        }
    };

    Some((response, None))
}

pub(crate) fn resolve_provider_alias(name: &str) -> Option<String> {
    let candidate = name.trim();
    if candidate.is_empty() {
        return None;
    }

    let providers_list = providers::list_providers();
    for provider in providers_list {
        if provider.name.eq_ignore_ascii_case(candidate)
            || provider
                .aliases
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(candidate))
        {
            return Some(provider.name.to_string());
        }
    }

    None
}

pub(crate) fn resolved_default_provider(config: &Config) -> String {
    config
        .default_provider
        .clone()
        .unwrap_or_else(|| DEFAULT_PROTOCOL_MODEL_ID.to_string())
}

pub(crate) fn resolved_default_model(config: &Config) -> String {
    config
        .default_model
        .clone()
        .unwrap_or_else(|| "anthropic/claude-sonnet-4.6".to_string())
}

pub(crate) fn runtime_defaults_from_config(config: &Config) -> ChannelRuntimeDefaults {
    ChannelRuntimeDefaults {
        default_provider: resolved_default_provider(config),
        model: resolved_default_model(config),
        temperature: config.default_temperature,
        api_key: config.api_key.clone(),
        api_url: config.api_url.clone(),
        reliability: config.reliability.clone(),
        max_tool_iterations: normalize_max_tool_iterations(config.agent.max_tool_iterations),
    }
}

pub(crate) fn runtime_config_path(ctx: &ChannelRuntimeContext) -> Option<PathBuf> {
    ctx.provider_runtime_options
        .velaclaw_dir
        .as_ref()
        .map(|dir| dir.join("config.toml"))
}

pub(crate) fn runtime_defaults_snapshot(ctx: &ChannelRuntimeContext) -> ChannelRuntimeDefaults {
    if let Some(config_path) = runtime_config_path(ctx) {
        let store = runtime_config_store()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(state) = store.get(&config_path) {
            return state.defaults.clone();
        }
    }

    ChannelRuntimeDefaults {
        default_provider: ctx.default_provider.as_str().to_string(),
        model: ctx.model.as_str().to_string(),
        temperature: ctx.temperature,
        api_key: ctx.api_key.clone(),
        api_url: ctx.api_url.clone(),
        reliability: (*ctx.reliability).clone(),
        max_tool_iterations: normalize_max_tool_iterations(ctx.max_tool_iterations),
    }
}

pub(crate) async fn config_file_stamp(path: &Path) -> Option<ConfigFileStamp> {
    let metadata = tokio::fs::metadata(path).await.ok()?;
    let modified = metadata.modified().ok()?;
    Some(ConfigFileStamp {
        modified,
        len: metadata.len(),
    })
}

pub(crate) fn decrypt_optional_secret_for_runtime_reload(
    store: &crate::security::SecretStore,
    value: &mut Option<String>,
    field_name: &str,
) -> Result<()> {
    if let Some(raw) = value.clone() {
        if crate::security::SecretStore::is_encrypted(&raw) {
            *value = Some(
                store
                    .decrypt(&raw)
                    .with_context(|| format!("Failed to decrypt {field_name}"))?,
            );
        }
    }
    Ok(())
}

pub(crate) async fn load_runtime_defaults_from_config_file(
    path: &Path,
) -> Result<ChannelRuntimeDefaults> {
    let contents = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("Failed to read {}", path.display()))?;
    let mut parsed: Config =
        toml::from_str(&contents).with_context(|| format!("Failed to parse {}", path.display()))?;
    parsed.config_path = path.to_path_buf();

    if let Some(velaclaw_dir) = path.parent() {
        let store = crate::security::SecretStore::new(velaclaw_dir, parsed.secrets.encrypt);
        decrypt_optional_secret_for_runtime_reload(&store, &mut parsed.api_key, "config.api_key")?;
    }

    parsed.apply_env_overrides();
    Ok(runtime_defaults_from_config(&parsed))
}

pub(crate) async fn maybe_apply_runtime_config_update(ctx: &ChannelRuntimeContext) -> Result<()> {
    let Some(config_path) = runtime_config_path(ctx) else {
        return Ok(());
    };

    let Some(stamp) = config_file_stamp(&config_path).await else {
        return Ok(());
    };

    {
        let store = runtime_config_store()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(state) = store.get(&config_path) {
            if state.last_applied_stamp == Some(stamp) {
                return Ok(());
            }
        }
    }

    let next_defaults = load_runtime_defaults_from_config_file(&config_path).await?;
    let next_default_provider = providers::create_resilient_provider_with_options(
        &next_defaults.default_provider,
        next_defaults.api_key.as_deref(),
        next_defaults.api_url.as_deref(),
        &next_defaults.reliability,
        &ctx.provider_runtime_options,
        None,
    )?;
    let next_default_provider: Arc<dyn Provider> = Arc::from(next_default_provider);

    if let Err(err) = next_default_provider.warmup().await {
        tracing::warn!(
            provider = %next_defaults.default_provider,
            "Provider warmup failed after config reload: {err}"
        );
    }

    {
        let mut cache = ctx.provider_cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.clear();
        cache.insert(
            next_defaults.default_provider.clone(),
            Arc::clone(&next_default_provider),
        );
    }

    {
        let mut store = runtime_config_store()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        store.insert(
            config_path.clone(),
            RuntimeConfigState {
                defaults: next_defaults.clone(),
                last_applied_stamp: Some(stamp),
            },
        );
    }

    tracing::info!(
        path = %config_path.display(),
        provider = %next_defaults.default_provider,
        model = %next_defaults.model,
        temperature = next_defaults.temperature,
        max_tool_iterations = next_defaults.max_tool_iterations,
        "Applied updated channel runtime config from disk"
    );

    Ok(())
}

pub(crate) fn default_route_selection(ctx: &ChannelRuntimeContext) -> ChannelRouteSelection {
    let defaults = runtime_defaults_snapshot(ctx);
    ChannelRouteSelection {
        provider: defaults.default_provider,
        model: defaults.model,
    }
}

pub(crate) fn get_route_selection(
    ctx: &ChannelRuntimeContext,
    sender_key: &str,
) -> ChannelRouteSelection {
    ctx.route_overrides
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(sender_key)
        .cloned()
        .unwrap_or_else(|| default_route_selection(ctx))
}

pub(crate) fn set_route_selection(
    ctx: &ChannelRuntimeContext,
    sender_key: &str,
    next: ChannelRouteSelection,
) {
    let default_route = default_route_selection(ctx);
    let mut routes = ctx
        .route_overrides
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if next == default_route {
        routes.remove(sender_key);
    } else {
        routes.insert(sender_key.to_string(), next);
    }
}

pub(crate) fn clear_sender_history(ctx: &ChannelRuntimeContext, sender_key: &str) {
    ctx.conversation_histories
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(sender_key);
}

pub(crate) fn compact_sender_history(ctx: &ChannelRuntimeContext, sender_key: &str) -> bool {
    let mut histories = ctx
        .conversation_histories
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    let Some(turns) = histories.get_mut(sender_key) else {
        return false;
    };

    if turns.is_empty() {
        return false;
    }

    let keep_from = turns
        .len()
        .saturating_sub(CHANNEL_HISTORY_COMPACT_KEEP_MESSAGES);
    let mut compacted = normalize_cached_channel_turns(turns[keep_from..].to_vec());

    for turn in &mut compacted {
        if turn.content.chars().count() > CHANNEL_HISTORY_COMPACT_CONTENT_CHARS {
            turn.content =
                truncate_with_ellipsis(&turn.content, CHANNEL_HISTORY_COMPACT_CONTENT_CHARS);
        }
    }

    if compacted.is_empty() {
        turns.clear();
        return false;
    }

    *turns = compacted;
    true
}

pub(crate) fn append_sender_turn(ctx: &ChannelRuntimeContext, sender_key: &str, turn: ChatMessage) {
    let mut histories = ctx
        .conversation_histories
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let turns = histories.entry(sender_key.to_string()).or_default();
    turns.push(turn);
    while turns.len() > MAX_CHANNEL_HISTORY {
        turns.remove(0);
    }
}

pub(crate) fn should_skip_memory_context_entry(key: &str, content: &str) -> bool {
    if memory::is_assistant_autosave_key(key) {
        return true;
    }

    if key.trim().to_ascii_lowercase().ends_with("_history") {
        return true;
    }

    content.chars().count() > MEMORY_CONTEXT_MAX_CHARS
}

pub(crate) fn is_context_window_overflow_error(err: &anyhow::Error) -> bool {
    let lower = err.to_string().to_lowercase();
    [
        "exceeds the context window",
        "context window of this model",
        "maximum context length",
        "context length exceeded",
        "too many tokens",
        "token limit exceeded",
        "prompt is too long",
        "input is too long",
    ]
    .iter()
    .any(|hint| lower.contains(hint))
}

pub(crate) fn load_cached_model_preview(workspace_dir: &Path, provider_name: &str) -> Vec<String> {
    let cache_path = workspace_dir.join("state").join(MODEL_CACHE_FILE);
    let Ok(raw) = std::fs::read_to_string(cache_path) else {
        return Vec::new();
    };
    let Ok(state) = serde_json::from_str::<ModelCacheState>(&raw) else {
        return Vec::new();
    };

    state
        .entries
        .into_iter()
        .find(|entry| entry.provider == provider_name)
        .map(|entry| {
            entry
                .models
                .into_iter()
                .take(MODEL_CACHE_PREVIEW_LIMIT)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

pub(crate) async fn get_or_create_provider(
    ctx: &ChannelRuntimeContext,
    provider_name: &str,
) -> anyhow::Result<Arc<dyn Provider>> {
    if let Some(existing) = ctx
        .provider_cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(provider_name)
        .cloned()
    {
        return Ok(existing);
    }

    if provider_name == ctx.default_provider.as_str() {
        return Ok(Arc::clone(&ctx.provider));
    }

    let defaults = runtime_defaults_snapshot(ctx);
    let api_url = if provider_name == defaults.default_provider.as_str() {
        defaults.api_url.as_deref()
    } else {
        None
    };

    let provider = create_resilient_provider_nonblocking(
        provider_name,
        ctx.api_key.clone(),
        api_url.map(ToString::to_string),
        ctx.reliability.as_ref().clone(),
        ctx.provider_runtime_options.clone(),
    )
    .await?;
    let provider: Arc<dyn Provider> = Arc::from(provider);

    if let Err(err) = provider.warmup().await {
        tracing::warn!(provider = provider_name, "Provider warmup failed: {err}");
    }

    let mut cache = ctx.provider_cache.lock().unwrap_or_else(|e| e.into_inner());
    let cached = cache
        .entry(provider_name.to_string())
        .or_insert_with(|| Arc::clone(&provider));
    Ok(Arc::clone(cached))
}

#[cfg(feature = "ai-protocol")]
pub(crate) async fn get_or_create_tool_dispatcher(
    ctx: &ChannelRuntimeContext,
    logical_model_id: &str,
    provider: &dyn Provider,
) -> anyhow::Result<(Arc<dyn ToolDispatcher>, bool)> {
    if let Some(existing) = ctx
        .tool_dispatcher_cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(logical_model_id)
        .cloned()
    {
        return Ok((
            Arc::clone(&existing.dispatcher),
            existing.text_tool_result_history,
        ));
    }

    let config_choice = ctx.tool_dispatcher_choice.as_str().to_string();
    let model = logical_model_id.to_string();
    let policy = tokio::task::spawn_blocking(move || {
        let client = crate::execution::init_ai_client_sync(&model)?;
        Ok::<_, anyhow::Error>(ai_lib_rust::ToolCallingPolicy::from_tool_calling(
            client.manifest.tool_calling(),
        ))
    })
    .await
    .context("tool dispatcher init task failed")??;

    let text_tool_result_history = policy.native_strategy == ai_lib_rust::NativeStrategy::Hybrid;
    let effective = crate::config::EffectivePolicy::resolve(
        config_choice.as_str(),
        ctx.workspace_tool_dispatcher.as_ref().as_deref(),
        None,
        policy,
    );
    let dispatcher = Arc::from(effective.build_dispatcher(provider));
    ctx.tool_dispatcher_cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            logical_model_id.to_string(),
            CachedToolDispatch {
                dispatcher: Arc::clone(&dispatcher),
                text_tool_result_history,
            },
        );
    Ok((dispatcher, text_tool_result_history))
}

pub(crate) async fn create_resilient_provider_nonblocking(
    provider_name: &str,
    api_key: Option<String>,
    api_url: Option<String>,
    reliability: crate::config::ReliabilityConfig,
    provider_runtime_options: providers::ProviderRuntimeOptions,
) -> anyhow::Result<Box<dyn Provider>> {
    let provider_name = provider_name.to_string();
    tokio::task::spawn_blocking(move || {
        providers::create_resilient_provider_with_options(
            &provider_name,
            api_key.as_deref(),
            api_url.as_deref(),
            &reliability,
            &provider_runtime_options,
            None,
        )
    })
    .await
    .context("failed to join provider initialization task")?
}

pub(crate) fn build_models_help_response(
    current: &ChannelRouteSelection,
    workspace_dir: &Path,
) -> String {
    let mut response = String::new();
    let _ = writeln!(
        response,
        "Current provider: `{}`\nCurrent model: `{}`",
        current.provider, current.model
    );
    response.push_str("\nSwitch model with `/model <model-id>`.\n");

    let cached_models = load_cached_model_preview(workspace_dir, &current.provider);
    if cached_models.is_empty() {
        let _ = writeln!(
            response,
            "\nNo cached model list found for `{}`. Ask the operator to run `velaclaw models refresh --provider {}`.",
            current.provider, current.provider
        );
    } else {
        let _ = writeln!(
            response,
            "\nCached model IDs (top {}):",
            cached_models.len()
        );
        for model in cached_models {
            let _ = writeln!(response, "- `{model}`");
        }
    }

    response
}

pub(crate) fn build_providers_help_response(current: &ChannelRouteSelection) -> String {
    let mut response = String::new();
    let _ = writeln!(
        response,
        "Current provider: `{}`\nCurrent model: `{}`",
        current.provider, current.model
    );
    response.push_str("\nSwitch provider with `/models <provider>`.\n");
    response.push_str("Switch model with `/model <model-id>`.\n\n");
    response.push_str("Available providers:\n");
    for provider in providers::list_providers() {
        if provider.aliases.is_empty() {
            let _ = writeln!(response, "- {}", provider.name);
        } else {
            let _ = writeln!(
                response,
                "- {} (aliases: {})",
                provider.name,
                provider.aliases.join(", ")
            );
        }
    }
    response
}

pub(crate) async fn handle_runtime_command_if_needed(
    ctx: &ChannelRuntimeContext,
    msg: &traits::ChannelMessage,
    target_channel: Option<&Arc<dyn Channel>>,
) -> bool {
    let Some(command) = parse_runtime_command(&msg.channel, &msg.content) else {
        return false;
    };

    let Some(channel) = target_channel else {
        return true;
    };

    let sender_key = conversation_history_key(msg);
    let mut current = get_route_selection(ctx, &sender_key);

    let response = match command {
        ChannelRuntimeCommand::ShowProviders => build_providers_help_response(&current),
        ChannelRuntimeCommand::SetProvider(raw_provider) => {
            match resolve_provider_alias(&raw_provider) {
                Some(provider_name) => match get_or_create_provider(ctx, &provider_name).await {
                    Ok(_) => {
                        if provider_name != current.provider {
                            current.provider = provider_name.clone();
                            set_route_selection(ctx, &sender_key, current.clone());
                            clear_sender_history(ctx, &sender_key);
                        }

                        format!(
                            "Provider switched to `{provider_name}` for this sender session. Current model is `{}`.\nUse `/model <model-id>` to set a provider-compatible model.",
                            current.model
                        )
                    }
                    Err(err) => {
                        let safe_err = providers::sanitize_api_error(&err.to_string());
                        format!(
                            "Failed to initialize provider `{provider_name}`. Route unchanged.\nDetails: {safe_err}"
                        )
                    }
                },
                None => format!(
                    "Unknown provider `{raw_provider}`. Use `/models` to list valid providers."
                ),
            }
        }
        ChannelRuntimeCommand::ShowModel => {
            build_models_help_response(&current, ctx.workspace_dir.as_path())
        }
        ChannelRuntimeCommand::SetModel(raw_model) => {
            let model = raw_model.trim().trim_matches('`').to_string();
            if model.is_empty() {
                "Model ID cannot be empty. Use `/model <model-id>`.".to_string()
            } else {
                current.model = model.clone();
                set_route_selection(ctx, &sender_key, current.clone());
                clear_sender_history(ctx, &sender_key);

                format!(
                    "Model switched to `{model}` for provider `{}` in this sender session.",
                    current.provider
                )
            }
        }
    };

    if let Err(err) = channel
        .send(&SendMessage::new(response, &msg.reply_target).in_thread(msg.thread_ts.clone()))
        .await
    {
        tracing::warn!(
            "Failed to send runtime command response on {}: {err}",
            channel.name()
        );
    }

    true
}

pub(crate) async fn build_memory_context(
    mem: &dyn Memory,
    user_msg: &str,
    min_relevance_score: f64,
) -> String {
    let mut context = String::new();

    if let Ok(entries) = mem.recall(user_msg, 5, None).await {
        let mut included = 0usize;
        let mut used_chars = 0usize;

        for entry in entries.iter().filter(|e| match e.score {
            Some(score) => score >= min_relevance_score,
            None => true, // keep entries without a score (e.g. non-vector backends)
        }) {
            if included >= MEMORY_CONTEXT_MAX_ENTRIES {
                break;
            }

            if should_skip_memory_context_entry(&entry.key, &entry.content) {
                continue;
            }

            let content = if entry.content.chars().count() > MEMORY_CONTEXT_ENTRY_MAX_CHARS {
                truncate_with_ellipsis(&entry.content, MEMORY_CONTEXT_ENTRY_MAX_CHARS)
            } else {
                entry.content.clone()
            };

            let line = format!("- {}: {}\n", entry.key, content);
            let line_chars = line.chars().count();
            if used_chars + line_chars > MEMORY_CONTEXT_MAX_CHARS {
                break;
            }

            if included == 0 {
                context.push_str("[Memory context]\n");
            }

            context.push_str(&line);
            used_chars += line_chars;
            included += 1;
        }

        if included > 0 {
            context.push('\n');
        }
    }

    context
}

/// Extract a compact summary of tool interactions from history messages added
/// during `run_tool_call_loop`. Scans assistant messages for `<tool_call>` tags
/// or native tool-call JSON to collect tool names used.
/// Returns an empty string when no tools were invoked.
pub(crate) fn extract_tool_context_summary(history: &[ChatMessage], start_index: usize) -> String {
    fn push_unique_tool_name(tool_names: &mut Vec<String>, name: &str) {
        let candidate = name.trim();
        if candidate.is_empty() {
            return;
        }
        if !tool_names.iter().any(|existing| existing == candidate) {
            tool_names.push(candidate.to_string());
        }
    }

    fn collect_tool_names_from_tool_call_tags(content: &str, tool_names: &mut Vec<String>) {
        const TAG_PAIRS: [(&str, &str); 4] = [
            ("<tool_call>", "</tool_call>"),
            ("<toolcall>", "</toolcall>"),
            ("<tool-call>", "</tool-call>"),
            ("<invoke>", "</invoke>"),
        ];

        for (open_tag, close_tag) in TAG_PAIRS {
            for segment in content.split(open_tag) {
                if let Some(json_end) = segment.find(close_tag) {
                    let json_str = segment[..json_end].trim();
                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str) {
                        if let Some(name) = val.get("name").and_then(|n| n.as_str()) {
                            push_unique_tool_name(tool_names, name);
                        }
                    }
                }
            }
        }
    }

    fn collect_tool_names_from_native_json(content: &str, tool_names: &mut Vec<String>) {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(content) {
            if let Some(calls) = val.get("tool_calls").and_then(|c| c.as_array()) {
                for call in calls {
                    let name = call
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|n| n.as_str())
                        .or_else(|| call.get("name").and_then(|n| n.as_str()));
                    if let Some(name) = name {
                        push_unique_tool_name(tool_names, name);
                    }
                }
            }
        }
    }

    fn collect_tool_names_from_tool_results(content: &str, tool_names: &mut Vec<String>) {
        let marker = "<tool_result name=\"";
        let mut remaining = content;
        while let Some(start) = remaining.find(marker) {
            let name_start = start + marker.len();
            let after_name_start = &remaining[name_start..];
            if let Some(name_end) = after_name_start.find('"') {
                let name = &after_name_start[..name_end];
                push_unique_tool_name(tool_names, name);
                remaining = &after_name_start[name_end + 1..];
            } else {
                break;
            }
        }
    }

    let mut tool_names: Vec<String> = Vec::new();

    for msg in history.iter().skip(start_index) {
        match msg.role.as_str() {
            "assistant" => {
                collect_tool_names_from_tool_call_tags(&msg.content, &mut tool_names);
                collect_tool_names_from_native_json(&msg.content, &mut tool_names);
            }
            "user" => {
                // Prompt-mode tool calls are always followed by [Tool results] entries
                // containing `<tool_result name="...">` tags with canonical tool names.
                collect_tool_names_from_tool_results(&msg.content, &mut tool_names);
            }
            _ => {}
        }
    }

    if tool_names.is_empty() {
        return String::new();
    }

    format!("[Used tools: {}]", tool_names.join(", "))
}

pub(crate) fn sanitize_channel_response(response: &str, tools: &[Box<dyn Tool>]) -> String {
    let known_tool_names: HashSet<String> = tools
        .iter()
        .map(|tool| tool.name().to_ascii_lowercase())
        .collect();
    velaclaw_agent_runtime::strip_isolated_tool_json_artifacts(response, &known_tool_names)
}

// Channel tests + sanitize use the shared VL-TTC-015 helpers.
#[cfg(test)]
pub(crate) use velaclaw_agent_runtime::strip_isolated_tool_json_artifacts;
