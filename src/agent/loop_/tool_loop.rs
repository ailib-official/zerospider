//! Shared tool-call iteration body (VL-REVIEW2-A1 / VL-CTX-002).

#[allow(clippy::wildcard_imports)]
use super::*;

/// Execute a single turn of the agent loop: send messages, parse tool calls,
/// execute tools, and loop until the LLM produces a final text response.
/// When `silent` is true, suppresses stdout (for channel use).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn agent_turn(
    provider: &dyn Provider,
    history: &mut Vec<ChatMessage>,
    tools_registry: &[Box<dyn Tool>],
    observer: &dyn Observer,
    provider_name: &str,
    model: &str,
    temperature: f64,
    silent: bool,
    multimodal_config: &crate::config::MultimodalConfig,
    max_tool_iterations: usize,
) -> Result<String> {
    run_tool_call_loop(
        provider,
        history,
        tools_registry,
        observer,
        provider_name,
        model,
        temperature,
        silent,
        None,
        "channel",
        multimodal_config,
        max_tool_iterations,
        None,
        None,
        None,
        None,
        None,
        false,
        RenderOpts {
            style: RenderStyle {
                ansi: false,
                markdown: true,
            },
            fold_lines: 10,
            fold_enabled: false,
        },
        None,
        None,
        None,
    )
    .await
}

// ── Agent Tool-Call Loop ──────────────────────────────────────────────────
// Core agentic iteration: send conversation to the LLM, parse any tool
// calls from the response, execute them, append results to history, and
// repeat until the LLM produces a final text-only answer.
//
// Loop invariant: at the start of each iteration, `history` contains the
// full conversation so far (system prompt + user messages + prior tool
// results). The loop exits when:
//   • the LLM returns no tool calls (final answer), or
//   • max_iterations is reached (runaway safety), or
//   • the cancellation token fires (external abort).

/// Append manifest-backed text tool instructions when the model may emit markup
/// instead of (or alongside) native API tool calls.
#[cfg(feature = "ai-protocol")]
pub(crate) fn append_text_tool_prompt(
    system_prompt: &mut String,
    dispatcher: &dyn crate::agent::dispatcher::ToolDispatcher,
    tools_registry: &[Box<dyn Tool>],
    native_strategy: ai_lib_rust::NativeStrategy,
) {
    let append = !dispatcher.should_send_tool_specs()
        || native_strategy == ai_lib_rust::NativeStrategy::Hybrid;
    if append {
        let instr = dispatcher.prompt_instructions(tools_registry);
        if !instr.is_empty() {
            system_prompt.push_str(&instr);
        }
    }
}

/// Soft-fail UX context for tool loop (ORCH-HOST-004/005).
///
/// `config` (CLI) or `host_decide` (Web) enable opt-in `host_decide_failover`.
/// Channel surfaces pass neither — notices still apply; Decide failover does not.
#[derive(Clone, Copy)]
pub(crate) struct SoftFailLoopCtx<'a> {
    pub session_key: &'a str,
    pub config: Option<&'a Config>,
    /// Pre-built host Decide context (Web `Agent` path — no full [`Config`] retained).
    #[cfg(feature = "ai-protocol")]
    pub host_decide: Option<&'a crate::orchestration::HostDecideHost>,
    pub surface: velaclaw_agent_runtime::SoftFailSurface,
    /// Logical model ids from `[[model_routes]]` (capability catalog, not cost order).
    pub peer_logical_ids: &'a [String],
    /// Shared per-node probe governor (DAG hops). None → local to this loop call.
    pub probe: Option<&'a std::sync::Mutex<crate::agent::probe_dedup::HopProbeGovernor>>,
}

#[cfg(feature = "ai-protocol")]
impl SoftFailLoopCtx<'_> {
    fn host_decide_owned(&self) -> Option<crate::orchestration::HostDecideHost> {
        self.config
            .map(crate::orchestration::HostDecideHost::from_config)
    }
}

fn with_probe<R>(
    soft_fail: Option<SoftFailLoopCtx<'_>>,
    local: &mut crate::agent::probe_dedup::HopProbeGovernor,
    f: impl FnOnce(&mut crate::agent::probe_dedup::HopProbeGovernor) -> R,
) -> R {
    if let Some(cell) = soft_fail.and_then(|c| c.probe) {
        let mut g = cell.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut g)
    } else {
        f(local)
    }
}

/// Execute a single turn of the agent loop: send messages, parse tool calls,
/// execute tools, and loop until the LLM produces a final text response.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_tool_call_loop(
    provider: &dyn Provider,
    history: &mut Vec<ChatMessage>,
    tools_registry: &[Box<dyn Tool>],
    observer: &dyn Observer,
    provider_name: &str,
    model: &str,
    temperature: f64,
    silent: bool,
    approval: Option<&ApprovalManager>,
    channel_name: &str,
    multimodal_config: &crate::config::MultimodalConfig,
    max_tool_iterations: usize,
    cancellation_token: Option<CancellationToken>,
    on_delta: Option<tokio::sync::mpsc::Sender<String>>,
    tool_dispatcher: Option<&dyn crate::agent::dispatcher::ToolDispatcher>,
    security: Option<&PolicyHandle>,
    channel_approval: Option<ChannelApprovalSession>,
    // When true, tool results use `[Tool results]` user text (Hybrid manifests).
    text_tool_result_history: bool,
    render_opts: RenderOpts,
    _fold_cache: Option<&FoldCache>,
    soft_fail: Option<SoftFailLoopCtx<'_>>,
    gate_extras: Option<&crate::agent::tool_batch::ToolBatchGateExtras>,
) -> Result<String> {
    let max_iterations = if max_tool_iterations == 0 {
        DEFAULT_MAX_TOOL_ITERATIONS
    } else {
        max_tool_iterations
    };

    let mut active_model = model.to_string();
    let mut peer_continue_used = false;
    let mut local_probe = Box::new(crate::agent::probe_dedup::HopProbeGovernor::new());

    let tool_specs: Vec<crate::tools::ToolSpec> =
        tools_registry.iter().map(|tool| tool.spec()).collect();
    let use_native_tools = tool_dispatcher
        .map(|d| d.should_send_tool_specs() && !tool_specs.is_empty())
        .unwrap_or_else(|| provider.supports_native_tools() && !tool_specs.is_empty());

    for _iteration in 0..max_iterations {
        if cancellation_token
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            return Err(ToolLoopCancelled.into());
        }

        let image_marker_count = multimodal::count_image_markers(history);
        if image_marker_count > 0 && !provider.supports_vision() {
            return Err(ProviderCapabilityError {
                provider: provider_name.to_string(),
                capability: "vision".to_string(),
                message: format!(
                    "received {image_marker_count} image marker(s), but this provider does not support vision input"
                ),
            }
            .into());
        }

        let prepared_messages =
            multimodal::prepare_messages_for_provider(history, multimodal_config).await?;

        observer.record_event(&ObserverEvent::LlmRequest {
            provider: provider_name.to_string(),
            model: active_model.clone(),
            messages_count: history.len(),
        });

        let llm_started_at = Instant::now();

        // Unified path via Provider::chat so provider-specific native tool logic
        // (OpenAI/Anthropic/OpenRouter/compatible adapters) is honored.
        let request_tools = if use_native_tools {
            Some(tool_specs.as_slice())
        } else {
            None
        };

        let chat_future = provider.chat(
            ChatRequest {
                messages: &prepared_messages.messages,
                tools: request_tools,
            },
            active_model.as_str(),
            temperature,
        );

        let chat_result = if let Some(token) = cancellation_token.as_ref() {
            tokio::select! {
                () = token.cancelled() => return Err(ToolLoopCancelled.into()),
                result = chat_future => result,
            }
        } else {
            chat_future.await
        };

        let (
            response_text,
            mut parsed_text,
            mut tool_calls,
            mut assistant_history_content,
            mut native_tool_calls,
        ) = match chat_result {
            Ok(resp) => {
                observer.record_event(&ObserverEvent::LlmResponse {
                    provider: provider_name.to_string(),
                    model: active_model.clone(),
                    duration: llm_started_at.elapsed(),
                    success: true,
                    error_message: None,
                });

                if let Some(dispatcher) = tool_dispatcher {
                    let response_text = resp.text_or_empty().to_string();
                    let (mut parsed_text, mut disp_calls) = dispatcher.parse_response(&resp);
                    if disp_calls.is_empty() {
                        // VL-TTC-010: manifest parser before residual loop_parse.
                        #[cfg(feature = "ai-protocol")]
                        {
                            let (manifest_text, manifest_calls) =
                                velaclaw_agent_runtime::parse_manifest_text_tool_fallback(
                                    &response_text,
                                );
                            if !manifest_calls.is_empty() {
                                if !manifest_text.is_empty() {
                                    parsed_text = manifest_text;
                                }
                                disp_calls = manifest_calls;
                            }
                        }
                        if disp_calls.is_empty() {
                            let (fallback_text, fallback_calls) = parse_tool_calls(&response_text);
                            if !fallback_calls.is_empty() {
                                if !fallback_text.is_empty() {
                                    parsed_text = fallback_text;
                                }
                                disp_calls = fallback_calls
                                    .into_iter()
                                    .map(|c| crate::agent::dispatcher::ParsedToolCall {
                                        name: c.name,
                                        arguments: c.arguments,
                                        tool_call_id: None,
                                    })
                                    .collect();
                            }
                        }
                    }
                    let calls: Vec<ParsedToolCall> = disp_calls
                        .into_iter()
                        .map(|c| ParsedToolCall {
                            name: c.name,
                            arguments: c.arguments,
                        })
                        .collect();
                    let assistant_history_content = if !resp.tool_calls.is_empty() {
                        build_native_assistant_history(&response_text, &resp.tool_calls)
                    } else if !calls.is_empty() {
                        let synthetic: Vec<ToolCall> = calls
                            .iter()
                            .enumerate()
                            .map(|(i, c)| ToolCall {
                                id: format!("text_tool_{i}"),
                                name: c.name.clone(),
                                arguments: c.arguments.to_string(),
                            })
                            .collect();
                        build_assistant_history_with_tool_calls(
                            if parsed_text.is_empty() {
                                response_text.as_str()
                            } else {
                                parsed_text.as_str()
                            },
                            &synthetic,
                        )
                    } else {
                        response_text.clone()
                    };
                    (
                        response_text,
                        parsed_text,
                        calls,
                        assistant_history_content,
                        resp.tool_calls,
                    )
                } else {
                    let response_text = resp.text_or_empty().to_string();
                    // First try native structured tool calls (OpenAI-format).
                    // Fall back to text-based parsing (XML tags, markdown blocks,
                    // GLM format) only if the provider returned no native calls —
                    // this ensures we support both native and prompt-guided models.
                    let mut calls = parse_structured_tool_calls(&resp.tool_calls);
                    let mut parsed_text = String::new();

                    if calls.is_empty() {
                        let (fallback_text, fallback_calls) = parse_tool_calls(&response_text);
                        if !fallback_text.is_empty() {
                            parsed_text = fallback_text;
                        }
                        calls = fallback_calls;
                    }

                    // Preserve native tool call IDs in assistant history so role=tool
                    // follow-up messages can reference the exact call id.
                    let assistant_history_content = if resp.tool_calls.is_empty() {
                        response_text.clone()
                    } else {
                        build_native_assistant_history(&response_text, &resp.tool_calls)
                    };

                    let native_calls = resp.tool_calls;
                    (
                        response_text,
                        parsed_text,
                        calls,
                        assistant_history_content,
                        native_calls,
                    )
                }
            }
            Err(e) => {
                observer.record_event(&ObserverEvent::LlmResponse {
                    provider: provider_name.to_string(),
                    model: active_model.clone(),
                    duration: llm_started_at.elapsed(),
                    success: false,
                    error_message: Some(crate::providers::sanitize_api_error(&e.to_string())),
                });
                #[cfg(feature = "ai-protocol")]
                if let Some(ctx) = &soft_fail {
                    let host_owned = ctx.host_decide_owned();
                    let host = ctx.host_decide.or(host_owned.as_ref());
                    return Err(crate::orchestration::map_provider_limit_error(
                        e,
                        &active_model,
                        ctx.surface,
                        host,
                        ctx.session_key,
                    ));
                }
                return Err(e);
            }
        };

        let mut display_text = if parsed_text.is_empty() {
            response_text.clone()
        } else {
            parsed_text.clone()
        };
        let mut unregistered_ir = 0usize;

        // VL-TTC-015: after envelopes miss, decode line-isolated {name,arguments} IR.
        if tool_calls.is_empty() {
            let allow: std::collections::HashMap<String, String> = tools_registry
                .iter()
                .map(|t| (t.name().to_ascii_lowercase(), t.name().to_string()))
                .collect();
            let decoded = velaclaw_agent_runtime::decode_unwrapped_ir(&response_text, &allow);
            unregistered_ir = decoded.unknown_isolated;
            if !decoded.calls.is_empty() {
                tracing::info!(
                    target: "velaclaw::agent",
                    calls = decoded.calls.len(),
                    "tool_decode: unwrapped IR (no envelope)"
                );
                tool_calls = decoded
                    .calls
                    .into_iter()
                    .map(|c| ParsedToolCall {
                        name: c.name,
                        arguments: c.arguments,
                    })
                    .collect();
                display_text = decoded.remaining;
                parsed_text = display_text.clone();
                let synthetic: Vec<ToolCall> = tool_calls
                    .iter()
                    .enumerate()
                    .map(|(i, c)| ToolCall {
                        id: format!("unwrapped_tool_{i}"),
                        name: c.name.clone(),
                        arguments: c.arguments.to_string(),
                    })
                    .collect();
                assistant_history_content = if text_tool_result_history {
                    build_assistant_history_with_tool_calls(display_text.as_str(), &synthetic)
                } else {
                    build_native_assistant_history(display_text.as_str(), &synthetic)
                };
                native_tool_calls = synthetic;
            } else if unregistered_ir > 0 {
                // Carrier stripped; turn continues without executing unknown tools.
                display_text = decoded.remaining;
                parsed_text = display_text.clone();
            }
        }

        if tool_calls.is_empty()
            && velaclaw_agent_runtime::needs_tool_format_correction(&response_text, 0)
        {
            let names: Vec<String> = tools_registry
                .iter()
                .map(|t| t.name().to_string())
                .collect();
            match try_ir_repair(
                provider,
                &active_model,
                &response_text,
                &names,
                cancellation_token.as_ref(),
                observer,
                provider_name,
            )
            .await?
            {
                Some(repaired) if !repaired.is_empty() => {
                    tracing::info!(
                        target: "velaclaw::agent",
                        repaired = repaired.len(),
                        "tool_format_repair: injected IR into shared tool loop"
                    );
                    tool_calls = repaired
                        .into_iter()
                        .map(|c| ParsedToolCall {
                            name: c.name,
                            arguments: c.arguments,
                        })
                        .collect();
                    let synthetic: Vec<ToolCall> = tool_calls
                        .iter()
                        .enumerate()
                        .map(|(i, c)| ToolCall {
                            id: format!("repair_tool_{i}"),
                            name: c.name.clone(),
                            arguments: c.arguments.to_string(),
                        })
                        .collect();
                    let history_text = if parsed_text.is_empty() {
                        response_text.as_str()
                    } else {
                        parsed_text.as_str()
                    };
                    assistant_history_content = if text_tool_result_history {
                        build_assistant_history_with_tool_calls(history_text, &synthetic)
                    } else {
                        build_native_assistant_history(history_text, &synthetic)
                    };
                    native_tool_calls = synthetic;
                }
                _ => {}
            }
        }

        if tool_calls.is_empty()
            && !peer_continue_used
            && velaclaw_agent_runtime::needs_tool_format_correction(&response_text, 0)
        {
            let peers = soft_fail
                .as_ref()
                .map(|c| c.peer_logical_ids)
                .unwrap_or(&[])
                .to_vec();
            let peers = if peers.is_empty() {
                soft_fail
                    .as_ref()
                    .and_then(|c| c.config)
                    .map(logical_ids_from_config)
                    .unwrap_or_default()
            } else {
                peers
            };
            if let Some(peer) = select_peer_continue_model(&active_model, &peers) {
                peer_continue_used = true;
                tracing::info!(
                    target: "velaclaw::agent",
                    from = %active_model,
                    to = %peer,
                    "tool_format_peer_continue: retrying shared loop with catalog peer"
                );
                active_model = peer;
                continue;
            }
        }

        if tool_calls.is_empty() {
            // VL-TTC-013: Repair already attempted above. Remaining markup → strip.
            let strip_fail_closed =
                velaclaw_agent_runtime::needs_tool_format_correction(&response_text, 0);
            if strip_fail_closed {
                tracing::warn!(
                    target: "velaclaw::agent",
                    "tool_format_repair_exhausted: stripping markup after IR extract miss"
                );
            }
            let known: std::collections::HashSet<String> = tools_registry
                .iter()
                .map(|t| t.name().to_ascii_lowercase())
                .collect();
            // Sanitize before streaming so CLI/Web never paint the carrier (GOV-007 shared path).
            let mut final_text = crate::util::strip_tool_call_markup(&display_text);
            final_text =
                velaclaw_agent_runtime::strip_isolated_tool_json_artifacts(&final_text, &known);
            if strip_fail_closed {
                let surface = soft_fail
                    .as_ref()
                    .map(|c| c.surface)
                    .unwrap_or(velaclaw_agent_runtime::SoftFailSurface::Cli);
                let session_key = soft_fail.as_ref().map(|c| c.session_key).unwrap_or("");
                #[cfg(feature = "ai-protocol")]
                {
                    let host_owned = soft_fail.as_ref().and_then(|c| c.host_decide_owned());
                    let host = soft_fail
                        .as_ref()
                        .and_then(|c| c.host_decide)
                        .or(host_owned.as_ref());
                    final_text = crate::orchestration::finalize_tool_format_exhausted(
                        &final_text,
                        &active_model,
                        surface,
                        host,
                        session_key,
                    );
                }
                #[cfg(not(feature = "ai-protocol"))]
                {
                    final_text = velaclaw_agent_runtime::append_tool_format_exhausted_notice(
                        &final_text,
                        &active_model,
                        surface,
                    );
                }
            } else if unregistered_ir > 0 {
                final_text = velaclaw_agent_runtime::append_unregistered_ir_notice(&final_text);
            }
            // Progressive draft: sanitized text only (no tool JSON / 入入 wrappers).
            if let Some(ref tx) = on_delta {
                let mut chunk = String::new();
                for word in final_text.split_inclusive(char::is_whitespace) {
                    if cancellation_token
                        .as_ref()
                        .is_some_and(CancellationToken::is_cancelled)
                    {
                        return Err(ToolLoopCancelled.into());
                    }
                    chunk.push_str(word);
                    if chunk.len() >= STREAM_CHUNK_MIN_CHARS
                        && tx.send(std::mem::take(&mut chunk)).await.is_err()
                    {
                        break;
                    }
                }
                if !chunk.is_empty() {
                    let _ = tx.send(chunk).await;
                }
            }
            history.push(ChatMessage::assistant(response_text.clone()));
            return Ok(final_text);
        }

        // Print any text the LLM produced alongside tool calls (unless silent)
        let known: std::collections::HashSet<String> = tools_registry
            .iter()
            .map(|t| t.name().to_ascii_lowercase())
            .collect();
        let visible_text = velaclaw_agent_runtime::strip_isolated_tool_json_artifacts(
            &crate::util::strip_tool_call_markup(&display_text),
            &known,
        );
        if !silent && !visible_text.is_empty() {
            let rendered = render_opts.render(&visible_text);
            let prefixed = prefix_agent_lines(&rendered, render_opts.style);
            print!("{prefixed}");
            let _ = std::io::stdout().flush();
        }

        // Execute tool calls and build results. `individual_results` tracks per-call output so
        // native-mode history can emit one role=tool message per tool call with the correct ID.
        //
        // When multiple tool calls are present and interactive CLI approval is not needed, run
        // tool executions concurrently for lower wall-clock latency.
        let mut tool_results = String::new();
        let mut skip_outputs: Vec<Option<String>> = vec![None; tool_calls.len()];
        let mut runnable: Vec<ParsedToolCall> = Vec::new();
        let mut runnable_idx: Vec<usize> = Vec::new();
        for (i, call) in tool_calls.iter().enumerate() {
            let is_shell = call.name.eq_ignore_ascii_case("shell");
            if is_shell {
                let fp =
                    crate::agent::probe_dedup::tool_probe_fingerprint(&call.name, &call.arguments);
                let decision = with_probe(soft_fail, &mut local_probe, |g| g.decide_shell(&fp));
                match decision {
                    crate::agent::probe_dedup::ProbeShellDecision::Cap => {
                        skip_outputs[i] =
                            Some(crate::agent::probe_dedup::SHELL_ROUND_CAP_NOTICE.into());
                        continue;
                    }
                    crate::agent::probe_dedup::ProbeShellDecision::SkipRepeat => {
                        skip_outputs[i] =
                            Some(crate::agent::probe_dedup::REPEAT_PROBE_NOTICE.into());
                        continue;
                    }
                    crate::agent::probe_dedup::ProbeShellDecision::Run => {}
                }
            }
            runnable.push(call.clone());
            runnable_idx.push(i);
        }
        let mut batch_outputs: Vec<String> = vec![String::new(); tool_calls.len()];
        if !runnable.is_empty() {
            let batch_results = tool_batch::execute_tool_batch(
                &runnable,
                tools_registry,
                observer,
                approval,
                security,
                channel_name,
                channel_approval.clone(),
                cancellation_token.as_ref(),
                gate_extras,
            )
            .await?;
            for (call_i, result) in runnable_idx.into_iter().zip(batch_results) {
                batch_outputs[call_i] = result.output;
            }
        }
        for (i, skip) in skip_outputs.into_iter().enumerate() {
            if let Some(msg) = skip {
                batch_outputs[i] = msg;
            }
        }
        let mut counted_round = false;
        for (call, out) in tool_calls.iter().zip(batch_outputs.iter()) {
            if !call.name.eq_ignore_ascii_case("shell") {
                continue;
            }
            if crate::agent::probe_dedup::shell_output_counts_as_round(out) {
                counted_round = true;
            } else {
                let fp =
                    crate::agent::probe_dedup::tool_probe_fingerprint(&call.name, &call.arguments);
                with_probe(soft_fail, &mut local_probe, |g| {
                    g.retract_unexecuted(&fp);
                });
            }
        }
        if counted_round {
            with_probe(soft_fail, &mut local_probe, |g| {
                g.record_executed_round();
            });
        }
        let individual_results = batch_outputs;

        for (call, result) in tool_calls.iter().zip(individual_results.iter()) {
            let _ = writeln!(
                tool_results,
                "<tool_result name=\"{}\">\n{}\n</tool_result>",
                call.name, result
            );
        }

        if !silent {
            let _ = std::io::stdout().flush();
        }

        // Add assistant message with tool calls + tool results to history.
        // Native mode: use JSON-structured messages so convert_messages() can
        // reconstruct proper OpenAI-format tool_calls and tool result messages.
        // Prompt mode: use XML-based text format as before.
        history.push(ChatMessage::assistant(assistant_history_content));
        if native_tool_calls.is_empty() || text_tool_result_history {
            history.push(ChatMessage::user(format!("[Tool results]\n{tool_results}")));
        } else {
            for (native_call, result) in native_tool_calls.iter().zip(individual_results.iter()) {
                history.push(ChatMessage::tool_with_call_id(&native_call.id, result));
            }
        }
    }

    anyhow::bail!("Agent exceeded maximum tool iterations ({max_iterations})")
}

/// Catalog logical ids from `[[model_routes]]` (not priced Decide fallbacks).
pub(crate) fn logical_ids_from_config(config: &Config) -> Vec<String> {
    config
        .model_routes
        .iter()
        .map(|r| {
            if r.model.contains('/') {
                r.model.clone()
            } else {
                format!("{}/{}", r.provider, r.model)
            }
        })
        .collect()
}

/// First remaining catalog id, sorted by name — capability list, not cost.
pub(crate) fn select_peer_continue_model(current: &str, peers: &[String]) -> Option<String> {
    let mut ids: Vec<&String> = peers
        .iter()
        .filter(|id| !id.is_empty() && id.as_str() != current)
        .collect();
    ids.sort_unstable();
    ids.into_iter().next().cloned()
}

/// Isolated Repair completion: blob → allowlisted IR. Network errors become `Ok(None)`.
async fn try_ir_repair(
    provider: &dyn Provider,
    model: &str,
    failed_blob: &str,
    allowlisted_names: &[String],
    cancellation_token: Option<&CancellationToken>,
    observer: &dyn Observer,
    provider_name: &str,
) -> Result<Option<Vec<velaclaw_agent_runtime::RepairedToolCall>>> {
    if cancellation_token.is_some_and(CancellationToken::is_cancelled) {
        return Err(ToolLoopCancelled.into());
    }
    if allowlisted_names.is_empty() {
        return Ok(None);
    }

    let system = velaclaw_agent_runtime::repair_extract_system_prompt(allowlisted_names);
    let blob = velaclaw_agent_runtime::truncate_repair_blob(failed_blob);
    let messages = [
        ChatMessage::system(system),
        ChatMessage::user(format!(
            "Extract tool calls from this assistant message:\n\n{blob}"
        )),
    ];

    observer.record_event(&ObserverEvent::LlmRequest {
        provider: provider_name.to_string(),
        model: model.to_string(),
        messages_count: messages.len(),
    });
    let started = Instant::now();
    let chat_future = provider.chat(
        ChatRequest {
            messages: &messages,
            tools: None,
        },
        model,
        0.0,
    );
    let chat_result = if let Some(token) = cancellation_token {
        tokio::select! {
            () = token.cancelled() => return Err(ToolLoopCancelled.into()),
            result = chat_future => result,
        }
    } else {
        chat_future.await
    };

    match chat_result {
        Ok(resp) => {
            observer.record_event(&ObserverEvent::LlmResponse {
                provider: provider_name.to_string(),
                model: model.to_string(),
                duration: started.elapsed(),
                success: true,
                error_message: None,
            });
            let allow: std::collections::HashSet<String> =
                allowlisted_names.iter().cloned().collect();
            Ok(Some(velaclaw_agent_runtime::parse_repaired_tool_calls(
                resp.text_or_empty(),
                &allow,
            )))
        }
        Err(e) => {
            observer.record_event(&ObserverEvent::LlmResponse {
                provider: provider_name.to_string(),
                model: model.to_string(),
                duration: started.elapsed(),
                success: false,
                error_message: Some(crate::providers::sanitize_api_error(&e.to_string())),
            });
            tracing::warn!(
                target: "velaclaw::agent",
                error = %crate::providers::sanitize_api_error(&e.to_string()),
                "tool_format_repair: extract call failed; stripping"
            );
            Ok(None)
        }
    }
}

/// Surface configured autonomy/shell/path policy in the system prompt.
pub(crate) fn append_execution_policy_to_prompt(
    system_prompt: &mut String,
    security: &PolicyHandle,
    config: &Config,
) {
    let http = config
        .http_request
        .effective_for_autonomy(security.autonomy());
    let (self_adjust_allowed_writes, self_adjust_denied_writes) = self_adjust_prompt_fields(config);
    let extras = crate::security::PolicyPromptExtras {
        http_request_enabled: http.enabled,
        proxy_enabled: config.proxy.enabled,
        proxy_http: if config.proxy.enabled {
            config.proxy.http_proxy.clone()
        } else {
            None
        },
        self_adjust_allowed_writes,
        self_adjust_denied_writes,
        policy_patch_enabled: cfg!(feature = "ai-protocol"),
        runtime_kind: config.runtime.kind.clone(),
        sandbox_name: crate::security::describe_effective_sandbox(&config.security)
            .name
            .to_string(),
    };
    security.append_execution_policy_prompt(system_prompt, &extras);
    if http.enabled && http.allow_private_hosts {
        system_prompt.push_str(
            "- HTTP LAN access: enabled for private/local hosts when `autonomy.level = full`.\n\n",
        );
    }
    append_hitl_continuity_guidance(system_prompt);
}

/// Web/CLI HITL: keep the agent in the loop; modals are for short credentials/choices only.
pub(crate) fn append_hitl_continuity_guidance(system_prompt: &mut String) {
    system_prompt.push_str(
        "## Human-in-the-loop (keep task continuity)\n\n\
             - You are the agent: run work with tools (`shell`, etc.). When policy needs approval, \
           the UI shows Deny / Allow once / Always / Never — wait for that, then continue the same turn.\n\
         - Use `request_human_input` only for short operator input: `choice` (buttons), \
           `secret` (password/token → secret_slot), or `text` (short codes ≤128 chars).\n\
         - Do **not** ask the human to run terminal commands and paste results back into chat \
           or a modal. That is not an agent workflow.\n\
         - Prefer `shell` + approval over `handoff`. Never collect command logs via \
           `request_human_input`.\n\n",
    );
}

fn self_adjust_prompt_fields(config: &Config) -> (Vec<String>, Vec<String>) {
    #[cfg(feature = "ai-protocol")]
    {
        match crate::config::discover_and_load(config) {
            Ok(Some(layer)) => {
                if let Some(section) = layer.self_adjust {
                    return (section.allowed_writes, section.denied_writes);
                }
                (
                    vec![
                        "approval.session_allowlist".into(),
                        "approval.session_shell_binaries".into(),
                        "approval.*".into(),
                    ],
                    vec![
                        "security".into(),
                        "security.*".into(),
                        "gateway".into(),
                        "gateway.*".into(),
                        "channels".into(),
                        "channels.*".into(),
                    ],
                )
            }
            Ok(None) | Err(_) => (Vec::new(), Vec::new()),
        }
    }
    #[cfg(not(feature = "ai-protocol"))]
    {
        let _ = config;
        (Vec::new(), Vec::new())
    }
}

// ── CLI Entrypoint ───────────────────────────────────────────────────────
// Wires up all subsystems (observer, runtime, security, memory, tools,
// provider, hardware RAG, peripherals) and enters either single-shot or
// interactive REPL mode. The interactive loop manages history compaction
// and hard trimming to keep the context window bounded.

/// Shared turn-model ladder for CLI (same as Web `Agent::turn`).
#[cfg(feature = "ai-protocol")]
pub(crate) fn resolve_cli_turn_model(
    config: &Config,
    user_message: &str,
    session_key: &str,
    default_model: &str,
    explicit_model: Option<&str>,
    available_hints: &[String],
) -> Result<String> {
    let host_decide = crate::orchestration::HostDecideHost::from_config(config);
    let intent_route = crate::agent::intent_route::IntentRouteHost::from_config(config);
    let req = crate::orchestration::TurnModelRequest {
        user_message,
        session_key,
        default_model,
        explicit_model,
        host_decide: Some(&host_decide),
        intent_route: Some(&intent_route),
        classification: &config.query_classification,
        available_hints,
    };
    Ok(crate::orchestration::resolve_turn_model(&req)?.model)
}

#[cfg(not(feature = "ai-protocol"))]
pub(crate) fn resolve_cli_turn_model(
    config: &Config,
    user_message: &str,
    _session_key: &str,
    default_model: &str,
    _explicit_model: Option<&str>,
    available_hints: &[String],
) -> Result<String> {
    Ok(crate::agent::classifier::resolve_model_for_message(
        &config.query_classification,
        available_hints,
        default_model,
        user_message,
    ))
}

#[cfg(test)]
mod peer_continue_tests {
    use super::select_peer_continue_model;

    #[test]
    fn select_peer_skips_current_and_sorts_lexically() {
        let peers = vec!["zeta/m".into(), "alpha/m".into(), "cur/m".into()];
        assert_eq!(
            select_peer_continue_model("cur/m", &peers).as_deref(),
            Some("alpha/m")
        );
        assert_eq!(
            select_peer_continue_model("only", &[String::from("only")]),
            None
        );
    }
}

#[cfg(test)]
mod hitl_prompt_tests {
    use super::append_hitl_continuity_guidance;

    #[test]
    fn hitl_guidance_rejects_paste_results_workflow() {
        let mut prompt = String::new();
        append_hitl_continuity_guidance(&mut prompt);
        assert!(prompt.contains("Human-in-the-loop"));
        assert!(prompt.contains("request_human_input"));
        assert!(prompt.contains("Do **not** ask the human to run terminal commands"));
        assert!(prompt.contains("shell") && prompt.contains("approval"));
    }
}
