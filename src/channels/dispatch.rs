//! Message processing / dispatch loop (VL-REVIEW-003).
//! Child of `channels` so it can access parent private fields.

#![allow(clippy::wildcard_imports)]

use super::*;

pub(crate) fn spawn_supervised_listener(
    ch: Arc<dyn Channel>,
    tx: tokio::sync::mpsc::Sender<traits::ChannelMessage>,
    initial_backoff_secs: u64,
    max_backoff_secs: u64,
) -> tokio::task::JoinHandle<()> {
    spawn_supervised_listener_with_health_interval(
        ch,
        tx,
        initial_backoff_secs,
        max_backoff_secs,
        Duration::from_secs(CHANNEL_HEALTH_HEARTBEAT_SECS),
    )
}

pub(crate) fn spawn_supervised_listener_with_health_interval(
    ch: Arc<dyn Channel>,
    tx: tokio::sync::mpsc::Sender<traits::ChannelMessage>,
    initial_backoff_secs: u64,
    max_backoff_secs: u64,
    health_interval: Duration,
) -> tokio::task::JoinHandle<()> {
    let health_interval = if health_interval.is_zero() {
        Duration::from_secs(1)
    } else {
        health_interval
    };

    tokio::spawn(async move {
        let component = format!("channel:{}", ch.name());
        let mut backoff = initial_backoff_secs.max(1);
        let max_backoff = max_backoff_secs.max(backoff);

        loop {
            crate::health::mark_component_ok(&component);
            let mut health = tokio::time::interval(health_interval);
            health.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let result = {
                let listen_future = ch.listen(tx.clone());
                tokio::pin!(listen_future);

                loop {
                    tokio::select! {
                        _ = health.tick() => {
                            crate::health::mark_component_ok(&component);
                        }
                        result = &mut listen_future => break result,
                    }
                }
            };

            if tx.is_closed() {
                break;
            }

            match result {
                Ok(()) => {
                    tracing::warn!("Channel {} exited unexpectedly; restarting", ch.name());
                    crate::health::mark_component_error(&component, "listener exited unexpectedly");
                    // Clean exit — reset backoff since the listener ran successfully
                    backoff = initial_backoff_secs.max(1);
                }
                Err(e) => {
                    tracing::error!("Channel {} error: {e}; restarting", ch.name());
                    crate::health::mark_component_error(&component, e.to_string());
                }
            }

            crate::health::bump_component_restart(&component);
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            // Double backoff AFTER sleeping so first error uses initial_backoff
            backoff = backoff.saturating_mul(2).min(max_backoff);
        }
    })
}

pub(crate) fn compute_max_in_flight_messages(channel_count: usize) -> usize {
    channel_count
        .saturating_mul(CHANNEL_PARALLELISM_PER_CHANNEL)
        .clamp(
            CHANNEL_MIN_IN_FLIGHT_MESSAGES,
            CHANNEL_MAX_IN_FLIGHT_MESSAGES,
        )
}

pub(crate) fn log_worker_join_result(result: Result<(), tokio::task::JoinError>) {
    if let Err(error) = result {
        tracing::error!("Channel message worker crashed: {error}");
    }
}

pub(crate) fn spawn_scoped_typing_task(
    channel: Arc<dyn Channel>,
    recipient: String,
    cancellation_token: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let stop_signal = cancellation_token;
    let refresh_interval = Duration::from_secs(CHANNEL_TYPING_REFRESH_INTERVAL_SECS);
    let handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(refresh_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                () = stop_signal.cancelled() => break,
                _ = interval.tick() => {
                    if let Err(e) = channel.start_typing(&recipient).await {
                        tracing::debug!("Failed to start typing on {}: {e}", channel.name());
                    }
                }
            }
        }

        if let Err(e) = channel.stop_typing(&recipient).await {
            tracing::debug!("Failed to stop typing on {}: {e}", channel.name());
        }
    });

    handle
}

pub(crate) async fn process_channel_message(
    ctx: Arc<ChannelRuntimeContext>,
    msg: traits::ChannelMessage,
    cancellation_token: CancellationToken,
) {
    if cancellation_token.is_cancelled() {
        return;
    }

    if ctx
        .channel_approval_hub
        .try_resolve_text(&msg.channel, &msg.reply_target, &msg.content)
    {
        tracing::info!(
            channel = %msg.channel,
            sender = %msg.sender,
            "Resolved inline tool approval from channel reply"
        );
        return;
    }

    println!(
        "  💬 [{}] from {}: {}",
        msg.channel,
        msg.sender,
        truncate_with_ellipsis(&msg.content, 80)
    );

    let target_channel = ctx.channels_by_name.get(&msg.channel).cloned();
    if let Err(err) = maybe_apply_runtime_config_update(ctx.as_ref()).await {
        tracing::warn!("Failed to apply runtime config update: {err}");
    }
    if handle_runtime_command_if_needed(ctx.as_ref(), &msg, target_channel.as_ref()).await {
        return;
    }

    let history_key = conversation_history_key(&msg);
    let route = get_route_selection(ctx.as_ref(), &history_key);
    let runtime_defaults = runtime_defaults_snapshot(ctx.as_ref());
    let active_provider = match get_or_create_provider(ctx.as_ref(), &route.provider).await {
        Ok(provider) => provider,
        Err(err) => {
            let safe_err = providers::sanitize_api_error(&err.to_string());
            let message = format!(
                "⚠️ Failed to initialize provider `{}`. Please run `/models` to choose another provider.\nDetails: {safe_err}",
                route.provider
            );
            if let Some(channel) = target_channel.as_ref() {
                let _ = channel
                    .send(
                        &SendMessage::new(message, &msg.reply_target)
                            .in_thread(msg.thread_ts.clone()),
                    )
                    .await;
            }
            return;
        }
    };

    if ctx.auto_save_memory && msg.content.chars().count() >= AUTOSAVE_MIN_MESSAGE_CHARS {
        let autosave_key = conversation_memory_key(&msg);
        let _ = ctx
            .memory
            .store(
                &autosave_key,
                &msg.content,
                crate::memory::MemoryCategory::Conversation,
                None,
            )
            .await;
    }

    println!("  ⏳ Processing message...");
    let started_at = Instant::now();

    let had_prior_history = ctx
        .conversation_histories
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&history_key)
        .is_some_and(|turns| !turns.is_empty());

    // Preserve user turn before the LLM call so interrupted requests keep context.
    append_sender_turn(ctx.as_ref(), &history_key, ChatMessage::user(&msg.content));

    // Build history from per-sender conversation cache.
    let prior_turns_raw = ctx
        .conversation_histories
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&history_key)
        .cloned()
        .unwrap_or_default();
    let mut prior_turns = normalize_cached_channel_turns(prior_turns_raw);

    // Only enrich with memory context when there is no prior conversation
    // history. Follow-up turns already include context from previous messages.
    if !had_prior_history {
        let memory_context =
            build_memory_context(ctx.memory.as_ref(), &msg.content, ctx.min_relevance_score).await;
        if let Some(last_turn) = prior_turns.last_mut() {
            if last_turn.role == "user" && !memory_context.is_empty() {
                last_turn.content = format!("{memory_context}{}", msg.content);
            }
        }
    }

    let system_prompt = build_channel_system_prompt(ctx.system_prompt.as_str(), &msg.channel);
    let mut history = vec![ChatMessage::system(system_prompt)];
    history.extend(prior_turns);

    let summarizer = crate::agent::context_orch::HistorySummarizer {
        provider: active_provider.as_ref(),
        model: route.model.as_str(),
    };
    if let Err(err) = crate::agent::context_orch::prepare_turn_history(
        &mut history,
        crate::agent::context_orch::PrepareHistoryOpts {
            layered: ctx.envelope_pilot.enabled,
            compact_context: ctx.envelope_pilot.compact_context,
            async_pool: ctx.envelope_pilot.use_async_pool,
            max_history: super::runtime::MAX_CHANNEL_HISTORY,
            extra_chunks: &[],
            context_window: None,
            summarizer: Some(&summarizer),
        },
    )
    .await
    {
        tracing::warn!(
            channel = %msg.channel,
            sender = %msg.sender,
            "Context prepare failed (fail-closed): {err}"
        );
        // Undo the user turn we appended before assemble so the next message
        // does not see an orphan user turn without an assistant reply.
        if let Some(turns) = ctx
            .conversation_histories
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&history_key)
        {
            let _ = turns.pop();
        }
        if let Some(channel) = target_channel.as_ref() {
            let reply = format!(
                "⚠️ Context envelope assembly failed. Message not processed.\nDetails: {err}"
            );
            if let Err(send_err) = channel
                .send(&SendMessage::new(&reply, &msg.reply_target).in_thread(msg.thread_ts.clone()))
                .await
            {
                eprintln!("  ❌ Failed to reply on {}: {send_err}", channel.name());
            }
        }
        return;
    }

    let use_streaming = target_channel
        .as_ref()
        .is_some_and(|ch| ch.supports_draft_updates());

    let (delta_tx, delta_rx) = if use_streaming {
        let (tx, rx) = tokio::sync::mpsc::channel::<String>(64);
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    let draft_message_id = if use_streaming {
        if let Some(channel) = target_channel.as_ref() {
            match channel
                .send_draft(
                    &SendMessage::new("...", &msg.reply_target).in_thread(msg.thread_ts.clone()),
                )
                .await
            {
                Ok(id) => id,
                Err(e) => {
                    tracing::debug!("Failed to send draft on {}: {e}", channel.name());
                    None
                }
            }
        } else {
            None
        }
    } else {
        None
    };

    let draft_updater = if let (Some(mut rx), Some(draft_id_ref), Some(channel_ref)) = (
        delta_rx,
        draft_message_id.as_deref(),
        target_channel.as_ref(),
    ) {
        let channel = Arc::clone(channel_ref);
        let reply_target = msg.reply_target.clone();
        let draft_id = draft_id_ref.to_string();
        Some(tokio::spawn(async move {
            let mut accumulated = String::new();
            while let Some(delta) = rx.recv().await {
                accumulated.push_str(&delta);
                if let Err(e) = channel
                    .update_draft(&reply_target, &draft_id, &accumulated)
                    .await
                {
                    tracing::debug!("Draft update failed: {e}");
                }
            }
        }))
    } else {
        None
    };

    let typing_cancellation = target_channel.as_ref().map(|_| CancellationToken::new());
    let typing_task = match (target_channel.as_ref(), typing_cancellation.as_ref()) {
        (Some(channel), Some(token)) => Some(spawn_scoped_typing_task(
            Arc::clone(channel),
            msg.reply_target.clone(),
            token.clone(),
        )),
        _ => None,
    };

    // Record history length before tool loop so we can extract tool context after.
    let history_len_before_tools = history.len();

    if cancellation_token.is_cancelled() {
        return;
    }

    #[cfg(feature = "ai-protocol")]
    let (tool_dispatcher, text_tool_result_history) = if ctx.tools_registry.is_empty() {
        (None, false)
    } else {
        tokio::select! {
            () = cancellation_token.cancelled() => return,
            result = get_or_create_tool_dispatcher(
                ctx.as_ref(),
                &route.provider,
                active_provider.as_ref(),
            ) => match result {
                Ok((dispatcher, hybrid_history)) => (Some(dispatcher), hybrid_history),
                Err(err) => {
                    tracing::warn!(
                        provider = route.provider.as_str(),
                        "Failed to build manifest tool dispatcher: {err}"
                    );
                    (None, false)
                }
            },
        }
    };

    #[cfg(not(feature = "ai-protocol"))]
    let (tool_dispatcher, text_tool_result_history) = (None, false);

    if cancellation_token.is_cancelled() {
        return;
    }

    let approval_manager = get_or_create_approval_manager(ctx.as_ref(), &history_key);
    let channel_approval = if msg.channel == "telegram" {
        ctx.telegram_approval.and_then(|(mode, timeout_secs)| {
            ctx.channels_by_name
                .get(&msg.channel)
                .map(|channel| ChannelApprovalSession {
                    hub: Arc::clone(&ctx.channel_approval_hub),
                    channel: Arc::clone(channel),
                    reply_target: msg.reply_target.clone(),
                    sender: msg.sender.clone(),
                    mode,
                    timeout: Duration::from_secs(timeout_secs),
                })
        })
    } else {
        None
    };

    enum LlmExecutionResult {
        Completed(Result<Result<String, anyhow::Error>, tokio::time::error::Elapsed>),
        Cancelled,
    }

    let max_tool_iterations = runtime_defaults.max_tool_iterations;
    let timeout_budget_secs =
        channel_message_timeout_budget_secs(ctx.message_timeout_secs, max_tool_iterations);
    let llm_result = tokio::select! {
        () = cancellation_token.cancelled() => LlmExecutionResult::Cancelled,
        result = tokio::time::timeout(
            Duration::from_secs(timeout_budget_secs),
            run_tool_call_loop(
                active_provider.as_ref(),
                &mut history,
                ctx.tools_registry.as_ref(),
                ctx.observer.as_ref(),
                route.provider.as_str(),
                route.model.as_str(),
                runtime_defaults.temperature,
                true,
                Some(approval_manager.as_ref()),
                msg.channel.as_str(),
                &ctx.multimodal,
                max_tool_iterations,
                Some(cancellation_token.clone()),
                delta_tx,
                {
                    #[cfg(feature = "ai-protocol")]
                    {
                        tool_dispatcher.as_deref()
                    }
                    #[cfg(not(feature = "ai-protocol"))]
                    {
                        None
                    }
                },
                Some(&ctx.security),
                channel_approval,
                text_tool_result_history,
                RenderOpts {
                    style: RenderStyle {
                        ansi: false,
                        markdown: true,
                    },
                    fold_lines: 10,
                    fold_enabled: false,
                },
                None,
                Some(SoftFailLoopCtx {
                    session_key: history_key.as_str(),
                    config: None,
                    #[cfg(feature = "ai-protocol")]
                    host_decide: None,
                    surface: velaclaw_agent_runtime::SoftFailSurface::Channel,
                    peer_logical_ids: ctx.peer_logical_ids.as_slice(),
                    model_routes: ctx.model_routes.as_slice(),
                    session_model: Some(ctx.model.as_str()),
                    probe: None,
                }),
            None,
            ),
        ) => LlmExecutionResult::Completed(result),
    };

    if let Some(handle) = draft_updater {
        let _ = handle.await;
    }

    if let Some(token) = typing_cancellation.as_ref() {
        token.cancel();
    }
    if let Some(handle) = typing_task {
        log_worker_join_result(handle.await);
    }

    match llm_result {
        LlmExecutionResult::Cancelled => {
            tracing::info!(
                channel = %msg.channel,
                sender = %msg.sender,
                "Cancelled in-flight channel request due to newer message"
            );
            if let (Some(channel), Some(draft_id)) =
                (target_channel.as_ref(), draft_message_id.as_deref())
            {
                if let Err(err) = channel.cancel_draft(&msg.reply_target, draft_id).await {
                    tracing::debug!("Failed to cancel draft on {}: {err}", channel.name());
                }
            }
        }
        LlmExecutionResult::Completed(Ok(Ok(response))) => {
            if cancellation_token.is_cancelled() {
                tracing::info!(
                    channel = %msg.channel,
                    sender = %msg.sender,
                    "Discarding completed channel reply due to newer message interrupt"
                );
                if let (Some(channel), Some(draft_id)) =
                    (target_channel.as_ref(), draft_message_id.as_deref())
                {
                    if let Err(err) = channel.cancel_draft(&msg.reply_target, draft_id).await {
                        tracing::debug!("Failed to cancel draft on {}: {err}", channel.name());
                    }
                }
                return;
            }
            let sanitized_response =
                sanitize_channel_response(&response, ctx.tools_registry.as_ref());
            let delivered_response = if sanitized_response.is_empty() && !response.trim().is_empty()
            {
                "I encountered malformed tool-call output and could not produce a safe reply. Please try again.".to_string()
            } else {
                sanitized_response
            };

            // Extract condensed tool-use context from the history messages
            // added during run_tool_call_loop, so the LLM retains awareness
            // of what it did on subsequent turns.
            let tool_summary = extract_tool_context_summary(&history, history_len_before_tools);
            let history_response = if tool_summary.is_empty() {
                delivered_response.clone()
            } else {
                format!("{tool_summary}\n{delivered_response}")
            };

            append_sender_turn(
                ctx.as_ref(),
                &history_key,
                ChatMessage::assistant(&history_response),
            );
            println!(
                "  🤖 Reply ({}ms): {}",
                started_at.elapsed().as_millis(),
                truncate_with_ellipsis(&delivered_response, 80)
            );
            if let Some(channel) = target_channel.as_ref() {
                if let Some(ref draft_id) = draft_message_id {
                    if let Err(e) = channel
                        .finalize_draft(&msg.reply_target, draft_id, &delivered_response)
                        .await
                    {
                        tracing::warn!("Failed to finalize draft: {e}; sending as new message");
                        let _ = channel
                            .send(
                                &SendMessage::new(&delivered_response, &msg.reply_target)
                                    .in_thread(msg.thread_ts.clone()),
                            )
                            .await;
                    }
                } else if let Err(e) = channel
                    .send(
                        &SendMessage::new(delivered_response, &msg.reply_target)
                            .in_thread(msg.thread_ts.clone()),
                    )
                    .await
                {
                    eprintln!("  ❌ Failed to reply on {}: {e}", channel.name());
                }
            }
        }
        LlmExecutionResult::Completed(Ok(Err(e))) => {
            if crate::agent::loop_::is_tool_loop_cancelled(&e) || cancellation_token.is_cancelled()
            {
                tracing::info!(
                    channel = %msg.channel,
                    sender = %msg.sender,
                    "Cancelled in-flight channel request due to newer message"
                );
                if let (Some(channel), Some(draft_id)) =
                    (target_channel.as_ref(), draft_message_id.as_deref())
                {
                    if let Err(err) = channel.cancel_draft(&msg.reply_target, draft_id).await {
                        tracing::debug!("Failed to cancel draft on {}: {err}", channel.name());
                    }
                }
                return;
            }

            if is_context_window_overflow_error(&e) {
                let compacted = compact_sender_history(ctx.as_ref(), &history_key);
                let error_text = if compacted {
                    "⚠️ Context window exceeded for this conversation. I compacted recent history and kept the latest context. Please resend your last message."
                } else {
                    "⚠️ Context window exceeded for this conversation. Please resend your last message."
                };
                eprintln!(
                    "  ⚠️ Context window exceeded after {}ms; sender history compacted={}",
                    started_at.elapsed().as_millis(),
                    compacted
                );
                if let Some(channel) = target_channel.as_ref() {
                    if let Some(ref draft_id) = draft_message_id {
                        let _ = channel
                            .finalize_draft(&msg.reply_target, draft_id, error_text)
                            .await;
                    } else {
                        let _ = channel
                            .send(
                                &SendMessage::new(error_text, &msg.reply_target)
                                    .in_thread(msg.thread_ts.clone()),
                            )
                            .await;
                    }
                }
                return;
            }

            eprintln!(
                "  ❌ LLM error after {}ms: {e}",
                started_at.elapsed().as_millis()
            );
            #[cfg(feature = "ai-protocol")]
            let e = crate::orchestration::map_provider_limit_error(
                e,
                route.model.as_str(),
                velaclaw_agent_runtime::SoftFailSurface::Channel,
                None,
                history_key.as_str(),
            );
            if let Some(channel) = target_channel.as_ref() {
                if let Some(ref draft_id) = draft_message_id {
                    let _ = channel
                        .finalize_draft(&msg.reply_target, draft_id, &format!("⚠️ Error: {e}"))
                        .await;
                } else {
                    let _ = channel
                        .send(
                            &SendMessage::new(format!("⚠️ Error: {e}"), &msg.reply_target)
                                .in_thread(msg.thread_ts.clone()),
                        )
                        .await;
                }
            }
        }
        LlmExecutionResult::Completed(Err(_)) => {
            let timeout_msg = format!(
                "LLM response timed out after {}s (base={}s, max_tool_iterations={})",
                timeout_budget_secs, ctx.message_timeout_secs, max_tool_iterations
            );
            eprintln!(
                "  ❌ {} (elapsed: {}ms)",
                timeout_msg,
                started_at.elapsed().as_millis()
            );
            if let Some(channel) = target_channel.as_ref() {
                let error_text =
                    "⚠️ Request timed out while waiting for the model. Please try again.";
                if let Some(ref draft_id) = draft_message_id {
                    let _ = channel
                        .finalize_draft(&msg.reply_target, draft_id, error_text)
                        .await;
                } else {
                    let _ = channel
                        .send(
                            &SendMessage::new(error_text, &msg.reply_target)
                                .in_thread(msg.thread_ts.clone()),
                        )
                        .await;
                }
            }
        }
    }
}

pub(crate) async fn run_message_dispatch_loop(
    mut rx: tokio::sync::mpsc::Receiver<traits::ChannelMessage>,
    ctx: Arc<ChannelRuntimeContext>,
    max_in_flight_messages: usize,
) {
    let semaphore = Arc::new(tokio::sync::Semaphore::new(max_in_flight_messages));
    let mut workers = tokio::task::JoinSet::new();
    let in_flight_by_sender = Arc::new(tokio::sync::Mutex::new(HashMap::<
        String,
        InFlightSenderTaskState,
    >::new()));
    let task_sequence = Arc::new(AtomicU64::new(1));

    while let Some(msg) = rx.recv().await {
        let permit = match Arc::clone(&semaphore).acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => break,
        };

        let worker_ctx = Arc::clone(&ctx);
        let in_flight = Arc::clone(&in_flight_by_sender);
        let task_sequence = Arc::clone(&task_sequence);
        workers.spawn(async move {
            let _permit = permit;
            let interrupt_enabled =
                worker_ctx.interrupt_on_new_message && msg.channel == "telegram";
            let sender_scope_key = interruption_scope_key(&msg);
            let cancellation_token = CancellationToken::new();
            let completion = Arc::new(InFlightTaskCompletion::new());
            let task_id = task_sequence.fetch_add(1, Ordering::Relaxed);

            if interrupt_enabled {
                let previous = {
                    let mut active = in_flight.lock().await;
                    active.insert(
                        sender_scope_key.clone(),
                        InFlightSenderTaskState {
                            task_id,
                            cancellation: cancellation_token.clone(),
                            completion: Arc::clone(&completion),
                        },
                    )
                };

                if let Some(previous) = previous {
                    tracing::info!(
                        channel = %msg.channel,
                        sender = %msg.sender,
                        "Interrupting previous in-flight request for sender"
                    );
                    previous.cancellation.cancel();
                    previous.completion.wait().await;
                }
            }

            process_channel_message(worker_ctx, msg, cancellation_token).await;

            if interrupt_enabled {
                let mut active = in_flight.lock().await;
                if active
                    .get(&sender_scope_key)
                    .is_some_and(|state| state.task_id == task_id)
                {
                    active.remove(&sender_scope_key);
                }
            }

            completion.mark_done();
        });

        while let Some(result) = workers.try_join_next() {
            log_worker_join_result(result);
        }
    }

    while let Some(result) = workers.join_next().await {
        log_worker_join_result(result);
    }
}
