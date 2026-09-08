//! `start_channels` entrypoint (VL-REVIEW-003).

#![allow(clippy::wildcard_imports)]

use super::dispatch::{
    compute_max_in_flight_messages, run_message_dispatch_loop, spawn_supervised_listener,
};
use super::*;

pub async fn start_channels(config: Config) -> Result<()> {
    // Canonical Config → stack (VL-REVIEW2-A0 / GOV-007). Warmup stays transport-only.
    let assembled = crate::agent::assemble::assemble_runtime(
        &config,
        crate::config::BootstrapOptions {
            with_embedding_routes: false,
        },
    )?;
    let provider: Arc<dyn Provider> = Arc::from(assembled.provider);

    // Warm up the provider connection pool (TLS handshake, DNS, HTTP/2 setup)
    // so the first real message doesn't hit a cold-start timeout.
    if let Err(e) = provider.warmup().await {
        tracing::warn!("Provider warmup failed (non-fatal): {e}");
    }

    let initial_stamp = config_file_stamp(&config.config_path).await;
    {
        let mut store = runtime_config_store()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        store.insert(
            config.config_path.clone(),
            RuntimeConfigState {
                defaults: runtime_defaults_from_config(&config),
                last_applied_stamp: initial_stamp,
            },
        );
    }

    let observer = assembled.boot.observer.clone();
    let security = assembled.boot.security.clone();
    let provider_runtime_options = assembled.boot.provider_runtime_options.clone();
    let effective_autonomy = crate::config::resolve_effective_autonomy(&config)?;
    let approval_wiring = Arc::new(crate::config::ApprovalManagerWiring::from_config(&config)?);
    let channel_approval_hub = Arc::new(ChannelApprovalHub::new());
    let telegram_approval = config
        .channels_config
        .telegram
        .as_ref()
        .map(|tg| (tg.approval_mode, tg.approval_timeout_secs));
    let model = assembled.model_name.clone();
    let provider_name = resolved_default_provider(&config);
    let temperature = config.default_temperature;
    let mem = assembled.boot.memory.clone();
    let text_tool_result_history = assembled.text_tool_result_history;
    let tool_dispatcher = assembled.tool_dispatcher;
    // Build system prompt from workspace identity files + skills
    let workspace = config.workspace_dir.clone();
    let tools_registry = Arc::new(assembled.boot.tools);

    let skills = crate::skills::load_skills_with_config(&workspace, &config);

    // Collect tool descriptions for the prompt
    let mut tool_descs: Vec<(&str, &str)> = vec![
        (
            "shell",
            "Execute terminal commands. Use when: running local checks, build/test commands, diagnostics. Don't use when: a safer dedicated tool exists, or command is destructive without approval.",
        ),
        (
            "file_read",
            "Read file contents. Use when: inspecting project files, configs, logs. Don't use when: a targeted search is enough.",
        ),
        (
            "file_write",
            "Write file contents. Use when: applying focused edits, scaffolding files, updating docs/code. Don't use when: side effects are unclear or file ownership is uncertain.",
        ),
        (
            "memory_store",
            "Save to memory. Use when: preserving durable preferences, decisions, key context. Don't use when: information is transient/noisy/sensitive without need.",
        ),
        (
            "memory_recall",
            "Search memory. Use when: retrieving prior decisions, user preferences, historical context. Don't use when: answer is already in current context.",
        ),
        (
            "memory_forget",
            "Delete a memory entry. Use when: memory is incorrect/stale or explicitly requested for removal. Don't use when: impact is uncertain.",
        ),
    ];

    if config.browser.enabled {
        tool_descs.push((
            "browser_open",
            "Open approved HTTPS URLs in Brave Browser (allowlist-only, no scraping)",
        ));
    }
    if config.composio.enabled {
        tool_descs.push((
            "composio",
            "Execute actions on 1000+ apps via Composio (Gmail, Notion, GitHub, Slack, etc.). Use action='list' to discover actions, 'list_accounts' to retrieve connected account IDs, 'execute' to run (optionally with connected_account_id), and 'connect' for OAuth.",
        ));
    }
    tool_descs.push((
        "schedule",
        "Manage scheduled tasks (create/list/get/cancel/pause/resume). Supports recurring cron and one-shot delays.",
    ));
    tool_descs.push((
        "pushover",
        "Send a Pushover notification to your device. Requires PUSHOVER_TOKEN and PUSHOVER_USER_KEY in .env file.",
    ));
    if !config.agents.is_empty() {
        tool_descs.push((
            "delegate",
            "Delegate a subtask to a specialized agent. Use when: a task benefits from a different model (e.g. fast summarization, deep reasoning, code generation). The sub-agent runs a single prompt and returns its response.",
        ));
    }

    let bootstrap_max_chars = if config.agent.compact_context {
        Some(6000)
    } else {
        None
    };
    let prompt_budget = crate::agent::prompt_composer::system_prompt_char_budget(
        config.agent.compact_context,
        &model,
    );
    let native_tools = provider.supports_native_tools();
    let mut system_prompt = crate::channels::build_system_prompt_pyramid(
        &workspace,
        &model,
        &tool_descs,
        &skills,
        Some(&config.identity),
        bootstrap_max_chars,
        native_tools,
        config.skills.prompt_injection_mode,
        crate::agent::prompt_composer::PromptMode::Full,
        prompt_budget,
    );
    #[cfg(feature = "ai-protocol")]
    {
        let strategy = if text_tool_result_history {
            ai_lib_rust::NativeStrategy::Hybrid
        } else if tool_dispatcher.should_send_tool_specs() {
            ai_lib_rust::NativeStrategy::Full
        } else {
            ai_lib_rust::NativeStrategy::TextOnly
        };
        append_text_tool_prompt(
            &mut system_prompt,
            tool_dispatcher.as_ref(),
            tools_registry.as_ref(),
            strategy,
        );
    }
    #[cfg(not(feature = "ai-protocol"))]
    {
        if !native_tools {
            system_prompt.push_str(&build_tool_instructions(tools_registry.as_ref()));
        }
    }
    crate::agent::loop_::append_execution_policy_to_prompt(&mut system_prompt, &security, &config);
    crate::agent::prompt_composer::append_phase_sections(
        &mut system_prompt,
        &[crate::agent::prompt_composer::PromptPhase::Approval],
    );

    if !skills.is_empty() {
        println!(
            "  🧩 Skills:   {}",
            skills
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    // Collect active channels
    let mut channels: Vec<Arc<dyn Channel>> = Vec::new();

    if let Some(ref tg) = config.channels_config.telegram {
        channels.push(Arc::new(
            TelegramChannel::new(
                tg.bot_token.clone(),
                tg.allowed_users.clone(),
                tg.mention_only,
            )
            .with_streaming(tg.stream_mode, tg.draft_update_interval_ms)
            .with_channel_approval_hub(Arc::clone(&channel_approval_hub)),
        ));
    }

    if let Some(ref dc) = config.channels_config.discord {
        channels.push(Arc::new(DiscordChannel::new(
            dc.bot_token.clone(),
            dc.guild_id.clone(),
            dc.allowed_users.clone(),
            dc.listen_to_bots,
            dc.mention_only,
        )));
    }

    if let Some(ref sl) = config.channels_config.slack {
        channels.push(Arc::new(SlackChannel::new(
            sl.bot_token.clone(),
            sl.channel_id.clone(),
            sl.allowed_users.clone(),
        )));
    }

    if let Some(ref mm) = config.channels_config.mattermost {
        channels.push(Arc::new(MattermostChannel::new(
            mm.url.clone(),
            mm.bot_token.clone(),
            mm.channel_id.clone(),
            mm.allowed_users.clone(),
            mm.thread_replies.unwrap_or(true),
            mm.mention_only.unwrap_or(false),
        )));
    }

    if let Some(ref im) = config.channels_config.imessage {
        channels.push(Arc::new(IMessageChannel::new(im.allowed_contacts.clone())));
    }

    #[cfg(feature = "channel-matrix")]
    if let Some(ref mx) = config.channels_config.matrix {
        channels.push(Arc::new(MatrixChannel::new_with_session_hint(
            mx.homeserver.clone(),
            mx.access_token.clone(),
            mx.room_id.clone(),
            mx.allowed_users.clone(),
            mx.user_id.clone(),
            mx.device_id.clone(),
        )));
    }

    #[cfg(not(feature = "channel-matrix"))]
    if config.channels_config.matrix.is_some() {
        tracing::warn!(
            "Matrix channel is configured but this build was compiled without `channel-matrix`; skipping Matrix runtime startup."
        );
    }

    if let Some(ref sig) = config.channels_config.signal {
        channels.push(Arc::new(SignalChannel::new(
            sig.http_url.clone(),
            sig.account.clone(),
            sig.group_id.clone(),
            sig.allowed_from.clone(),
            sig.ignore_attachments,
            sig.ignore_stories,
        )));
    }

    if let Some(ref wa) = config.channels_config.whatsapp {
        if wa.is_ambiguous_config() {
            tracing::warn!(
                "WhatsApp config has both phone_number_id and session_path set; preferring Cloud API mode. Remove one selector to avoid ambiguity."
            );
        }
        // Runtime negotiation: detect backend type from config
        match wa.backend_type() {
            "cloud" => {
                // Cloud API mode: requires phone_number_id, access_token, verify_token
                if wa.is_cloud_config() {
                    channels.push(Arc::new(WhatsAppChannel::new(
                        wa.access_token.clone().unwrap_or_default(),
                        wa.phone_number_id.clone().unwrap_or_default(),
                        wa.verify_token.clone().unwrap_or_default(),
                        wa.allowed_numbers.clone(),
                    )));
                } else {
                    tracing::warn!("WhatsApp Cloud API configured but missing required fields (phone_number_id, access_token, verify_token)");
                }
            }
            "web" => {
                // Web mode: requires session_path
                #[cfg(feature = "whatsapp-web")]
                if wa.is_web_config() {
                    channels.push(Arc::new(WhatsAppWebChannel::new(
                        wa.session_path.clone().unwrap_or_default(),
                        wa.pair_phone.clone(),
                        wa.pair_code.clone(),
                        wa.allowed_numbers.clone(),
                    )));
                } else {
                    tracing::warn!("WhatsApp Web configured but session_path not set");
                }
                #[cfg(not(feature = "whatsapp-web"))]
                {
                    tracing::warn!("WhatsApp Web backend requires 'whatsapp-web' feature. Enable with: cargo build --features whatsapp-web");
                }
            }
            _ => {
                tracing::warn!("WhatsApp config invalid: neither phone_number_id (Cloud API) nor session_path (Web) is set");
            }
        }
    }

    if let Some(ref lq) = config.channels_config.linq {
        channels.push(Arc::new(LinqChannel::new(
            lq.api_token.clone(),
            lq.from_phone.clone(),
            lq.allowed_senders.clone(),
        )));
    }

    if let Some(ref nc) = config.channels_config.nextcloud_talk {
        channels.push(Arc::new(NextcloudTalkChannel::new(
            nc.base_url.clone(),
            nc.app_token.clone(),
            nc.allowed_users.clone(),
        )));
    }

    if let Some(ref email_cfg) = config.channels_config.email {
        channels.push(Arc::new(EmailChannel::new(email_cfg.clone())));
    }

    if let Some(ref irc) = config.channels_config.irc {
        channels.push(Arc::new(IrcChannel::new(irc::IrcChannelConfig {
            server: irc.server.clone(),
            port: irc.port,
            nickname: irc.nickname.clone(),
            username: irc.username.clone(),
            channels: irc.channels.clone(),
            allowed_users: irc.allowed_users.clone(),
            server_password: irc.server_password.clone(),
            nickserv_password: irc.nickserv_password.clone(),
            sasl_password: irc.sasl_password.clone(),
            verify_tls: irc.verify_tls.unwrap_or(true),
        })));
    }

    #[cfg(feature = "channel-lark")]
    if let Some(ref lk) = config.channels_config.lark {
        channels.push(Arc::new(LarkChannel::from_config(lk)));
    }

    #[cfg(not(feature = "channel-lark"))]
    if config.channels_config.lark.is_some() {
        tracing::warn!(
            "Lark channel is configured but this build was compiled without `channel-lark`; skipping Lark runtime startup."
        );
    }

    if let Some(ref dt) = config.channels_config.dingtalk {
        channels.push(Arc::new(DingTalkChannel::new(
            dt.client_id.clone(),
            dt.client_secret.clone(),
            dt.allowed_users.clone(),
        )));
    }

    if let Some(ref qq) = config.channels_config.qq {
        channels.push(Arc::new(QQChannel::new(
            qq.app_id.clone(),
            qq.app_secret.clone(),
            qq.allowed_users.clone(),
        )));
    }

    if channels.is_empty() {
        println!("No channels configured. Run `velaclaw onboard` to set up channels.");
        return Ok(());
    }

    println!("🦀 VelaClaw Channel Server");
    println!("  🤖 Model:    {model}");
    let effective_backend = memory::effective_memory_backend_name(
        &config.memory.backend,
        Some(&config.storage.provider.config),
    );
    println!(
        "  🧠 Memory:   {} (auto-save: {})",
        effective_backend,
        if config.memory.auto_save { "on" } else { "off" }
    );
    println!(
        "  📡 Channels: {}",
        channels
            .iter()
            .map(|c| c.name())
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!();
    println!("  Listening for messages... (Ctrl+C to stop)");
    println!();

    crate::health::mark_component_ok("channels");

    let initial_backoff_secs = config
        .reliability
        .channel_initial_backoff_secs
        .max(DEFAULT_CHANNEL_INITIAL_BACKOFF_SECS);
    let max_backoff_secs = config
        .reliability
        .channel_max_backoff_secs
        .max(DEFAULT_CHANNEL_MAX_BACKOFF_SECS);

    // Single message bus — all channels send messages here
    let (tx, rx) = tokio::sync::mpsc::channel::<traits::ChannelMessage>(100);

    // Spawn a listener for each channel
    let mut handles = Vec::new();
    for ch in &channels {
        handles.push(spawn_supervised_listener(
            ch.clone(),
            tx.clone(),
            initial_backoff_secs,
            max_backoff_secs,
        ));
    }
    drop(tx); // Drop our copy so rx closes when all channels stop

    let channels_by_name = Arc::new(
        channels
            .iter()
            .map(|ch| (ch.name().to_string(), Arc::clone(ch)))
            .collect::<HashMap<_, _>>(),
    );
    let max_in_flight_messages = compute_max_in_flight_messages(channels.len());

    println!("  🚦 In-flight message limit: {max_in_flight_messages}");

    let mut provider_cache_seed: HashMap<String, Arc<dyn Provider>> = HashMap::new();
    provider_cache_seed.insert(provider_name.clone(), Arc::clone(&provider));
    let message_timeout_secs =
        effective_channel_message_timeout_secs(config.channels_config.message_timeout_secs);
    let interrupt_on_new_message = config
        .channels_config
        .telegram
        .as_ref()
        .is_some_and(|tg| tg.interrupt_on_new_message);

    #[cfg(feature = "ai-protocol")]
    let workspace_tool_dispatcher = {
        let policy = crate::config::discover_and_load(&config)
            .with_context(|| "load workspace agent-policy.yaml")?;
        Arc::new(policy.and_then(|p| p.tool_dispatcher().map(str::to_string)))
    };

    let runtime_ctx = Arc::new(ChannelRuntimeContext {
        channels_by_name,
        provider: Arc::clone(&provider),
        default_provider: Arc::new(provider_name),
        memory: Arc::clone(&mem),
        tools_registry: Arc::clone(&tools_registry),
        observer,
        system_prompt: Arc::new(system_prompt),
        model: Arc::new(model.clone()),
        peer_logical_ids: Arc::new(crate::agent::loop_::logical_ids_from_config(&config)),
        model_routes: Arc::new(config.model_routes.clone()),
        temperature,
        auto_save_memory: config.memory.auto_save,
        max_tool_iterations: config.agent.max_tool_iterations,
        min_relevance_score: config.memory.min_relevance_score,
        conversation_histories: Arc::new(Mutex::new(HashMap::new())),
        provider_cache: Arc::new(Mutex::new(provider_cache_seed)),
        route_overrides: Arc::new(Mutex::new(HashMap::new())),
        api_key: config.api_key.clone(),
        api_url: config.api_url.clone(),
        reliability: Arc::new(config.reliability.clone()),
        provider_runtime_options,
        workspace_dir: Arc::new(config.workspace_dir.clone()),
        message_timeout_secs,
        interrupt_on_new_message,
        multimodal: config.multimodal.clone(),
        #[cfg(feature = "ai-protocol")]
        tool_dispatcher_choice: Arc::new(config.agent.tool_dispatcher.clone()),
        #[cfg(feature = "ai-protocol")]
        workspace_tool_dispatcher,
        #[cfg(feature = "ai-protocol")]
        tool_dispatcher_cache: Arc::new(Mutex::new(HashMap::new())),
        #[cfg(feature = "ai-protocol")]
        envelope_pilot: EnvelopePilotConfig {
            enabled: config.agent.envelope_assemble,
            use_async_pool: config.agent.envelope_assemble_async,
            compact_context: config.agent.compact_context,
        },
        security: security.clone(),
        channel_approval_hub,
        approval_managers: Arc::new(Mutex::new(HashMap::new())),
        autonomy_config: Arc::new(effective_autonomy),
        approval_wiring,
        telegram_approval,
    });

    run_message_dispatch_loop(rx, runtime_ctx, max_in_flight_messages).await;

    // Wait for all channel tasks
    for h in handles {
        let _ = h.await;
    }

    Ok(())
}
