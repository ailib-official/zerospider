use crate::agent::tool_batch::{self, ParsedToolCall};
use crate::agent::turn_progress::{get_fold_payload, FoldCache};
use crate::approval::{ApprovalManager, ChannelApprovalSession};
use crate::cli_render::{format_user_prompt, prefix_agent_lines, RenderOpts, RenderStyle};
use crate::config::Config;
use crate::memory::{self, Memory, MemoryCategory};
use crate::multimodal;
use crate::observability::{Observer, ObserverEvent};
use crate::providers::{ChatMessage, ChatRequest, Provider, ProviderCapabilityError, ToolCall};
use crate::security::PolicyHandle;
use crate::tools::Tool;
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::io::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use velaclaw_agent_runtime::loop_parse::{
    self, build_assistant_history_with_tool_calls, build_native_assistant_history,
    ToolLoopCancelled, DEFAULT_MAX_TOOL_ITERATIONS,
};

pub(crate) use velaclaw_agent_runtime::loop_parse::{
    build_tool_instructions, is_tool_loop_cancelled,
};

/// Minimum characters per chunk when relaying LLM text to a streaming draft.
const STREAM_CHUNK_MIN_CHARS: usize = 80;

/// Minimum user-message length (in chars) for auto-save to memory.
/// Matches the channel-side constant in `channels/mod.rs`.
const AUTOSAVE_MIN_MESSAGE_CHARS: usize = 20;

fn autosave_memory_key(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::new_v4())
}

fn parse_tool_calls(response: &str) -> (String, Vec<ParsedToolCall>) {
    let (text, calls) = loop_parse::parse_tool_calls(response);
    (text, calls.into_iter().map(to_local_call).collect())
}

fn parse_structured_tool_calls(tool_calls: &[ToolCall]) -> Vec<ParsedToolCall> {
    loop_parse::parse_structured_tool_calls(tool_calls)
        .into_iter()
        .map(to_local_call)
        .collect()
}

fn to_local_call(c: velaclaw_agent_runtime::ParsedToolCall) -> ParsedToolCall {
    ParsedToolCall {
        name: c.name,
        arguments: c.arguments,
    }
}

fn parse_arguments_value(raw: Option<&serde_json::Value>) -> serde_json::Value {
    loop_parse::parse_arguments_value(raw)
}

fn parse_tool_call_value(value: &serde_json::Value) -> Option<ParsedToolCall> {
    loop_parse::parse_tool_call_value(value).map(to_local_call)
}

fn parse_tool_calls_from_json_value(value: &serde_json::Value) -> Vec<ParsedToolCall> {
    loop_parse::parse_tool_calls_from_json_value(value)
        .into_iter()
        .map(to_local_call)
        .collect()
}

fn extract_json_values(input: &str) -> Vec<serde_json::Value> {
    loop_parse::extract_json_values(input)
}

fn parse_glm_style_tool_calls(text: &str) -> Vec<(String, serde_json::Value, Option<String>)> {
    loop_parse::parse_glm_style_tool_calls(text)
}

/// Build context preamble by searching memory for relevant entries.
/// Entries with a hybrid score below `min_relevance_score` are dropped to
/// prevent unrelated memories from bleeding into the conversation.
///
/// VL-MEM-001: when `session_id` is set, Conversation/Daily inject only for
/// that session; Core always may inject; legacy `session_id=None` Conversation
/// is excluded.
async fn build_context(
    mem: &dyn Memory,
    user_msg: &str,
    min_relevance_score: f64,
    session_id: Option<&str>,
) -> String {
    let mut context = String::new();

    // Pull relevant memories for this message (no SQL session filter so Core
    // with session_id=None remains visible; apply inject rules below).
    if let Ok(entries) = mem.recall(user_msg, 5, None).await {
        let relevant: Vec<_> = entries
            .iter()
            .filter(|e| match e.score {
                Some(score) => score >= min_relevance_score,
                None => true,
            })
            .filter(|e| memory::should_inject_for_session(e, session_id))
            .collect();

        if !relevant.is_empty() {
            context.push_str("[Memory context]\n");
            for entry in &relevant {
                if memory::is_assistant_autosave_key(&entry.key) {
                    continue;
                }
                let _ = writeln!(context, "- {}: {}", entry.key, entry.content);
            }
            if context == "[Memory context]\n" {
                context.clear();
            } else {
                context.push('\n');
            }
        }
    }

    context
}

/// Build hardware datasheet context from RAG when peripherals are enabled.
/// Includes pin-alias lookup (e.g. "red_led" → 13) when query matches, plus retrieved chunks.
fn build_hardware_context(
    rag: &crate::rag::HardwareRag,
    user_msg: &str,
    boards: &[String],
    chunk_limit: usize,
) -> String {
    if rag.is_empty() || boards.is_empty() {
        return String::new();
    }

    let mut context = String::new();

    // Pin aliases: when user says "red led", inject "red_led: 13" for matching boards
    let pin_ctx = rag.pin_alias_context(user_msg, boards);
    if !pin_ctx.is_empty() {
        context.push_str(&pin_ctx);
    }

    let chunks = rag.retrieve(user_msg, boards, chunk_limit);
    if chunks.is_empty() && pin_ctx.is_empty() {
        return String::new();
    }

    if !chunks.is_empty() {
        context.push_str("[Hardware documentation]\n");
    }
    for chunk in chunks {
        let board_tag = chunk.board.as_deref().unwrap_or("generic");
        let _ = writeln!(
            context,
            "--- {} ({}) ---\n{}\n",
            chunk.source, board_tag, chunk.content
        );
    }
    context.push('\n');
    context
}

pub(crate) mod tool_loop;
pub(crate) use tool_loop::{
    agent_turn, append_execution_policy_to_prompt, append_text_tool_prompt,
    logical_ids_from_config, resolve_cli_turn_model, run_tool_call_loop, SoftFailLoopCtx,
};

/// Extra CLI/daemon options for [`run`] (keeps the argument list clippy-clean).
pub struct AgentRunOpts<'a> {
    pub extra_prompt_phases: &'a [crate::agent::prompt_composer::PromptPhase],
    pub host_phase: crate::agent::host_phase::HostPhase,
    pub chat_session_id: Option<String>,
    pub persist_chat_session: bool,
}

impl<'a> AgentRunOpts<'a> {
    pub fn phases(extra_prompt_phases: &'a [crate::agent::prompt_composer::PromptPhase]) -> Self {
        Self {
            extra_prompt_phases,
            host_phase: crate::agent::host_phase::HostPhase::Build,
            chat_session_id: None,
            persist_chat_session: false,
        }
    }
}

/// HITL channel name for `loop_::run`. Cron/heartbeat reuse the CLI assemble path
/// but must not claim stdin (`interactive_shell_approval`); elevation stays denied
/// and sandbox wrap is not skipped (VL-SEC-011 / GOV-007).
fn approval_channel_for_phases(
    extra_prompt_phases: &[crate::agent::prompt_composer::PromptPhase],
) -> &'static str {
    use crate::agent::prompt_composer::PromptPhase;
    if extra_prompt_phases.contains(&PromptPhase::Cron) {
        "cron"
    } else if extra_prompt_phases.contains(&PromptPhase::Heartbeat) {
        "heartbeat"
    } else {
        "cli"
    }
}

pub async fn run(
    mut config: Config,
    message: Option<String>,
    provider_override: Option<String>,
    model_override: Option<String>,
    temperature: f64,
    peripheral_overrides: Vec<String>,
    no_color: bool,
    no_fold: bool,
    opts: AgentRunOpts<'_>,
) -> Result<String> {
    let extra_prompt_phases = opts.extra_prompt_phases;
    let approval_channel = approval_channel_for_phases(extra_prompt_phases);
    let host_phase = opts.host_phase;
    let chat_session_id = opts.chat_session_id;
    let persist_chat_session = opts.persist_chat_session;
    // CLI `-p/--model` must win over config for both protocol and legacy paths.
    // (Previously the ai-protocol branch discarded these and always used config.)
    let cli_explicit_flags = provider_override
        .as_ref()
        .is_some_and(|s| !s.trim().is_empty())
        || model_override
            .as_ref()
            .is_some_and(|s| !s.trim().is_empty());
    if let Some(provider) = provider_override {
        let provider = provider.trim();
        if !provider.is_empty() {
            config.default_provider = Some(provider.to_string());
        }
    }
    if let Some(model) = model_override {
        let model = model.trim();
        if !model.is_empty() {
            config.default_model = Some(model.to_string());
        }
    }

    let interactive = message.is_none();
    let render_opts =
        RenderOpts::from_config(config.cli_render.as_ref(), no_color, no_fold, interactive);
    let fold_cache: FoldCache = Arc::new(Mutex::new(HashMap::new()));

    // ── Canonical stack (VL-REVIEW2-A0 / GOV-007) ─────────────────
    let mut assembled = crate::agent::assemble::assemble_runtime(
        &config,
        crate::config::BootstrapOptions {
            with_embedding_routes: false,
        },
    )?;
    let observer = assembled.boot.observer.clone();
    let security = assembled.boot.security.clone();
    let mem = assembled.boot.memory.clone();
    tracing::info!(backend = mem.name(), "Memory initialized");

    // ── Peripherals (merge peripheral tools into registry) ─
    if !peripheral_overrides.is_empty() {
        tracing::info!(
            peripherals = ?peripheral_overrides,
            "Peripheral overrides from CLI (config boards take precedence)"
        );
    }

    let mut tools_registry = std::mem::take(&mut assembled.boot.tools);
    let peripheral_tools: Vec<Box<dyn Tool>> =
        crate::peripherals::create_peripheral_tools(&config.peripherals).await?;
    if !peripheral_tools.is_empty() {
        tracing::info!(count = peripheral_tools.len(), "Peripheral tools added");
        tools_registry.extend(peripheral_tools);
    }

    let provider = assembled.provider;
    let model_name = assembled.model_name;
    let text_tool_result_history = assembled.text_tool_result_history;
    let tool_dispatcher = Some(assembled.tool_dispatcher);

    let provider_name = model_name
        .split_once('/')
        .map_or(model_name.as_str(), |(provider, _)| provider);

    let available_hints: Vec<String> = config
        .model_routes
        .iter()
        .map(|route| route.hint.clone())
        .collect();
    let catalog_peers = logical_ids_from_config(&config);

    observer.record_event(&ObserverEvent::AgentStart {
        provider: model_name
            .split_once('/')
            .map_or(model_name.as_str(), |(provider, _)| provider)
            .to_string(),
        model: model_name.to_string(),
    });

    // ── Hardware RAG (datasheet retrieval when peripherals + datasheet_dir) ──
    let hardware_rag: Option<crate::rag::HardwareRag> = config
        .peripherals
        .datasheet_dir
        .as_ref()
        .filter(|d| !d.trim().is_empty())
        .map(|dir| crate::rag::HardwareRag::load(&config.workspace_dir, dir.trim()))
        .and_then(Result::ok)
        .filter(|r: &crate::rag::HardwareRag| !r.is_empty());
    if let Some(ref rag) = hardware_rag {
        tracing::info!(chunks = rag.len(), "Hardware RAG loaded");
    }

    let board_names: Vec<String> = config
        .peripherals
        .boards
        .iter()
        .map(|b| b.board.clone())
        .collect();

    // ── Build system prompt from workspace MD files (OpenClaw framework) ──
    let skills = crate::skills::load_skills_with_config(&config.workspace_dir, &config);
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
    tool_descs.push((
        "cron_add",
        "Create a cron job. Supports schedule kinds: cron, at, every; and job types: shell or agent.",
    ));
    tool_descs.push((
        "cron_list",
        "List all cron jobs with schedule, status, and metadata.",
    ));
    tool_descs.push(("cron_remove", "Remove a cron job by job_id."));
    tool_descs.push((
        "cron_update",
        "Patch a cron job (schedule, enabled, command/prompt, model, delivery, session_target).",
    ));
    tool_descs.push((
        "cron_run",
        "Force-run a cron job immediately and record a run history entry.",
    ));
    tool_descs.push(("cron_runs", "Show recent run history for a cron job."));
    tool_descs.push((
        "screenshot",
        "Capture a screenshot of the current screen. Returns file path and base64-encoded PNG. Use when: visual verification, UI inspection, debugging displays.",
    ));
    tool_descs.push((
        "image_info",
        "Read image file metadata (format, dimensions, size) and optionally base64-encode it. Use when: inspecting images, preparing visual data for analysis.",
    ));
    if config.browser.enabled {
        tool_descs.push((
            "browser_open",
            "Open approved HTTPS URLs in Brave Browser (allowlist-only, no scraping)",
        ));
    }
    if config.composio.enabled {
        tool_descs.push((
            "composio",
            "Execute actions on 1000+ apps via Composio (Gmail, Notion, GitHub, Slack, etc.). Use action='list' to discover, 'execute' to run (optionally with connected_account_id), 'connect' to OAuth.",
        ));
    }
    tool_descs.push((
        "schedule",
        "Manage scheduled tasks (create/list/get/cancel/pause/resume). Supports recurring cron and one-shot delays.",
    ));
    if !config.agents.is_empty() {
        tool_descs.push((
            "delegate",
            "Delegate a sub-task to a specialized agent. Use when: task needs different model/capability, or to parallelize work.",
        ));
    }
    if config.peripherals.enabled && !config.peripherals.boards.is_empty() {
        tool_descs.push((
            "gpio_read",
            "Read GPIO pin value (0 or 1) on connected hardware (STM32, Arduino). Use when: checking sensor/button state, LED status.",
        ));
        tool_descs.push((
            "gpio_write",
            "Set GPIO pin high (1) or low (0) on connected hardware. Use when: turning LED on/off, controlling actuators.",
        ));
        tool_descs.push((
            "arduino_upload",
            "Upload agent-generated Arduino sketch. Use when: user asks for 'make a heart', 'blink pattern', or custom LED behavior on Arduino. You write the full .ino code; VelaClaw compiles and uploads it. Pin 13 = built-in LED on Uno.",
        ));
        tool_descs.push((
            "hardware_memory_map",
            "Return flash and RAM address ranges for connected hardware. Use when: user asks for 'upper and lower memory addresses', 'memory map', or 'readable addresses'.",
        ));
        tool_descs.push((
            "hardware_board_info",
            "Return full board info (chip, architecture, memory map) for connected hardware. Use when: user asks for 'board info', 'what board do I have', 'connected hardware', 'chip info', or 'what hardware'.",
        ));
        tool_descs.push((
            "hardware_memory_read",
            "Read actual memory/register values from Nucleo via USB. Use when: user asks to 'read register values', 'read memory', 'dump lower memory 0-126', 'give address and value'. Params: address (hex, default 0x20000000), length (bytes, default 128).",
        ));
        tool_descs.push((
            "hardware_capabilities",
            "Query connected hardware for reported GPIO pins and LED pin. Use when: user asks what pins are available.",
        ));
    }
    let bootstrap_max_chars = if config.agent.compact_context {
        Some(6000)
    } else {
        None
    };
    let prompt_budget = crate::agent::prompt_composer::system_prompt_char_budget(
        config.agent.compact_context,
        &model_name,
    );
    let native_tools = provider.supports_native_tools();
    let mut system_prompt = crate::channels::build_system_prompt_pyramid(
        &config.workspace_dir,
        &model_name,
        &tool_descs,
        &skills,
        Some(&config.identity),
        bootstrap_max_chars,
        native_tools,
        config.skills.prompt_injection_mode,
        crate::agent::prompt_composer::PromptMode::Full,
        prompt_budget,
    );

    // Append structured tool-use instructions (Hybrid / xml mode; native-only Full skips).
    #[cfg(feature = "ai-protocol")]
    {
        if let Some(ref dispatcher) = tool_dispatcher {
            let strategy = if text_tool_result_history {
                ai_lib_rust::NativeStrategy::Hybrid
            } else if dispatcher.should_send_tool_specs() {
                ai_lib_rust::NativeStrategy::Full
            } else {
                ai_lib_rust::NativeStrategy::TextOnly
            };
            append_text_tool_prompt(
                &mut system_prompt,
                dispatcher.as_ref(),
                &tools_registry,
                strategy,
            );
        } else if !native_tools {
            system_prompt.push_str(&build_tool_instructions(&tools_registry));
        }
    }
    #[cfg(not(feature = "ai-protocol"))]
    {
        if !native_tools {
            system_prompt.push_str(&build_tool_instructions(&tools_registry));
        }
    }
    append_execution_policy_to_prompt(&mut system_prompt, &security, &config);
    crate::agent::prompt_composer::append_phase_sections(
        &mut system_prompt,
        &crate::agent::prompt_composer::default_run_prompt_phases(extra_prompt_phases),
    );
    if let Some(note) = host_phase.system_note() {
        system_prompt.push_str("\n\n");
        system_prompt.push_str(note);
    }

    let tool_dispatcher_ref = tool_dispatcher.as_deref();

    // ── Approval manager (supervised mode) ───────────────────────
    let effective_autonomy = crate::config::resolve_effective_autonomy(&config)?;
    let approval_wiring = crate::config::ApprovalManagerWiring::from_config(&config)?;
    let approval_manager = approval_wiring.spawn_manager(&effective_autonomy);
    let cli_gate_extras = crate::agent::tool_batch::ToolBatchGateExtras {
        approval_hub: None,
        human_input_hub: None,
        host_phase,
    };

    // ── Execute ──────────────────────────────────────────────────
    let start = Instant::now();

    let mut final_output = String::new();

    if let Some(msg) = message {
        // One-shot: memory session stays isolated; optional ChatSessionStore resume (R8).
        let (chat_store_id, prior_chat) = if persist_chat_session {
            crate::agent::session_resume::load_or_create_session(
                &config.workspace_dir,
                chat_session_id.as_deref(),
            )
            .await?
        } else {
            (String::new(), Vec::new())
        };
        let session_id = if persist_chat_session && !chat_store_id.is_empty() {
            chat_store_id.clone()
        } else {
            memory::new_session_id()
        };

        // Auto-save user message to memory (skip short/trivial messages)
        if config.memory.auto_save && msg.chars().count() >= AUTOSAVE_MIN_MESSAGE_CHARS {
            let user_key = autosave_memory_key("user_msg");
            let _ = mem
                .store(
                    &user_key,
                    &msg,
                    MemoryCategory::Conversation,
                    Some(session_id.as_str()),
                )
                .await;
        }

        // Inject memory + hardware RAG context into user message
        let mem_context = if config.agent.envelope_assemble {
            String::new()
        } else {
            build_context(
                mem.as_ref(),
                &msg,
                config.memory.min_relevance_score,
                Some(session_id.as_str()),
            )
            .await
        };
        let rag_limit = if config.agent.compact_context { 2 } else { 5 };
        let hw_context = hardware_rag
            .as_ref()
            .map(|r| build_hardware_context(r, &msg, &board_names, rag_limit))
            .unwrap_or_default();
        let context = format!("{mem_context}{hw_context}");
        let enriched = if context.is_empty() {
            msg.clone()
        } else {
            format!("{context}{msg}")
        };

        let mut history = vec![ChatMessage::system(&system_prompt)];
        history.extend(prior_chat);
        history.push(ChatMessage::user(&enriched));

        let summarizer = crate::agent::context_orch::HistorySummarizer {
            provider: provider.as_ref(),
            model: &model_name,
        };
        #[cfg(feature = "ai-protocol")]
        let skip_session_prepare = crate::agent::bounded_dag_live::skip_session_prepare_for_live(
            config.agent.bounded_dag_live,
            host_phase,
        );
        #[cfg(not(feature = "ai-protocol"))]
        let skip_session_prepare = false;
        if !skip_session_prepare {
            let extra_chunks = crate::agent::context_contract::retrieve_turn_extra_chunks(
                &config.workspace_dir,
                mem.as_ref(),
                &msg,
                Some(session_id.as_str()),
            )
            .await;
            crate::agent::context_orch::prepare_turn_history(
                &mut history,
                crate::agent::context_orch::PrepareHistoryOpts {
                    layered: config.agent.envelope_assemble,
                    compact_context: config.agent.compact_context,
                    async_pool: config.agent.envelope_assemble_async,
                    max_history: config.agent.max_history_messages,
                    extra_chunks: &extra_chunks,
                    context_window: crate::protocol_registry::lookup_context_window(&model_name),
                    summarizer: Some(&summarizer),
                },
            )
            .await?;
        }

        #[cfg(feature = "ai-protocol")]
        let hop = if config.agent.bounded_dag_live {
            crate::agent::bounded_dag_live::live_first_hop(
                &config.agent,
                mem.as_ref(),
                session_id.as_str(),
                provider.as_ref(),
                &model_name,
                &msg,
                &history,
                temperature,
                host_phase,
            )
            .await?
        } else {
            crate::agent::bounded_dag_live::LiveFirstHop::SingleWork
        };

        let turn_model = resolve_cli_turn_model(
            &config,
            &msg,
            session_id.as_str(),
            &model_name,
            if cli_explicit_flags {
                Some(model_name.as_str())
            } else {
                None
            },
            &available_hints,
        )?;

        let progress_obs =
            crate::agent::turn_progress::ProgressObserver::cli(Arc::clone(&observer));
        let response = {
            #[cfg(feature = "ai-protocol")]
            {
                match hop {
                    crate::agent::bounded_dag_live::LiveFirstHop::Plan(planned) => {
                        if host_phase == crate::agent::host_phase::HostPhase::Plan {
                            planned.preview_with_contact(&model_name, &available_hints)
                        } else {
                            let dag = &planned.dag;
                            let order = &planned.order;
                            let node_count = order.len();
                            let outline = planned.brief_outline(&msg);
                            let graph_task = match &planned.graph_task_override {
                                Some(task) => task.clone(),
                                None => {
                                    crate::agent::bounded_dag_live::work_node_user_task(
                                        mem.as_ref(),
                                        session_id.as_str(),
                                        &history,
                                        &msg,
                                    )
                                    .await
                                }
                            };
                            let no_extra: &[ai_lib_rust::context::MessageChunk] = &[];
                            let by_id: HashMap<_, _> =
                                dag.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
                            let mut last_body = String::new();
                            let mut operator_prefix = String::new();
                            let mut hop_probes: HashMap<
                                String,
                                Arc<Mutex<crate::agent::probe_dedup::HopProbeGovernor>>,
                            > = HashMap::new();
                            let mut prior: Vec<String> = Vec::new();
                            for id in order.iter().take(planned.resume_from) {
                                prior.push(id.clone());
                            }
                            let mut work_sys = crate::channels::build_work_node_system_prompt(
                                &config.workspace_dir,
                                &model_name,
                                &tool_descs,
                                &skills,
                                Some(&config.identity),
                                native_tools,
                            );
                            #[cfg(feature = "ai-protocol")]
                            if let Some(ref dispatcher) = tool_dispatcher {
                                let strategy = if text_tool_result_history {
                                    ai_lib_rust::NativeStrategy::Hybrid
                                } else if dispatcher.should_send_tool_specs() {
                                    ai_lib_rust::NativeStrategy::Full
                                } else {
                                    ai_lib_rust::NativeStrategy::TextOnly
                                };
                                append_text_tool_prompt(
                                    &mut work_sys,
                                    dispatcher.as_ref(),
                                    &tools_registry,
                                    strategy,
                                );
                            }
                            let mut force_default = false;
                            let mut auto_used = false;
                            let mut index = planned.resume_from;
                            crate::agent::bounded_dag_delivery::print_operator_note(
                                &mut operator_prefix,
                                &crate::agent::bounded_dag_live::operator_plan_gist(
                                    &msg,
                                    dag,
                                    order,
                                    planned.used_fallback,
                                ),
                                None,
                            );
                            while index < order.len() {
                                let id = &order[index];
                                let node = by_id.get(id.as_str()).ok_or_else(|| {
                                    anyhow::anyhow!("bounded DAG missing node {id}")
                                })?;
                                let mut retrieve =
                                    crate::agent::bounded_dag_context::node_retrieve_texts(
                                        mem.as_ref(),
                                        &config.workspace_dir,
                                        session_id.as_str(),
                                        node,
                                        &prior,
                                    )
                                    .await;
                                let _ = crate::agent::bounded_dag_context::ensure_graph_scratch(
                                    &config.workspace_dir,
                                    session_id.as_str(),
                                    dag.id.as_str(),
                                );
                                security.set_graph_scratch_rel(Some(
                                    crate::agent::bounded_dag_context::graph_scratch_rel(
                                        session_id.as_str(),
                                        dag.id.as_str(),
                                    ),
                                ));
                                if let Some(listing) =
                                    crate::agent::bounded_dag_context::scratch_retrieve_text(
                                        &config.workspace_dir,
                                        session_id.as_str(),
                                        dag.id.as_str(),
                                    )
                                {
                                    retrieve.push(listing);
                                }
                                if let Ok(Some(fail)) =
                                    crate::agent::bounded_dag_live::load_dag_fail(
                                        mem.as_ref(),
                                        session_id.as_str(),
                                    )
                                    .await
                                {
                                    retrieve.push(
                                        crate::agent::bounded_dag_context::cached_fail_block(&fail),
                                    );
                                }
                                let contact =
                                    crate::agent::bounded_dag_context::contact_for_live_node(
                                        node,
                                        &model_name,
                                        &available_hints,
                                        force_default,
                                    );
                                crate::agent::bounded_dag_context::reset_chat_scope(
                                    &mut history,
                                    &crate::agent::bounded_dag_context::NodeWorkPacket {
                                        dag_id: dag.id.as_str(),
                                        node,
                                        index: index + 1,
                                        node_count,
                                        user_task: &graph_task,
                                        retrieve_texts: &retrieve,
                                    },
                                    Some(work_sys.as_str()),
                                );
                                let hop_window =
                                    crate::protocol_registry::lookup_hop_context_window(
                                        &contact.model,
                                        &config.model_routes,
                                    );
                                crate::agent::context_orch::prepare_turn_history(
                                    &mut history,
                                    crate::agent::context_orch::PrepareHistoryOpts {
                                        layered: config.agent.envelope_assemble,
                                        compact_context: config.agent.compact_context,
                                        async_pool: config.agent.envelope_assemble_async,
                                        max_history: config.agent.max_history_messages,
                                        extra_chunks: no_extra,
                                        context_window: hop_window,
                                        summarizer: Some(&summarizer),
                                    },
                                )
                                .await
                                .map_err(|err| {
                                    crate::agent::envelope_pilot::annotate_hop_budget_error(
                                        err,
                                        &contact.model,
                                        hop_window,
                                    )
                                })?;
                                let node_provider =
                                    crate::protocol_registry::provider_id_from_logical(
                                        &contact.model,
                                    );
                                let probe_cell = hop_probes
                                    .entry(node.id.clone())
                                    .or_insert_with(|| {
                                        Arc::new(Mutex::new(
                                            crate::agent::probe_dedup::HopProbeGovernor::new(),
                                        ))
                                    })
                                    .clone();
                                let piece = match run_tool_call_loop(
                                    provider.as_ref(),
                                    &mut history,
                                    &tools_registry,
                                    &progress_obs,
                                    node_provider,
                                    &contact.model,
                                    temperature,
                                    false,
                                    Some(&approval_manager),
                                    approval_channel,
                                    &config.multimodal,
                                    config.agent.max_tool_iterations,
                                    None,
                                    None,
                                    tool_dispatcher_ref,
                                    Some(&security),
                                    None,
                                    text_tool_result_history,
                                    render_opts,
                                    None,
                                    Some(SoftFailLoopCtx {
                                        session_key: session_id.as_str(),
                                        config: Some(&config),
                                        host_decide: None,
                                        surface: velaclaw_agent_runtime::SoftFailSurface::Cli,
                                        peer_logical_ids: &catalog_peers,
                                        model_routes: &config.model_routes,
                                        session_model: Some(model_name.as_str()),
                                        probe: Some(probe_cell.as_ref()),
                                    }),
                                    Some(&cli_gate_extras),
                                )
                                .await
                                {
                                    Ok(piece) => {
                                        let (notes, close) = {
                                            let mut g = probe_cell
                                                .lock()
                                                .unwrap_or_else(|e| e.into_inner());
                                            (g.drain_notices(), g.hop_close())
                                        };
                                        for note in notes {
                                            crate::agent::bounded_dag_delivery::print_operator_note(
                                                &mut operator_prefix,
                                                &note,
                                                None,
                                            );
                                        }
                                        if close == crate::agent::hop_stop::HopClose::PolicyDeny {
                                            let _ = crate::agent::bounded_dag_live::store_dag_fail(
                                                mem.as_ref(),
                                                session_id.as_str(),
                                                &crate::agent::bounded_dag_live::policy_deny_fail_cursor(
                                                    &node.id, index, dag.id.as_str(),
                                                ),
                                            )
                                            .await;
                                            security.set_graph_scratch_rel(None);
                                            crate::agent::bounded_dag_delivery::print_operator_note(
                                                &mut operator_prefix,
                                                &crate::agent::bounded_dag_live::format_work_node_stop(
                                                    &msg,
                                                    &node.id,
                                                    "repeated policy denials of the same class",
                                                    index + 1,
                                                    node_count,
                                                ),
                                                None,
                                            );
                                            return Ok(operator_prefix);
                                        }
                                        piece
                                    }
                                    Err(err) if is_tool_loop_cancelled(&err) => {
                                        let _ = crate::agent::bounded_dag_live::store_dag_fail(
                                            mem.as_ref(),
                                            session_id.as_str(),
                                            &crate::agent::bounded_dag_live::cancelled_fail_cursor(
                                                &node.id,
                                                index,
                                                dag.id.as_str(),
                                            ),
                                        )
                                        .await;
                                        security.set_graph_scratch_rel(None);
                                        return Err(err);
                                    }
                                    Err(err) => {
                                        let err_s = format!("{err:#}");
                                        let class =
                                            crate::providers::hint_peer::classify_hop_error(&err_s);
                                        match crate::agent::bounded_dag_live::decide_work_node_fail(
                                        config.agent.dag_fail_auto_replan,
                                        auto_used,
                                        &err_s,
                                    ) {
                                        crate::agent::bounded_dag_live::WorkNodeFailDecision::RetrySame {
                                            force_default: fd,
                                        } => {
                                            auto_used = true;
                                            force_default = force_default || fd;
                                            let _ = crate::agent::bounded_dag_live::store_dag_fail(
                                                mem.as_ref(),
                                                session_id.as_str(),
                                                &crate::agent::bounded_dag_live::DagFailCursor {
                                                    node_id: node.id.clone(),
                                                    index,
                                                    err: err_s,
                                                    dag_id: dag.id.clone(),
                                                    auto_replan_count: 1,
                                                    fail_class: class.as_str().into(),
                                                },
                                            )
                                            .await;
                                            continue;
                                        }
                                        crate::agent::bounded_dag_live::WorkNodeFailDecision::Stop => {
                                            let _ = crate::agent::bounded_dag_live::store_dag_fail(
                                                mem.as_ref(),
                                                session_id.as_str(),
                                                &crate::agent::bounded_dag_live::DagFailCursor {
                                                    node_id: node.id.clone(),
                                                    index,
                                                    err: err_s.clone(),
                                                    dag_id: dag.id.clone(),
                                                    auto_replan_count: u32::from(auto_used),
                                                    fail_class: class.as_str().into(),
                                                },
                                            )
                                            .await;
                                            security.set_graph_scratch_rel(None);
                                            return Ok({
                                                crate::agent::bounded_dag_delivery::print_operator_note(
                                                    &mut operator_prefix,
                                                    &crate::agent::bounded_dag_live::format_work_node_stop(
                                                        &msg,
                                                        &node.id,
                                                        &err_s,
                                                        index + 1,
                                                        node_count,
                                                    ),
                                                    None,
                                                );
                                                operator_prefix
                                            });
                                        }
                                    }
                                    }
                                };
                                let _ = crate::agent::bounded_dag_context::store_node_artifact(
                                    mem.as_ref(),
                                    session_id.as_str(),
                                    &node.id,
                                    &piece,
                                )
                                .await;
                                last_body = piece;
                                prior.push(node.id.clone());
                                let remaining = order.len().saturating_sub(index + 1);
                                if crate::agent::bounded_dag_delivery::should_emit_mid_hop_note(
                                    remaining,
                                ) {
                                    crate::agent::bounded_dag_delivery::print_operator_note(
                                        &mut operator_prefix,
                                        &crate::agent::bounded_dag_delivery::mid_hop_operator_note(
                                            &graph_task,
                                            id,
                                            &last_body,
                                            None,
                                        ),
                                        None,
                                    );
                                }
                                index += 1;
                                if crate::agent::bounded_dag_delivery::hop_body_closes_graph(
                                    &last_body,
                                ) {
                                    break;
                                }
                            }
                            let _ = crate::agent::bounded_dag_live::clear_dag_fail(
                                mem.as_ref(),
                                session_id.as_str(),
                            )
                            .await;
                            security.set_graph_scratch_rel(None);
                            let raw = if last_body.is_empty() {
                                outline
                            } else {
                                last_body
                            };
                            let prior =
                                crate::agent::bounded_dag_delivery::collect_prior_exclusivity(
                                    std::iter::once(operator_prefix.as_str()).chain(
                                        history
                                            .iter()
                                            .filter(|m| m.role == "assistant")
                                            .map(|m| m.content.as_str()),
                                    ),
                                );
                            crate::agent::bounded_dag_delivery::host_delivery(
                                provider.as_ref(),
                                &model_name,
                                temperature,
                                &graph_task,
                                &raw,
                                &prior,
                            )
                            .await?
                        }
                    }
                    crate::agent::bounded_dag_live::LiveFirstHop::ChatOnly { reply } => {
                        history.push(ChatMessage::assistant(&reply));
                        reply
                    }
                    crate::agent::bounded_dag_live::LiveFirstHop::SingleWork => {
                        #[cfg(feature = "ai-protocol")]
                        let tools_for_turn = tools_registry.as_slice();
                        #[cfg(not(feature = "ai-protocol"))]
                        let tools_for_turn = tools_registry.as_slice();
                        #[cfg(feature = "ai-protocol")]
                        let hop_model = if config.agent.bounded_dag_live {
                            model_name.as_str()
                        } else {
                            turn_model.as_str()
                        };
                        #[cfg(not(feature = "ai-protocol"))]
                        let hop_model = turn_model.as_str();
                        run_tool_call_loop(
                            provider.as_ref(),
                            &mut history,
                            tools_for_turn,
                            &progress_obs,
                            provider_name,
                            hop_model,
                            temperature,
                            false,
                            Some(&approval_manager),
                            approval_channel,
                            &config.multimodal,
                            config.agent.max_tool_iterations,
                            None,
                            None,
                            tool_dispatcher_ref,
                            Some(&security),
                            None,
                            text_tool_result_history,
                            render_opts,
                            None,
                            Some(SoftFailLoopCtx {
                                session_key: session_id.as_str(),
                                config: Some(&config),
                                host_decide: None,
                                surface: velaclaw_agent_runtime::SoftFailSurface::Cli,
                                peer_logical_ids: &catalog_peers,
                                model_routes: &config.model_routes,
                                session_model: Some(turn_model.as_str()),
                                probe: None,
                            }),
                            Some(&cli_gate_extras),
                        )
                        .await?
                    }
                }
            }
            #[cfg(not(feature = "ai-protocol"))]
            {
                run_tool_call_loop(
                    provider.as_ref(),
                    &mut history,
                    &tools_registry,
                    &progress_obs,
                    provider_name,
                    &turn_model,
                    temperature,
                    false,
                    Some(&approval_manager),
                    approval_channel,
                    &config.multimodal,
                    config.agent.max_tool_iterations,
                    None,
                    None,
                    tool_dispatcher_ref,
                    Some(&security),
                    None,
                    text_tool_result_history,
                    render_opts,
                    None,
                    Some(SoftFailLoopCtx {
                        session_key: session_id.as_str(),
                        config: Some(&config),
                        host_decide: None,
                        surface: velaclaw_agent_runtime::SoftFailSurface::Cli,
                        peer_logical_ids: &catalog_peers,
                        model_routes: &config.model_routes,
                        session_model: Some(turn_model.as_str()),
                        probe: None,
                    }),
                    Some(&cli_gate_extras),
                )
                .await?
            }
        };
        final_output = response.clone();
        let rendered = render_opts.render(&response);
        println!("{}", prefix_agent_lines(&rendered, render_opts.style));
        if persist_chat_session && !chat_store_id.is_empty() {
            let _ = crate::agent::session_resume::append_user_assistant_turn(
                &config.workspace_dir,
                &chat_store_id,
                &msg,
                &response,
                Some(turn_model.as_str()),
            )
            .await;
            eprintln!("session-id: {chat_store_id} (reuse with --session-id)");
        }
        observer.record_event(&ObserverEvent::TurnComplete);
    } else {
        println!("🦀 VelaClaw Interactive Mode");
        println!("Type /help for commands. During a turn, press Esc twice to stop.\n");
        let cli = crate::channels::CliChannel::with_render_opts(render_opts);

        // Persistent conversation history across turns
        let mut history = vec![ChatMessage::system(&system_prompt)];
        let mut session_model = model_name.clone();
        let session_provider = provider_name.to_string();
        let (chat_store_id, prior_chat) = if persist_chat_session {
            crate::agent::session_resume::load_or_create_session(
                &config.workspace_dir,
                chat_session_id.as_deref(),
            )
            .await?
        } else {
            (String::new(), Vec::new())
        };
        history.extend(prior_chat);
        let mut session_id = if persist_chat_session && !chat_store_id.is_empty() {
            chat_store_id.clone()
        } else {
            memory::new_session_id()
        };
        if persist_chat_session && !chat_store_id.is_empty() {
            eprintln!("session-id: {chat_store_id} (reuse with --session-id)");
        }
        let mut session_explicit = cli_explicit_flags;

        loop {
            print!("{}", format_user_prompt(render_opts.style));
            let _ = std::io::stdout().flush();

            let mut input = String::new();
            match std::io::stdin().read_line(&mut input) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => {
                    eprintln!("\nError reading input: {e}\n");
                    break;
                }
            }

            let user_input = input.trim().to_string();
            if user_input.is_empty() {
                continue;
            }
            match user_input.as_str() {
                "/quit" | "/exit" => break,
                "/help" => {
                    println!("Available commands:");
                    println!("  /help        Show this help message");
                    println!("  /version     Show VelaClaw version");
                    println!("  /models      List providers (or `/models <provider>`)");
                    println!("  /model       Show/set model for this session");
                    println!("  /expand <id> Replay a folded long output by id");
                    println!("  /clear /new  Start a new session (clear this session's memory)");
                    println!("  /quit /exit  Exit interactive mode");
                    println!("  Esc Esc      Stop the current turn (TTY)\n");
                    continue;
                }
                "/version" => {
                    println!(
                        "VelaClaw {} (provider: {}, model: {})\n",
                        env!("CARGO_PKG_VERSION"),
                        session_provider,
                        session_model
                    );
                    continue;
                }
                cmd if cmd.starts_with("/expand") => {
                    let id_str = cmd.strip_prefix("/expand").unwrap_or("").trim();
                    if id_str.is_empty() {
                        println!("Usage: /expand <id>\n");
                        continue;
                    }
                    match id_str.parse::<u64>() {
                        Ok(id) => {
                            let payload = get_fold_payload(&fold_cache, id);
                            match payload {
                                Some(text) => {
                                    // Replay raw stored payload without re-rendering.
                                    println!("{text}\n");
                                }
                                None => {
                                    println!("No folded output with id {id}.\n");
                                }
                            }
                        }
                        Err(_) => {
                            println!("Usage: /expand <id>  (id must be a number)\n");
                        }
                    }
                    continue;
                }
                "/clear" | "/new" => {
                    println!(
                        "This will clear the current conversation and delete this session's memory."
                    );
                    println!("Core memories (long-term facts/preferences) will be preserved.");
                    print!("Continue? [y/N] ");
                    let _ = std::io::stdout().flush();

                    let mut confirm = String::new();
                    if std::io::stdin().read_line(&mut confirm).is_err() {
                        continue;
                    }
                    if !matches!(confirm.trim().to_lowercase().as_str(), "y" | "yes") {
                        println!("Cancelled.\n");
                        continue;
                    }

                    history.clear();
                    history.push(ChatMessage::system(&system_prompt));
                    // Clear Conversation/Daily for the *current* session only.
                    let mut cleared = 0;
                    for category in [MemoryCategory::Conversation, MemoryCategory::Daily] {
                        let entries = mem
                            .list(Some(&category), Some(session_id.as_str()))
                            .await
                            .unwrap_or_default();
                        for entry in entries {
                            if mem.forget(&entry.key).await.unwrap_or(false) {
                                cleared += 1;
                            }
                        }
                    }
                    session_id = memory::new_session_id();
                    if cleared > 0 {
                        println!(
                            "Conversation cleared ({cleared} memory entries removed); new session started.\n"
                        );
                    } else {
                        println!("Conversation cleared; new session started.\n");
                    }
                    continue;
                }
                _ => {}
            }

            if let Some((response, new_model)) = crate::channels::handle_cli_runtime_slash_command(
                &user_input,
                &config,
                &session_provider,
                &session_model,
            ) {
                println!("{response}\n");
                if let Some(model) = new_model {
                    session_model = model;
                    session_explicit = true;
                }
                continue;
            }

            // Auto-save conversation turns (skip short/trivial messages)
            if config.memory.auto_save && user_input.chars().count() >= AUTOSAVE_MIN_MESSAGE_CHARS {
                let user_key = autosave_memory_key("user_msg");
                let _ = mem
                    .store(
                        &user_key,
                        &user_input,
                        MemoryCategory::Conversation,
                        Some(session_id.as_str()),
                    )
                    .await;
            }

            // Inject memory + hardware RAG context into user message
            let mem_context = if config.agent.envelope_assemble {
                String::new()
            } else {
                build_context(
                    mem.as_ref(),
                    &user_input,
                    config.memory.min_relevance_score,
                    Some(session_id.as_str()),
                )
                .await
            };
            let rag_limit = if config.agent.compact_context { 2 } else { 5 };
            let hw_context = hardware_rag
                .as_ref()
                .map(|r| build_hardware_context(r, &user_input, &board_names, rag_limit))
                .unwrap_or_default();
            let context = format!("{mem_context}{hw_context}");
            let enriched = if context.is_empty() {
                user_input.clone()
            } else {
                format!("{context}{user_input}")
            };

            history.push(ChatMessage::user(&enriched));

            let summarizer = crate::agent::context_orch::HistorySummarizer {
                provider: provider.as_ref(),
                model: &session_model,
            };
            #[cfg(feature = "ai-protocol")]
            let skip_session_prepare =
                crate::agent::bounded_dag_live::skip_session_prepare_for_live(
                    config.agent.bounded_dag_live,
                    host_phase,
                );
            #[cfg(not(feature = "ai-protocol"))]
            let skip_session_prepare = false;
            let prepare_report = if skip_session_prepare {
                crate::agent::context_orch::PrepareHistoryReport::default()
            } else {
                let extra_chunks = crate::agent::context_contract::retrieve_turn_extra_chunks(
                    &config.workspace_dir,
                    mem.as_ref(),
                    &user_input,
                    Some(session_id.as_str()),
                )
                .await;
                crate::agent::context_orch::prepare_turn_history(
                    &mut history,
                    crate::agent::context_orch::PrepareHistoryOpts {
                        layered: config.agent.envelope_assemble,
                        compact_context: config.agent.compact_context,
                        async_pool: config.agent.envelope_assemble_async,
                        max_history: config.agent.max_history_messages,
                        extra_chunks: &extra_chunks,
                        context_window: crate::protocol_registry::lookup_context_window(
                            &session_model,
                        ),
                        summarizer: Some(&summarizer),
                    },
                )
                .await?
            };

            #[cfg(feature = "ai-protocol")]
            let hop = if config.agent.bounded_dag_live {
                crate::agent::bounded_dag_live::live_first_hop(
                    &config.agent,
                    mem.as_ref(),
                    session_id.as_str(),
                    provider.as_ref(),
                    &session_model,
                    &user_input,
                    &history,
                    temperature,
                    host_phase,
                )
                .await?
            } else {
                crate::agent::bounded_dag_live::LiveFirstHop::SingleWork
            };
            if prepare_report.compacted {
                println!("🧹 Auto-compaction complete");
            }

            let turn_model = resolve_cli_turn_model(
                &config,
                &user_input,
                session_id.as_str(),
                &session_model,
                if session_explicit {
                    Some(session_model.as_str())
                } else {
                    None
                },
                &available_hints,
            )?;

            let cancel = crate::agent::turn_cancel::CliTurnCancel::begin();
            let progress_obs = crate::agent::turn_progress::ProgressObserver::cli_with_fold(
                Arc::clone(&observer),
                Arc::clone(&fold_cache),
            );

            let loop_result = {
                #[cfg(feature = "ai-protocol")]
                {
                    async {
                        match hop {
                            crate::agent::bounded_dag_live::LiveFirstHop::Plan(planned) => {
                            if host_phase == crate::agent::host_phase::HostPhase::Plan {
                                return Ok(
                                    planned.preview_with_contact(&session_model, &available_hints)
                                );
                            }
                            let dag = &planned.dag;
                            let order = &planned.order;
                            let node_count = order.len();
                            let outline = planned.brief_outline(&user_input);
                            let graph_task = match &planned.graph_task_override {
                                Some(task) => task.clone(),
                                None => {
                                    crate::agent::bounded_dag_live::work_node_user_task(
                                        mem.as_ref(),
                                        session_id.as_str(),
                                        &history,
                                        &user_input,
                                    )
                                    .await
                                }
                            };
                            let no_extra: &[ai_lib_rust::context::MessageChunk] = &[];
                            let by_id: HashMap<_, _> =
                                dag.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
                            let mut last_body = String::new();
                            let mut operator_prefix = String::new();
                            let mut hop_probes: HashMap<
                                String,
                                Arc<Mutex<crate::agent::probe_dedup::HopProbeGovernor>>,
                            > = HashMap::new();
                            let mut prior: Vec<String> = Vec::new();
                            let mut completed: HashSet<String> = HashSet::new();
                            for id in order.iter().take(planned.resume_from) {
                                prior.push(id.clone());
                                completed.insert(id.clone());
                            }
                            let mut work_sys = crate::channels::build_work_node_system_prompt(
                                &config.workspace_dir,
                                &session_model,
                                &tool_descs,
                                &skills,
                                Some(&config.identity),
                                native_tools,
                            );
                            #[cfg(feature = "ai-protocol")]
                            if let Some(ref dispatcher) = tool_dispatcher {
                                let strategy = if text_tool_result_history {
                                    ai_lib_rust::NativeStrategy::Hybrid
                                } else if dispatcher.should_send_tool_specs() {
                                    ai_lib_rust::NativeStrategy::Full
                                } else {
                                    ai_lib_rust::NativeStrategy::TextOnly
                                };
                                append_text_tool_prompt(
                                    &mut work_sys,
                                    dispatcher.as_ref(),
                                    &tools_registry,
                                    strategy,
                                );
                            }
                            let contacts = crate::agent::bounded_dag_live::dag_contact_labels(
                                provider.as_ref(),
                                dag,
                                order,
                                &session_model,
                                &available_hints,
                            );
                            crate::agent::turn_progress::print_cli_progress(
                                &crate::agent::bounded_dag_live::live_dag_progress(
                                    dag.id.as_str(),
                                    planned.used_fallback,
                                    &outline,
                                    dag,
                                    order,
                                    None,
                                    &completed,
                                    None,
                                    Some(&contacts),
                                ),
                                Some(&fold_cache),
                            );
                            crate::agent::bounded_dag_delivery::print_operator_note(
                                &mut operator_prefix,
                                &crate::agent::bounded_dag_live::operator_plan_gist(
                                    &user_input,
                                    dag,
                                    order,
                                    planned.used_fallback,
                                ),
                                Some(&fold_cache),
                            );
                            let mut force_default = false;
                            let mut auto_used = false;
                            let mut index = planned.resume_from;
                            while index < order.len() {
                                let id = &order[index];
                                let node = by_id.get(id.as_str()).ok_or_else(|| {
                                    anyhow::anyhow!("bounded DAG missing node {id}")
                                })?;
                                let mut retrieve =
                                    crate::agent::bounded_dag_context::node_retrieve_texts(
                                        mem.as_ref(),
                                        &config.workspace_dir,
                                        session_id.as_str(),
                                        node,
                                        &prior,
                                    )
                                    .await;
                                let _ = crate::agent::bounded_dag_context::ensure_graph_scratch(
                                    &config.workspace_dir,
                                    session_id.as_str(),
                                    dag.id.as_str(),
                                );
                                security.set_graph_scratch_rel(Some(
                                    crate::agent::bounded_dag_context::graph_scratch_rel(
                                        session_id.as_str(),
                                        dag.id.as_str(),
                                    ),
                                ));
                                if let Some(listing) =
                                    crate::agent::bounded_dag_context::scratch_retrieve_text(
                                        &config.workspace_dir,
                                        session_id.as_str(),
                                        dag.id.as_str(),
                                    )
                                {
                                    retrieve.push(listing);
                                }
                                if let Ok(Some(fail)) =
                                    crate::agent::bounded_dag_live::load_dag_fail(
                                        mem.as_ref(),
                                        session_id.as_str(),
                                    )
                                    .await
                                {
                                    retrieve.push(
                                        crate::agent::bounded_dag_context::cached_fail_block(&fail),
                                    );
                                }
                                let contact = crate::agent::bounded_dag_context::contact_for_live_node(
                                    node,
                                    &session_model,
                                    &available_hints,
                                    force_default,
                                );
                                crate::agent::bounded_dag_context::reset_chat_scope(
                                    &mut history,
                                    &crate::agent::bounded_dag_context::NodeWorkPacket {
                                        dag_id: dag.id.as_str(),
                                        node,
                                        index: index + 1,
                                        node_count,
                                        user_task: &graph_task,
                                        retrieve_texts: &retrieve,
                                    },
                                    Some(work_sys.as_str()),
                                );
                                let hop_window =
                                    crate::protocol_registry::lookup_hop_context_window(
                                        &contact.model,
                                        &config.model_routes,
                                    );
                                crate::agent::context_orch::prepare_turn_history(
                                    &mut history,
                                    crate::agent::context_orch::PrepareHistoryOpts {
                                        layered: config.agent.envelope_assemble,
                                        compact_context: config.agent.compact_context,
                                        async_pool: config.agent.envelope_assemble_async,
                                        max_history: config.agent.max_history_messages,
                                        extra_chunks: no_extra,
                                        context_window: hop_window,
                                        summarizer: Some(&summarizer),
                                    },
                                )
                                .await
                                .map_err(|err| {
                                    crate::agent::envelope_pilot::annotate_hop_budget_error(
                                        err,
                                        &contact.model,
                                        hop_window,
                                    )
                                })?;
                                let node_provider =
                                    crate::protocol_registry::provider_id_from_logical(
                                        &contact.model,
                                    );
                                let contacts = crate::agent::bounded_dag_live::dag_contact_labels(
                                    provider.as_ref(),
                                    dag,
                                    order,
                                    &session_model,
                                    &available_hints,
                                );
                                crate::agent::turn_progress::print_cli_progress(
                                    &crate::agent::bounded_dag_live::live_dag_progress(
                                        dag.id.as_str(),
                                        planned.used_fallback,
                                        &outline,
                                        dag,
                                        order,
                                        Some(id.as_str()),
                                        &completed,
                                        None,
                                        Some(&contacts),
                                    ),
                                    Some(&fold_cache),
                                );
                                let probe_cell = hop_probes
                                    .entry(node.id.clone())
                                    .or_insert_with(|| {
                                        Arc::new(Mutex::new(
                                            crate::agent::probe_dedup::HopProbeGovernor::new(),
                                        ))
                                    })
                                    .clone();
                                let piece = match run_tool_call_loop(
                                    provider.as_ref(),
                                    &mut history,
                                    &tools_registry,
                                    &progress_obs,
                                    node_provider,
                                    &contact.model,
                                    temperature,
                                    false,
                                    Some(&approval_manager),
                                    approval_channel,
                                    &config.multimodal,
                                    config.agent.max_tool_iterations,
                                    Some(cancel.token()),
                                    None,
                                    tool_dispatcher_ref,
                                    Some(&security),
                                    None,
                                    text_tool_result_history,
                                    render_opts,
                                    Some(&fold_cache),
                                    Some(SoftFailLoopCtx {
                                        session_key: session_id.as_str(),
                                        config: Some(&config),
                                        host_decide: None,
                                        surface: velaclaw_agent_runtime::SoftFailSurface::Cli,
                                        peer_logical_ids: &catalog_peers,
                                        model_routes: &config.model_routes,
                                        session_model: Some(session_model.as_str()),
                                        probe: Some(probe_cell.as_ref()),
                                    }),
                                    Some(&cli_gate_extras),
                                )
                                .await
                                {
                                    Ok(piece) => {
                                        let (notes, close) = {
                                            let mut g = probe_cell
                                                .lock()
                                                .unwrap_or_else(|e| e.into_inner());
                                            (g.drain_notices(), g.hop_close())
                                        };
                                        for note in notes {
                                            crate::agent::bounded_dag_delivery::print_operator_note(
                                                &mut operator_prefix,
                                                &note,
                                                None,
                                            );
                                        }
                                        if close == crate::agent::hop_stop::HopClose::PolicyDeny {
                                            let _ = crate::agent::bounded_dag_live::store_dag_fail(
                                                mem.as_ref(),
                                                session_id.as_str(),
                                                &crate::agent::bounded_dag_live::policy_deny_fail_cursor(
                                                    &node.id, index, dag.id.as_str(),
                                                ),
                                            )
                                            .await;
                                            security.set_graph_scratch_rel(None);
                                            crate::agent::bounded_dag_delivery::print_operator_note(
                                                &mut operator_prefix,
                                                &crate::agent::bounded_dag_live::format_work_node_stop(
                                                    &user_input,
                                                    &node.id,
                                                    "repeated policy denials of the same class",
                                                    index + 1,
                                                    node_count,
                                                ),
                                                Some(&fold_cache),
                                            );
                                            return Ok(operator_prefix);
                                        }
                                        piece
                                    }
                                    Err(err) if is_tool_loop_cancelled(&err) => {
                                        let _ = crate::agent::bounded_dag_live::store_dag_fail(
                                            mem.as_ref(),
                                            session_id.as_str(),
                                            &crate::agent::bounded_dag_live::cancelled_fail_cursor(
                                                &node.id, index, dag.id.as_str(),
                                            ),
                                        )
                                        .await;
                                        security.set_graph_scratch_rel(None);
                                        return Err(err);
                                    }
                                    Err(err) => {
                                        let err_s = format!("{err:#}");
                                        let class =
                                            crate::providers::hint_peer::classify_hop_error(&err_s);
                                        match crate::agent::bounded_dag_live::decide_work_node_fail(
                                            config.agent.dag_fail_auto_replan,
                                            auto_used,
                                            &err_s,
                                        ) {
                                            crate::agent::bounded_dag_live::WorkNodeFailDecision::RetrySame {
                                                force_default: fd,
                                            } => {
                                                auto_used = true;
                                                force_default = force_default || fd;
                                                let _ = crate::agent::bounded_dag_live::store_dag_fail(
                                                    mem.as_ref(),
                                                    session_id.as_str(),
                                                    &crate::agent::bounded_dag_live::DagFailCursor {
                                                        node_id: node.id.clone(),
                                                        index,
                                                        err: err_s,
                                                        dag_id: dag.id.clone(),
                                                        auto_replan_count: 1,
                                                        fail_class: class.as_str().into(),
                                                    },
                                                )
                                                .await;
                                                continue;
                                            }
                                            crate::agent::bounded_dag_live::WorkNodeFailDecision::Stop => {
                                        let contacts =
                                            crate::agent::bounded_dag_live::dag_contact_labels(
                                                provider.as_ref(),
                                                dag,
                                                order,
                                                &session_model,
                                                &available_hints,
                                            );
                                        crate::agent::turn_progress::print_cli_progress(
                                            &crate::agent::bounded_dag_live::live_dag_progress(
                                                dag.id.as_str(),
                                                planned.used_fallback,
                                                &outline,
                                                dag,
                                                order,
                                                None,
                                                &completed,
                                                Some(id.as_str()),
                                                Some(&contacts),
                                            ),
                                            Some(&fold_cache),
                                        );
                                        let _ = crate::agent::bounded_dag_live::store_dag_fail(
                                            mem.as_ref(),
                                            session_id.as_str(),
                                            &crate::agent::bounded_dag_live::DagFailCursor {
                                                node_id: node.id.clone(),
                                                index,
                                                err: err_s.clone(),
                                                dag_id: dag.id.clone(),
                                                auto_replan_count: u32::from(auto_used),
                                                fail_class: class.as_str().into(),
                                            },
                                        )
                                        .await;
                                        crate::agent::bounded_dag_delivery::print_operator_note(
                                            &mut operator_prefix,
                                            &crate::agent::bounded_dag_live::format_work_node_stop(
                                                &user_input,
                                                &node.id,
                                                &err_s,
                                                index + 1,
                                                node_count,
                                            ),
                                            Some(&fold_cache),
                                        );
                                        security.set_graph_scratch_rel(None);
                                        return Ok(operator_prefix);
                                            }
                                        }
                                    }
                                };
                                let _ = crate::agent::bounded_dag_context::store_node_artifact(
                                    mem.as_ref(),
                                    session_id.as_str(),
                                    &node.id,
                                    &piece,
                                )
                                .await;
                                completed.insert(node.id.clone());
                                last_body = piece;
                                prior.push(node.id.clone());
                                let contacts = crate::agent::bounded_dag_live::dag_contact_labels(
                                    provider.as_ref(),
                                    dag,
                                    order,
                                    &session_model,
                                    &available_hints,
                                );
                                crate::agent::turn_progress::print_cli_progress(
                                    &crate::agent::bounded_dag_live::live_dag_progress(
                                        dag.id.as_str(),
                                        planned.used_fallback,
                                        &outline,
                                        dag,
                                        order,
                                        None,
                                        &completed,
                                        None,
                                        Some(&contacts),
                                    ),
                                    Some(&fold_cache),
                                );
                                let remaining = order.len().saturating_sub(index + 1);
                                if crate::agent::bounded_dag_delivery::should_emit_mid_hop_note(
                                    remaining,
                                ) {
                                    crate::agent::bounded_dag_delivery::print_operator_note(
                                        &mut operator_prefix,
                                        &crate::agent::bounded_dag_delivery::mid_hop_operator_note(
                                            &graph_task,
                                            id,
                                            &last_body,
                                            None,
                                        ),
                                        Some(&fold_cache),
                                    );
                                }
                                index += 1;
                                if crate::agent::bounded_dag_delivery::hop_body_closes_graph(
                                    &last_body,
                                ) {
                                    break;
                                }
                            }
                            let _ = crate::agent::bounded_dag_live::clear_dag_fail(
                                mem.as_ref(),
                                session_id.as_str(),
                            )
                            .await;
                            security.set_graph_scratch_rel(None);
                            let raw = if last_body.is_empty() {
                                outline
                            } else {
                                last_body
                            };
                            let prior = crate::agent::bounded_dag_delivery::collect_prior_exclusivity(
                                std::iter::once(operator_prefix.as_str()).chain(
                                    history
                                        .iter()
                                        .filter(|m| m.role == "assistant")
                                        .map(|m| m.content.as_str()),
                                ),
                            );
                            Ok(crate::agent::bounded_dag_delivery::host_delivery(
                                    provider.as_ref(),
                                    &session_model,
                                    temperature,
                                    &graph_task,
                                    &raw,
                                    &prior,
                                )
                                .await?)
                        }
                            crate::agent::bounded_dag_live::LiveFirstHop::ChatOnly { reply } => {
                                history.push(ChatMessage::assistant(&reply));
                                Ok(reply)
                            }
                            crate::agent::bounded_dag_live::LiveFirstHop::SingleWork => {
                            #[cfg(feature = "ai-protocol")]
                            let tools_for_turn = tools_registry.as_slice();
                            #[cfg(not(feature = "ai-protocol"))]
                            let tools_for_turn = tools_registry.as_slice();
                            #[cfg(feature = "ai-protocol")]
                            let hop_model = if config.agent.bounded_dag_live {
                                session_model.as_str()
                            } else {
                                turn_model.as_str()
                            };
                            #[cfg(not(feature = "ai-protocol"))]
                            let hop_model = turn_model.as_str();
                            run_tool_call_loop(
                                provider.as_ref(),
                                &mut history,
                                tools_for_turn,
                                &progress_obs,
                                provider_name,
                                hop_model,
                                temperature,
                                false,
                                Some(&approval_manager),
                                approval_channel,
                                &config.multimodal,
                                config.agent.max_tool_iterations,
                                Some(cancel.token()),
                                None,
                                tool_dispatcher_ref,
                                Some(&security),
                                None,
                                text_tool_result_history,
                                render_opts,
                                Some(&fold_cache),
                                Some(SoftFailLoopCtx {
                                    session_key: session_id.as_str(),
                                    config: Some(&config),
                                    host_decide: None,
                                    surface: velaclaw_agent_runtime::SoftFailSurface::Cli,
                                    peer_logical_ids: &catalog_peers,
                                    model_routes: &config.model_routes,
                                    session_model: Some(session_model.as_str()),
                                    probe: None,
                                }),
                                Some(&cli_gate_extras),
                            )
                            .await
                            }
                        }
                    }
                    .await
                }
                #[cfg(not(feature = "ai-protocol"))]
                {
                    run_tool_call_loop(
                        provider.as_ref(),
                        &mut history,
                        &tools_registry,
                        &progress_obs,
                        provider_name,
                        &turn_model,
                        temperature,
                        false,
                        Some(&approval_manager),
                        approval_channel,
                        &config.multimodal,
                        config.agent.max_tool_iterations,
                        Some(cancel.token()),
                        None,
                        tool_dispatcher_ref,
                        Some(&security),
                        None,
                        text_tool_result_history,
                        render_opts,
                        Some(&fold_cache),
                        Some(SoftFailLoopCtx {
                            session_key: session_id.as_str(),
                            config: Some(&config),
                            host_decide: None,
                            surface: velaclaw_agent_runtime::SoftFailSurface::Cli,
                            peer_logical_ids: &catalog_peers,
                            model_routes: &config.model_routes,
                            session_model: Some(session_model.as_str()),
                            probe: None,
                        }),
                        Some(&cli_gate_extras),
                    )
                    .await
                }
            };
            let response = match cancel.conclude(loop_result).await {
                crate::agent::turn_cancel::TurnFinish::Completed(resp) => resp,
                crate::agent::turn_cancel::TurnFinish::Cancelled => {
                    eprintln!("{}\n", crate::agent::turn_cancel::STOPPED_USER_MESSAGE);
                    if persist_chat_session && !chat_store_id.is_empty() {
                        let _ = crate::agent::session_resume::append_user_assistant_turn(
                            &config.workspace_dir,
                            &chat_store_id,
                            &user_input,
                            crate::agent::turn_cancel::STOPPED_USER_MESSAGE,
                            Some(turn_model.as_str()),
                        )
                        .await;
                    }
                    continue;
                }
                crate::agent::turn_cancel::TurnFinish::Failed(e) => {
                    eprintln!("\nError: {e}\n");
                    continue;
                }
            };
            final_output = response.clone();
            let visible_response = crate::util::strip_tool_call_markup(&response);
            if let Err(e) = crate::channels::Channel::send(
                &cli,
                &crate::channels::traits::SendMessage::new(
                    format!("\n{visible_response}\n"),
                    "user",
                ),
            )
            .await
            {
                eprintln!("\nError sending CLI response: {e}\n");
            }
            observer.record_event(&ObserverEvent::TurnComplete);
            if persist_chat_session && !chat_store_id.is_empty() {
                let _ = crate::agent::session_resume::append_user_assistant_turn(
                    &config.workspace_dir,
                    &chat_store_id,
                    &user_input,
                    &response,
                    Some(turn_model.as_str()),
                )
                .await;
            }

            // Post-turn prepare: compact overflow + layered (or trim kill-switch).
            if !skip_session_prepare {
                let summarizer = crate::agent::context_orch::HistorySummarizer {
                    provider: provider.as_ref(),
                    model: &session_model,
                };
                let extra_chunks = crate::agent::context_contract::retrieve_turn_extra_chunks(
                    &config.workspace_dir,
                    mem.as_ref(),
                    &user_input,
                    Some(session_id.as_str()),
                )
                .await;
                if let Ok(report) = crate::agent::context_orch::prepare_turn_history(
                    &mut history,
                    crate::agent::context_orch::PrepareHistoryOpts {
                        layered: config.agent.envelope_assemble,
                        compact_context: config.agent.compact_context,
                        async_pool: config.agent.envelope_assemble_async,
                        max_history: config.agent.max_history_messages,
                        extra_chunks: &extra_chunks,
                        context_window: crate::protocol_registry::lookup_context_window(
                            &session_model,
                        ),
                        summarizer: Some(&summarizer),
                    },
                )
                .await
                {
                    if report.compacted {
                        println!("🧹 Auto-compaction complete");
                    }
                }
            }
        }
    }

    let duration = start.elapsed();
    observer.record_event(&ObserverEvent::AgentEnd {
        provider: provider_name.to_string(),
        model: model_name.to_string(),
        duration,
        tokens_used: None,
        cost_usd: None,
    });

    Ok(final_output)
}

/// Process a single message through the full agent (with tools, peripherals, memory).
/// Used by channels (Telegram, Discord, etc.) to enable hardware and tool use.
pub async fn process_message(config: Config, message: &str) -> Result<String> {
    let mut assembled = crate::agent::assemble::assemble_runtime(
        &config,
        crate::config::BootstrapOptions {
            with_embedding_routes: false,
        },
    )?;
    let observer = assembled.boot.observer.clone();
    let security = assembled.boot.security.clone();
    let mem = assembled.boot.memory.clone();
    let mut tools_registry = std::mem::take(&mut assembled.boot.tools);
    let peripheral_tools: Vec<Box<dyn Tool>> =
        crate::peripherals::create_peripheral_tools(&config.peripherals).await?;
    tools_registry.extend(peripheral_tools);

    let provider = assembled.provider;
    let model_name = assembled.model_name;
    let provider: Box<dyn Provider> = provider;
    let provider_name = model_name
        .split_once('/')
        .map_or(model_name.as_str(), |(provider, _)| provider);
    let available_hints: Vec<String> = config
        .model_routes
        .iter()
        .map(|route| route.hint.clone())
        .collect();

    let hardware_rag: Option<crate::rag::HardwareRag> = config
        .peripherals
        .datasheet_dir
        .as_ref()
        .filter(|d| !d.trim().is_empty())
        .map(|dir| crate::rag::HardwareRag::load(&config.workspace_dir, dir.trim()))
        .and_then(Result::ok)
        .filter(|r: &crate::rag::HardwareRag| !r.is_empty());
    let board_names: Vec<String> = config
        .peripherals
        .boards
        .iter()
        .map(|b| b.board.clone())
        .collect();

    let skills = crate::skills::load_skills_with_config(&config.workspace_dir, &config);
    let mut tool_descs: Vec<(&str, &str)> = vec![
        ("shell", "Execute terminal commands."),
        ("file_read", "Read file contents."),
        ("file_write", "Write file contents."),
        ("memory_store", "Save to memory."),
        ("memory_recall", "Search memory."),
        ("memory_forget", "Delete a memory entry."),
        ("screenshot", "Capture a screenshot."),
        ("image_info", "Read image metadata."),
    ];
    if config.browser.enabled {
        tool_descs.push(("browser_open", "Open approved URLs in browser."));
    }
    if config.composio.enabled {
        tool_descs.push(("composio", "Execute actions on 1000+ apps via Composio."));
    }
    if config.peripherals.enabled && !config.peripherals.boards.is_empty() {
        tool_descs.push(("gpio_read", "Read GPIO pin value on connected hardware."));
        tool_descs.push((
            "gpio_write",
            "Set GPIO pin high or low on connected hardware.",
        ));
        tool_descs.push((
            "arduino_upload",
            "Upload Arduino sketch. Use for 'make a heart', custom patterns. You write full .ino code; VelaClaw uploads it.",
        ));
        tool_descs.push((
            "hardware_memory_map",
            "Return flash and RAM address ranges. Use when user asks for memory addresses or memory map.",
        ));
        tool_descs.push((
            "hardware_board_info",
            "Return full board info (chip, architecture, memory map). Use when user asks for board info, what board, connected hardware, or chip info.",
        ));
        tool_descs.push((
            "hardware_memory_read",
            "Read actual memory/register values from Nucleo. Use when user asks to read registers, read memory, dump lower memory 0-126, or give address and value.",
        ));
        tool_descs.push((
            "hardware_capabilities",
            "Query connected hardware for reported GPIO pins and LED pin. Use when user asks what pins are available.",
        ));
    }
    let bootstrap_max_chars = if config.agent.compact_context {
        Some(6000)
    } else {
        None
    };
    let prompt_budget = crate::agent::prompt_composer::system_prompt_char_budget(
        config.agent.compact_context,
        &model_name,
    );
    let native_tools = provider.supports_native_tools();
    let mut system_prompt = crate::channels::build_system_prompt_pyramid(
        &config.workspace_dir,
        &model_name,
        &tool_descs,
        &skills,
        Some(&config.identity),
        bootstrap_max_chars,
        native_tools,
        config.skills.prompt_injection_mode,
        crate::agent::prompt_composer::PromptMode::Full,
        prompt_budget,
    );
    if !native_tools {
        system_prompt.push_str(&build_tool_instructions(&tools_registry));
    }
    append_execution_policy_to_prompt(&mut system_prompt, &security, &config);
    crate::agent::prompt_composer::append_phase_sections(
        &mut system_prompt,
        &[crate::agent::prompt_composer::PromptPhase::Approval],
    );

    let session_id = memory::new_session_id();
    let mem_context = build_context(
        mem.as_ref(),
        message,
        config.memory.min_relevance_score,
        Some(session_id.as_str()),
    )
    .await;
    let rag_limit = if config.agent.compact_context { 2 } else { 5 };
    let hw_context = hardware_rag
        .as_ref()
        .map(|r| build_hardware_context(r, message, &board_names, rag_limit))
        .unwrap_or_default();
    let context = format!("{mem_context}{hw_context}");
    let enriched = if context.is_empty() {
        message.to_string()
    } else {
        format!("{context}{message}")
    };

    let mut history = vec![
        ChatMessage::system(&system_prompt),
        ChatMessage::user(&enriched),
    ];

    let turn_model = crate::agent::classifier::resolve_model_for_message(
        &config.query_classification,
        &available_hints,
        &model_name,
        message,
    );

    agent_turn(
        provider.as_ref(),
        &mut history,
        &tools_registry,
        observer.as_ref(),
        provider_name,
        &turn_model,
        config.default_temperature,
        true,
        &config.multimodal,
        config.agent.max_tool_iterations,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use velaclaw_agent_runtime::loop_parse::{
        apply_compaction_summary, build_compaction_transcript, trim_history,
    };
    use velaclaw_agent_runtime::loop_parse::{
        tools_to_openai_format, DEFAULT_MAX_HISTORY_MESSAGES,
    };

    #[test]
    fn approval_channel_for_phases_maps_cron_heartbeat() {
        use crate::agent::prompt_composer::PromptPhase;
        assert_eq!(approval_channel_for_phases(&[]), "cli");
        assert_eq!(approval_channel_for_phases(&[PromptPhase::Cron]), "cron");
        assert_eq!(
            approval_channel_for_phases(&[PromptPhase::Heartbeat]),
            "heartbeat"
        );
    }

    #[test]
    fn test_scrub_credentials() {
        let input = "API_KEY=sk-1234567890abcdef; token: 1234567890; password=\"secret123456\"";
        let scrubbed = tool_batch::scrub_credentials(input);
        assert!(scrubbed.contains("API_KEY=sk-1*[REDACTED]"));
        assert!(scrubbed.contains("token: 1234*[REDACTED]"));
        assert!(scrubbed.contains("password=\"secr*[REDACTED]\""));
        assert!(!scrubbed.contains("abcdef"));
        assert!(!scrubbed.contains("secret123456"));
    }

    #[test]
    fn test_scrub_credentials_json() {
        let input = r#"{"api_key": "sk-1234567890", "other": "public"}"#;
        let scrubbed = tool_batch::scrub_credentials(input);
        assert!(scrubbed.contains("\"api_key\": \"sk-1*[REDACTED]\""));
        assert!(scrubbed.contains("public"));
    }
    use crate::memory::{Memory, MemoryCategory, SqliteMemory};
    use crate::observability::NoopObserver;
    use crate::providers::traits::ProviderCapabilities;
    use crate::providers::ChatResponse;
    use tempfile::TempDir;

    struct NonVisionProvider {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Provider for NonVisionProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok("ok".to_string())
        }
    }

    struct VisionProvider {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Provider for VisionProvider {
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                native_tool_calling: false,
                vision: true,
            }
        }

        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok("ok".to_string())
        }

        async fn chat(
            &self,
            request: ChatRequest<'_>,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<ChatResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let marker_count = crate::multimodal::count_image_markers(request.messages);
            if marker_count == 0 {
                anyhow::bail!("expected image markers in request messages");
            }

            if request.tools.is_some() {
                anyhow::bail!("no tools should be attached for this test");
            }

            Ok(ChatResponse {
                text: Some("vision-ok".to_string()),
                tool_calls: Vec::new(),
            })
        }
    }

    struct ScriptedProvider {
        responses: Arc<Mutex<VecDeque<ChatResponse>>>,
    }

    impl ScriptedProvider {
        fn from_text_responses(responses: Vec<&str>) -> Self {
            let scripted = responses
                .into_iter()
                .map(|text| ChatResponse {
                    text: Some(text.to_string()),
                    tool_calls: Vec::new(),
                })
                .collect();
            Self {
                responses: Arc::new(Mutex::new(scripted)),
            }
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
        ) -> anyhow::Result<String> {
            anyhow::bail!("chat_with_system should not be used in scripted provider tests");
        }

        async fn chat(
            &self,
            _request: ChatRequest<'_>,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<ChatResponse> {
            let mut responses = self
                .responses
                .lock()
                .expect("responses lock should be valid");
            responses
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("scripted provider exhausted responses"))
        }
    }

    struct DelayTool {
        name: String,
        delay_ms: u64,
        active: Arc<AtomicUsize>,
        max_active: Arc<AtomicUsize>,
    }

    impl DelayTool {
        fn new(
            name: &str,
            delay_ms: u64,
            active: Arc<AtomicUsize>,
            max_active: Arc<AtomicUsize>,
        ) -> Self {
            Self {
                name: name.to_string(),
                delay_ms,
                active,
                max_active,
            }
        }
    }

    #[async_trait]
    impl Tool for DelayTool {
        fn name(&self) -> &str {
            &self.name
        }

        fn description(&self) -> &str {
            "Delay tool for testing parallel tool execution"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "value": { "type": "string" }
                },
                "required": ["value"]
            })
        }

        async fn execute(
            &self,
            args: serde_json::Value,
            _ctx: &crate::tools::ToolExecutionContext,
        ) -> anyhow::Result<crate::tools::ToolResult> {
            let now_active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(now_active, Ordering::SeqCst);

            tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;

            self.active.fetch_sub(1, Ordering::SeqCst);

            let value = args
                .get("value")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();

            Ok(crate::tools::ToolResult {
                success: true,
                output: format!("ok:{value}"),
                error: None,
            })
        }
    }

    #[tokio::test]
    async fn run_tool_call_loop_returns_structured_error_for_non_vision_provider() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = NonVisionProvider {
            calls: Arc::clone(&calls),
        };

        let mut history = vec![ChatMessage::user(
            "please inspect [IMAGE:data:image/png;base64,iVBORw0KGgo=]".to_string(),
        )];
        let tools_registry: Vec<Box<dyn Tool>> = Vec::new();
        let observer = NoopObserver;

        let err = run_tool_call_loop(
            &provider,
            &mut history,
            &tools_registry,
            &observer,
            "mock-provider",
            "mock-model",
            0.0,
            true,
            None,
            "cli",
            &crate::config::MultimodalConfig::default(),
            3,
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
        .expect_err("provider without vision support should fail");

        assert!(err.to_string().contains("provider_capability_error"));
        assert!(err.to_string().contains("capability=vision"));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn run_tool_call_loop_rejects_oversized_image_payload() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = VisionProvider {
            calls: Arc::clone(&calls),
        };

        let oversized_payload = STANDARD.encode(vec![0_u8; (1024 * 1024) + 1]);
        let mut history = vec![ChatMessage::user(format!(
            "[IMAGE:data:image/png;base64,{oversized_payload}]"
        ))];

        let tools_registry: Vec<Box<dyn Tool>> = Vec::new();
        let observer = NoopObserver;
        let multimodal = crate::config::MultimodalConfig {
            max_images: 4,
            max_image_size_mb: 1,
            allow_remote_fetch: false,
        };

        let err = run_tool_call_loop(
            &provider,
            &mut history,
            &tools_registry,
            &observer,
            "mock-provider",
            "mock-model",
            0.0,
            true,
            None,
            "cli",
            &multimodal,
            3,
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
        .expect_err("oversized payload must fail");

        assert!(err
            .to_string()
            .contains("multimodal image size limit exceeded"));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn run_tool_call_loop_accepts_valid_multimodal_request_flow() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = VisionProvider {
            calls: Arc::clone(&calls),
        };

        let mut history = vec![ChatMessage::user(
            "Analyze this [IMAGE:data:image/png;base64,iVBORw0KGgo=]".to_string(),
        )];
        let tools_registry: Vec<Box<dyn Tool>> = Vec::new();
        let observer = NoopObserver;

        let result = run_tool_call_loop(
            &provider,
            &mut history,
            &tools_registry,
            &observer,
            "mock-provider",
            "mock-model",
            0.0,
            true,
            None,
            "cli",
            &crate::config::MultimodalConfig::default(),
            3,
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
        .expect("valid multimodal payload should pass");

        assert_eq!(result, "vision-ok");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn should_execute_tools_in_parallel_returns_false_for_single_call() {
        let calls = vec![ParsedToolCall {
            name: "file_read".to_string(),
            arguments: serde_json::json!({"path": "a.txt"}),
        }];

        assert!(!tool_batch::should_execute_tools_in_parallel(&calls, None));
    }

    #[test]
    fn should_execute_tools_in_parallel_returns_false_when_approval_is_required() {
        use crate::approval::ApprovalGate;

        let calls = vec![
            ParsedToolCall {
                name: "shell".to_string(),
                arguments: serde_json::json!({"command": "pwd"}),
            },
            ParsedToolCall {
                name: "http_request".to_string(),
                arguments: serde_json::json!({"url": "https://example.com"}),
            },
        ];
        let approval_cfg = crate::config::AutonomyConfig::default();
        let approval_mgr = ApprovalManager::from_config(&approval_cfg);
        let gate = ApprovalGate::new(&approval_mgr, "cli", None);

        assert!(!tool_batch::should_execute_tools_in_parallel(
            &calls,
            Some(&gate)
        ));
    }

    #[test]
    fn should_execute_tools_in_parallel_returns_false_for_shell_even_under_full() {
        use crate::approval::ApprovalGate;

        let calls = vec![
            ParsedToolCall {
                name: "shell".to_string(),
                arguments: serde_json::json!({"command": "pwd"}),
            },
            ParsedToolCall {
                name: "http_request".to_string(),
                arguments: serde_json::json!({"url": "https://example.com"}),
            },
        ];
        let approval_cfg = crate::config::AutonomyConfig {
            level: crate::security::AutonomyLevel::Full,
            ..crate::config::AutonomyConfig::default()
        };
        let approval_mgr = ApprovalManager::from_config(&approval_cfg);
        let gate = ApprovalGate::new(&approval_mgr, "cli", None);

        assert!(!tool_batch::should_execute_tools_in_parallel(
            &calls,
            Some(&gate)
        ));
    }

    #[tokio::test]
    async fn run_tool_call_loop_executes_multiple_tools_in_parallel_with_ordered_results() {
        let provider = ScriptedProvider::from_text_responses(vec![
            r#"<tool_call>
{"name":"delay_a","arguments":{"value":"A"}}
</tool_call>
<tool_call>
{"name":"delay_b","arguments":{"value":"B"}}
</tool_call>"#,
            "done",
        ]);

        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let tools_registry: Vec<Box<dyn Tool>> = vec![
            Box::new(DelayTool::new(
                "delay_a",
                200,
                Arc::clone(&active),
                Arc::clone(&max_active),
            )),
            Box::new(DelayTool::new(
                "delay_b",
                200,
                Arc::clone(&active),
                Arc::clone(&max_active),
            )),
        ];

        let approval_cfg = crate::config::AutonomyConfig {
            level: crate::security::AutonomyLevel::Full,
            ..crate::config::AutonomyConfig::default()
        };
        let approval_mgr = ApprovalManager::from_config(&approval_cfg);

        let mut history = vec![
            ChatMessage::system("test-system"),
            ChatMessage::user("run tool calls"),
        ];
        let observer = NoopObserver;

        let started = std::time::Instant::now();
        let result = run_tool_call_loop(
            &provider,
            &mut history,
            &tools_registry,
            &observer,
            "mock-provider",
            "mock-model",
            0.0,
            true,
            Some(&approval_mgr),
            "telegram",
            &crate::config::MultimodalConfig::default(),
            4,
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
        .expect("parallel execution should complete");
        let elapsed = started.elapsed();

        assert_eq!(result, "done");
        assert!(
            elapsed < Duration::from_millis(350),
            "parallel execution should be faster than sequential fallback; elapsed={elapsed:?}"
        );
        assert!(
            max_active.load(Ordering::SeqCst) >= 2,
            "both tools should overlap in execution"
        );

        let tool_results_message = history
            .iter()
            .find(|msg| msg.role == "user" && msg.content.starts_with("[Tool results]"))
            .expect("tool results message should be present");
        let idx_a = tool_results_message
            .content
            .find("name=\"delay_a\"")
            .expect("delay_a result should be present");
        let idx_b = tool_results_message
            .content
            .find("name=\"delay_b\"")
            .expect("delay_b result should be present");
        assert!(
            idx_a < idx_b,
            "tool results should preserve input order for tool call mapping"
        );
    }

    #[tokio::test]
    async fn run_tool_call_loop_repairs_unparsed_markup_into_ir() {
        let bad = "<tool_call>\nNOT_JSON\n</tool_call>";
        let repair = r#"[{"name":"delay_a","arguments":{"value":"fixed"}}]"#;
        let provider = ScriptedProvider::from_text_responses(vec![bad, repair, "all good"]);

        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let tools_registry: Vec<Box<dyn Tool>> = vec![Box::new(DelayTool::new(
            "delay_a",
            1,
            Arc::clone(&active),
            Arc::clone(&max_active),
        ))];

        let approval_cfg = crate::config::AutonomyConfig {
            level: crate::security::AutonomyLevel::Full,
            ..crate::config::AutonomyConfig::default()
        };
        let approval_mgr = ApprovalManager::from_config(&approval_cfg);

        let mut history = vec![
            ChatMessage::system("test-system"),
            ChatMessage::user("run tool"),
        ];
        let observer = NoopObserver;

        let result = run_tool_call_loop(
            &provider,
            &mut history,
            &tools_registry,
            &observer,
            "mock-provider",
            "mock-model",
            0.0,
            true,
            Some(&approval_mgr),
            "cli",
            &crate::config::MultimodalConfig::default(),
            6,
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
        .expect("IR repair should recover");

        assert_eq!(result, "all good");
        assert!(!history
            .iter()
            .any(|m| m.role == "user" && m.content.contains("invalid format")));
        assert!(history
            .iter()
            .any(|m| m.role == "tool" && m.content.contains("ok:fixed")));
    }

    #[test]
    fn parse_tool_calls_extracts_single_call() {
        let response = r#"Let me check that.
<tool_call>
{"name": "shell", "arguments": {"command": "ls -la"}}
</tool_call>"#;

        let (text, calls) = parse_tool_calls(response);
        assert_eq!(text, "Let me check that.");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "ls -la"
        );
    }

    #[test]
    fn parse_tool_calls_extracts_multiple_calls() {
        let response = r#"<tool_call>
{"name": "file_read", "arguments": {"path": "a.txt"}}
</tool_call>
<tool_call>
{"name": "file_read", "arguments": {"path": "b.txt"}}
</tool_call>"#;

        let (_, calls) = parse_tool_calls(response);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "file_read");
        assert_eq!(calls[1].name, "file_read");
    }

    #[test]
    fn parse_tool_calls_returns_text_only_when_no_calls() {
        let response = "Just a normal response with no tools.";
        let (text, calls) = parse_tool_calls(response);
        assert_eq!(text, "Just a normal response with no tools.");
        assert!(calls.is_empty());
    }

    #[test]
    fn parse_tool_calls_handles_malformed_json() {
        let response = r#"<tool_call>
not valid json
</tool_call>
Some text after."#;

        let (text, calls) = parse_tool_calls(response);
        assert!(calls.is_empty());
        assert!(text.contains("Some text after."));
    }

    #[test]
    fn parse_tool_calls_text_before_and_after() {
        let response = r#"Before text.
<tool_call>
{"name": "shell", "arguments": {"command": "echo hi"}}
</tool_call>
After text."#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.contains("Before text."));
        assert!(text.contains("After text."));
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn parse_tool_calls_handles_openai_format() {
        // OpenAI-style response with tool_calls array
        let response = r#"{"content": "Let me check that for you.", "tool_calls": [{"type": "function", "function": {"name": "shell", "arguments": "{\"command\": \"ls -la\"}"}}]}"#;

        let (text, calls) = parse_tool_calls(response);
        assert_eq!(text, "Let me check that for you.");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "ls -la"
        );
    }

    #[test]
    fn parse_tool_calls_handles_openai_format_multiple_calls() {
        let response = r#"{"tool_calls": [{"type": "function", "function": {"name": "file_read", "arguments": "{\"path\": \"a.txt\"}"}}, {"type": "function", "function": {"name": "file_read", "arguments": "{\"path\": \"b.txt\"}"}}]}"#;

        let (_, calls) = parse_tool_calls(response);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "file_read");
        assert_eq!(calls[1].name, "file_read");
    }

    #[test]
    fn parse_tool_calls_openai_format_without_content() {
        // Some providers don't include content field with tool_calls
        let response = r#"{"tool_calls": [{"type": "function", "function": {"name": "memory_recall", "arguments": "{}"}}]}"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty()); // No content field
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "memory_recall");
    }

    #[test]
    fn parse_tool_calls_handles_markdown_json_inside_tool_call_tag() {
        let response = r#"<tool_call>
```json
{"name": "file_write", "arguments": {"path": "test.py", "content": "print('ok')"}}
```
</tool_call>"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "file_write");
        assert_eq!(
            calls[0].arguments.get("path").unwrap().as_str().unwrap(),
            "test.py"
        );
    }

    #[test]
    fn parse_tool_calls_handles_noisy_tool_call_tag_body() {
        let response = r#"<tool_call>
I will now call the tool with this payload:
{"name": "shell", "arguments": {"command": "pwd"}}
</tool_call>"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "pwd"
        );
    }

    #[test]
    fn parse_tool_calls_handles_xml_nested_tool_payload() {
        let response = r#"<tool_call>
<memory_recall>
<query>project roadmap</query>
</memory_recall>
</tool_call>"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "memory_recall");
        assert_eq!(
            calls[0].arguments.get("query").unwrap().as_str().unwrap(),
            "project roadmap"
        );
    }

    #[test]
    fn parse_tool_calls_ignores_xml_thinking_wrapper() {
        let response = r#"<tool_call>
<thinking>Need to inspect memory first</thinking>
<memory_recall>
<query>recent deploy notes</query>
</memory_recall>
</tool_call>"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "memory_recall");
        assert_eq!(
            calls[0].arguments.get("query").unwrap().as_str().unwrap(),
            "recent deploy notes"
        );
    }

    #[test]
    fn parse_tool_calls_handles_xml_with_json_arguments() {
        let response = r#"<tool_call>
<shell>{"command":"pwd"}</shell>
</tool_call>"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "pwd"
        );
    }

    #[test]
    fn parse_tool_calls_handles_markdown_tool_call_fence() {
        let response = r#"I'll check that.
```tool_call
{"name": "shell", "arguments": {"command": "pwd"}}
```
Done."#;

        let (text, calls) = parse_tool_calls(response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "pwd"
        );
        assert!(text.contains("I'll check that."));
        assert!(text.contains("Done."));
        assert!(!text.contains("```tool_call"));
    }

    #[test]
    fn parse_tool_calls_handles_markdown_tool_call_hybrid_close_tag() {
        let response = r#"Preface
```tool-call
{"name": "shell", "arguments": {"command": "date"}}
</tool_call>
Tail"#;

        let (text, calls) = parse_tool_calls(response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "date"
        );
        assert!(text.contains("Preface"));
        assert!(text.contains("Tail"));
        assert!(!text.contains("```tool-call"));
    }

    #[test]
    fn parse_tool_calls_handles_markdown_shell_fence() {
        let response = r#"I'll run that.
```shell
echo hello
```
Done."#;

        let (text, calls) = parse_tool_calls(response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "echo hello"
        );
        assert!(text.contains("I'll run that."));
        assert!(text.contains("Done."));
    }

    #[test]
    fn parse_tool_calls_handles_markdown_invoke_fence() {
        let response = r#"Checking.
```invoke
{"name": "shell", "arguments": {"command": "date"}}
```
Done."#;

        let (text, calls) = parse_tool_calls(response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "date"
        );
        assert!(text.contains("Checking."));
        assert!(text.contains("Done."));
    }

    #[test]
    fn parse_tool_calls_handles_toolcall_tag_alias() {
        let response = r#"<toolcall>
{"name": "shell", "arguments": {"command": "date"}}
</toolcall>"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "date"
        );
    }

    #[test]
    fn parse_tool_calls_handles_tool_dash_call_tag_alias() {
        let response = r#"<tool-call>
{"name": "shell", "arguments": {"command": "whoami"}}
</tool-call>"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "whoami"
        );
    }

    #[test]
    fn parse_tool_calls_handles_invoke_tag_alias() {
        let response = r#"<invoke>
{"name": "shell", "arguments": {"command": "uptime"}}
</invoke>"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "uptime"
        );
    }

    #[test]
    fn parse_tool_calls_recovers_unclosed_tool_call_with_json() {
        let response = r#"I will call the tool now.
<tool_call>
{"name": "shell", "arguments": {"command": "uptime -p"}}"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.contains("I will call the tool now."));
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "uptime -p"
        );
    }

    #[test]
    fn parse_tool_calls_recovers_mismatched_close_tag() {
        let response = r#"<tool_call>
{"name": "shell", "arguments": {"command": "uptime"}}
</arg_value>"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "uptime"
        );
    }

    #[test]
    fn parse_tool_calls_recovers_cross_alias_closing_tags() {
        let response = r#"<toolcall>
{"name": "shell", "arguments": {"command": "date"}}
</tool_call>"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
    }

    #[test]
    fn parse_tool_calls_rejects_raw_tool_json_without_tags() {
        // SECURITY: Raw JSON without explicit wrappers should NOT be parsed
        // This prevents prompt injection attacks where malicious content
        // could include JSON that mimics a tool call.
        let response = r#"Sure, creating the file now.
{"name": "file_write", "arguments": {"path": "hello.py", "content": "print('hello')"}}"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.contains("Sure, creating the file now."));
        assert_eq!(
            calls.len(),
            0,
            "Raw JSON without wrappers should not be parsed"
        );
    }

    #[test]
    fn build_tool_instructions_includes_all_tools() {
        let security = PolicyHandle::from_config(
            &crate::config::AutonomyConfig::default(),
            std::path::Path::new("/tmp"),
        );
        let tools = crate::tools::default_tools(security);
        let instructions = build_tool_instructions(&tools);

        assert!(instructions.contains("## Tool Use Protocol"));
        assert!(instructions.contains("<tool_call>"));
        assert!(instructions.contains("shell"));
        assert!(instructions.contains("file_read"));
        assert!(instructions.contains("file_write"));
    }

    #[test]
    fn tools_to_openai_format_produces_valid_schema() {
        let security = PolicyHandle::from_config(
            &crate::config::AutonomyConfig::default(),
            std::path::Path::new("/tmp"),
        );
        let tools = crate::tools::default_tools(security);
        let formatted = tools_to_openai_format(&tools);

        assert!(!formatted.is_empty());
        for tool_json in &formatted {
            assert_eq!(tool_json["type"], "function");
            assert!(tool_json["function"]["name"].is_string());
            assert!(tool_json["function"]["description"].is_string());
            assert!(!tool_json["function"]["name"].as_str().unwrap().is_empty());
        }
        // Verify known tools are present
        let names: Vec<&str> = formatted
            .iter()
            .filter_map(|t| t["function"]["name"].as_str())
            .collect();
        assert!(names.contains(&"shell"));
        assert!(names.contains(&"file_read"));
    }

    #[test]
    fn trim_history_preserves_system_prompt() {
        let mut history = vec![ChatMessage::system("system prompt")];
        for i in 0..DEFAULT_MAX_HISTORY_MESSAGES + 20 {
            history.push(ChatMessage::user(format!("msg {i}")));
        }
        let original_len = history.len();
        assert!(original_len > DEFAULT_MAX_HISTORY_MESSAGES + 1);

        trim_history(&mut history, DEFAULT_MAX_HISTORY_MESSAGES);

        // System prompt preserved
        assert_eq!(history[0].role, "system");
        assert_eq!(history[0].content, "system prompt");
        // Trimmed to limit
        assert_eq!(history.len(), DEFAULT_MAX_HISTORY_MESSAGES + 1); // +1 for system
                                                                     // Most recent messages preserved
        let last = &history[history.len() - 1];
        assert_eq!(
            last.content,
            format!("msg {}", DEFAULT_MAX_HISTORY_MESSAGES + 19)
        );
    }

    #[test]
    fn trim_history_noop_when_within_limit() {
        let mut history = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("hello"),
            ChatMessage::assistant("hi"),
        ];
        trim_history(&mut history, DEFAULT_MAX_HISTORY_MESSAGES);
        assert_eq!(history.len(), 3);
    }

    #[test]
    fn build_compaction_transcript_formats_roles() {
        let messages = vec![
            ChatMessage::user("I like dark mode"),
            ChatMessage::assistant("Got it"),
        ];
        let transcript = build_compaction_transcript(&messages);
        assert!(transcript.contains("USER: I like dark mode"));
        assert!(transcript.contains("ASSISTANT: Got it"));
    }

    #[test]
    fn apply_compaction_summary_replaces_old_segment() {
        let mut history = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("old 1"),
            ChatMessage::assistant("old 2"),
            ChatMessage::user("recent 1"),
            ChatMessage::assistant("recent 2"),
        ];

        apply_compaction_summary(&mut history, 1, 3, "- user prefers concise replies");

        assert_eq!(history.len(), 4);
        assert!(history[1].content.contains("Compaction summary"));
        assert!(history[2].content.contains("recent 1"));
        assert!(history[3].content.contains("recent 2"));
    }

    #[test]
    fn autosave_memory_key_has_prefix_and_uniqueness() {
        let key1 = autosave_memory_key("user_msg");
        let key2 = autosave_memory_key("user_msg");

        assert!(key1.starts_with("user_msg_"));
        assert!(key2.starts_with("user_msg_"));
        assert_ne!(key1, key2);
    }

    #[tokio::test]
    async fn autosave_memory_keys_preserve_multiple_turns() {
        let tmp = TempDir::new().unwrap();
        let mem = SqliteMemory::new(tmp.path()).unwrap();

        let key1 = autosave_memory_key("user_msg");
        let key2 = autosave_memory_key("user_msg");

        mem.store(&key1, "I'm Paul", MemoryCategory::Conversation, None)
            .await
            .unwrap();
        mem.store(&key2, "I'm 45", MemoryCategory::Conversation, None)
            .await
            .unwrap();

        assert_eq!(mem.count().await.unwrap(), 2);

        let recalled = mem.recall("45", 5, None).await.unwrap();
        assert!(recalled.iter().any(|entry| entry.content.contains("45")));
    }

    #[tokio::test]
    async fn build_context_ignores_legacy_assistant_autosave_entries() {
        let tmp = TempDir::new().unwrap();
        let mem = SqliteMemory::new(tmp.path()).unwrap();
        mem.store(
            "assistant_resp_poisoned",
            "User suffered a fabricated event",
            MemoryCategory::Daily,
            Some("sess-a"),
        )
        .await
        .unwrap();
        mem.store(
            "user_msg_real",
            "User asked for concise status updates",
            MemoryCategory::Conversation,
            Some("sess-a"),
        )
        .await
        .unwrap();

        let context = build_context(&mem, "status updates", 0.0, Some("sess-a")).await;
        assert!(context.contains("user_msg_real"));
        assert!(!context.contains("assistant_resp_poisoned"));
        assert!(!context.contains("fabricated event"));
    }

    #[tokio::test]
    async fn build_context_excludes_legacy_and_other_session_conversation() {
        let tmp = TempDir::new().unwrap();
        let mem = SqliteMemory::new(tmp.path()).unwrap();
        // Shared keyword so FTS/hybrid recall returns all rows; inject filter decides.
        mem.store(
            "legacy_shell",
            "hello: 用 shell 执行 echo hello，不要解释。",
            MemoryCategory::Conversation,
            None,
        )
        .await
        .unwrap();
        mem.store(
            "other_sess",
            "hello: previous user messages from other session",
            MemoryCategory::Conversation,
            Some("sess-old"),
        )
        .await
        .unwrap();
        mem.store(
            "core_fact",
            "hello: username is velaclaw_user",
            MemoryCategory::Core,
            None,
        )
        .await
        .unwrap();
        mem.store(
            "current_note",
            "hello: current session note about greeting",
            MemoryCategory::Conversation,
            Some("sess-new"),
        )
        .await
        .unwrap();

        let context = build_context(&mem, "hello", 0.0, Some("sess-new")).await;
        assert!(
            context.contains("username is velaclaw_user"),
            "core should inject; context={context:?}"
        );
        assert!(
            context.contains("current session note"),
            "same-session conversation should inject; context={context:?}"
        );
        assert!(!context.contains("echo hello"));
        assert!(!context.contains("other session"));
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Recovery Tests - Tool Call Parsing Edge Cases
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn parse_tool_calls_handles_empty_tool_result() {
        // Recovery: Empty tool_result tag should be handled gracefully
        let response = r#"I'll run that command.
<tool_result name="shell">

</tool_result>
Done."#;
        let (text, calls) = parse_tool_calls(response);
        assert!(text.contains("Done."));
        assert!(calls.is_empty());
    }

    #[test]
    fn parse_arguments_value_handles_null() {
        // Recovery: null arguments are returned as-is (Value::Null)
        let value = serde_json::json!(null);
        let result = parse_arguments_value(Some(&value));
        assert!(result.is_null());
    }

    #[test]
    fn parse_tool_calls_handles_empty_tool_calls_array() {
        // Recovery: Empty tool_calls array returns original response (no tool parsing)
        let response = r#"{"content": "Hello", "tool_calls": []}"#;
        let (text, calls) = parse_tool_calls(response);
        // When tool_calls is empty, the entire JSON is returned as text
        assert!(text.contains("Hello"));
        assert!(calls.is_empty());
    }

    #[test]
    fn parse_tool_calls_handles_whitespace_only_name() {
        // Recovery: Whitespace-only tool name should return None
        let value = serde_json::json!({"function": {"name": "   ", "arguments": {}}});
        let result = parse_tool_call_value(&value);
        assert!(result.is_none());
    }

    #[test]
    fn parse_tool_calls_handles_empty_string_arguments() {
        // Recovery: Empty string arguments should be handled
        let value = serde_json::json!({"name": "test", "arguments": ""});
        let result = parse_tool_call_value(&value);
        assert!(result.is_some());
        assert_eq!(result.unwrap().name, "test");
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Recovery Tests - History Management
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn trim_history_with_no_system_prompt() {
        // Recovery: History without system prompt should trim correctly
        let mut history = vec![];
        for i in 0..DEFAULT_MAX_HISTORY_MESSAGES + 20 {
            history.push(ChatMessage::user(format!("msg {i}")));
        }
        trim_history(&mut history, DEFAULT_MAX_HISTORY_MESSAGES);
        assert_eq!(history.len(), DEFAULT_MAX_HISTORY_MESSAGES);
    }

    #[test]
    fn trim_history_preserves_role_ordering() {
        // Recovery: After trimming, role ordering should remain consistent
        let mut history = vec![ChatMessage::system("system")];
        for i in 0..DEFAULT_MAX_HISTORY_MESSAGES + 10 {
            history.push(ChatMessage::user(format!("user {i}")));
            history.push(ChatMessage::assistant(format!("assistant {i}")));
        }
        trim_history(&mut history, DEFAULT_MAX_HISTORY_MESSAGES);
        assert_eq!(history[0].role, "system");
        assert_eq!(history[history.len() - 1].role, "assistant");
    }

    #[test]
    fn trim_history_with_only_system_prompt() {
        // Recovery: Only system prompt should not be trimmed
        let mut history = vec![ChatMessage::system("system prompt")];
        trim_history(&mut history, DEFAULT_MAX_HISTORY_MESSAGES);
        assert_eq!(history.len(), 1);
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Recovery Tests - Arguments Parsing
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn parse_arguments_value_handles_invalid_json_string() {
        // Recovery: Invalid JSON string should return empty object
        let value = serde_json::Value::String("not valid json".to_string());
        let result = parse_arguments_value(Some(&value));
        assert!(result.is_object());
        assert!(result.as_object().unwrap().is_empty());
    }

    #[test]
    fn parse_arguments_value_handles_none() {
        // Recovery: None arguments should return empty object
        let result = parse_arguments_value(None);
        assert!(result.is_object());
        assert!(result.as_object().unwrap().is_empty());
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Recovery Tests - JSON Extraction
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn extract_json_values_handles_empty_string() {
        // Recovery: Empty input should return empty vec
        let result = extract_json_values("");
        assert!(result.is_empty());
    }

    #[test]
    fn extract_json_values_handles_whitespace_only() {
        // Recovery: Whitespace only should return empty vec
        let result = extract_json_values("   \n\t  ");
        assert!(result.is_empty());
    }

    #[test]
    fn extract_json_values_handles_multiple_objects() {
        // Recovery: Multiple JSON objects should all be extracted
        let input = r#"{"a": 1}{"b": 2}{"c": 3}"#;
        let result = extract_json_values(input);
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn extract_json_values_handles_arrays() {
        // Recovery: JSON arrays should be extracted
        let input = r#"[1, 2, 3]{"key": "value"}"#;
        let result = extract_json_values(input);
        assert_eq!(result.len(), 2);
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Recovery Tests - Constants Validation
    // ═══════════════════════════════════════════════════════════════════════

    const _: () = {
        assert!(DEFAULT_MAX_TOOL_ITERATIONS > 0);
        assert!(DEFAULT_MAX_TOOL_ITERATIONS <= 100);
        assert!(DEFAULT_MAX_HISTORY_MESSAGES > 0);
        assert!(DEFAULT_MAX_HISTORY_MESSAGES <= 1000);
    };

    #[test]
    fn constants_bounds_are_compile_time_checked() {
        // Bounds are enforced by the const assertions above.
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Recovery Tests - Tool Call Value Parsing
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn parse_tool_call_value_handles_missing_name_field() {
        // Recovery: Missing name field should return None
        let value = serde_json::json!({"function": {"arguments": {}}});
        let result = parse_tool_call_value(&value);
        assert!(result.is_none());
    }

    #[test]
    fn parse_tool_call_value_handles_top_level_name() {
        // Recovery: Tool call with name at top level (non-OpenAI format)
        let value = serde_json::json!({"name": "test_tool", "arguments": {}});
        let result = parse_tool_call_value(&value);
        assert!(result.is_some());
        assert_eq!(result.unwrap().name, "test_tool");
    }

    #[test]
    fn parse_tool_call_value_accepts_top_level_parameters_alias() {
        let value = serde_json::json!({
            "name": "schedule",
            "parameters": {"action": "create", "message": "test"}
        });
        let result = parse_tool_call_value(&value).expect("tool call should parse");
        assert_eq!(result.name, "schedule");
        assert_eq!(
            result.arguments.get("action").and_then(|v| v.as_str()),
            Some("create")
        );
    }

    #[test]
    fn parse_tool_call_value_accepts_function_parameters_alias() {
        let value = serde_json::json!({
            "function": {
                "name": "shell",
                "parameters": {"command": "date"}
            }
        });
        let result = parse_tool_call_value(&value).expect("tool call should parse");
        assert_eq!(result.name, "shell");
        assert_eq!(
            result.arguments.get("command").and_then(|v| v.as_str()),
            Some("date")
        );
    }

    #[test]
    fn parse_tool_calls_from_json_value_handles_empty_array() {
        // Recovery: Empty tool_calls array should return empty vec
        let value = serde_json::json!({"tool_calls": []});
        let result = parse_tool_calls_from_json_value(&value);
        assert!(result.is_empty());
    }

    #[test]
    fn parse_tool_calls_from_json_value_handles_missing_tool_calls() {
        // Recovery: Missing tool_calls field should fall through
        let value = serde_json::json!({"name": "test", "arguments": {}});
        let result = parse_tool_calls_from_json_value(&value);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn parse_tool_calls_from_json_value_handles_top_level_array() {
        // Recovery: Top-level array of tool calls
        let value = serde_json::json!([
            {"name": "tool_a", "arguments": {}},
            {"name": "tool_b", "arguments": {}}
        ]);
        let result = parse_tool_calls_from_json_value(&value);
        assert_eq!(result.len(), 2);
    }

    // ═══════════════════════════════════════════════════════════════════════
    // GLM-Style Tool Call Parsing
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn parse_glm_style_browser_open_url() {
        let response = "browser_open/url>https://example.com";
        let calls = parse_glm_style_tool_calls(response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "shell");
        assert!(calls[0].1["command"].as_str().unwrap().contains("curl"));
        assert!(calls[0].1["command"]
            .as_str()
            .unwrap()
            .contains("example.com"));
    }

    #[test]
    fn parse_glm_style_shell_command() {
        let response = "shell/command>ls -la";
        let calls = parse_glm_style_tool_calls(response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "shell");
        assert_eq!(calls[0].1["command"], "ls -la");
    }

    #[test]
    fn parse_glm_style_http_request() {
        let response = "http_request/url>https://api.example.com/data";
        let calls = parse_glm_style_tool_calls(response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "http_request");
        assert_eq!(calls[0].1["url"], "https://api.example.com/data");
        assert_eq!(calls[0].1["method"], "GET");
    }

    #[test]
    fn parse_glm_style_plain_url() {
        let response = "https://example.com/api";
        let calls = parse_glm_style_tool_calls(response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "shell");
        assert!(calls[0].1["command"].as_str().unwrap().contains("curl"));
    }

    #[test]
    fn parse_glm_style_json_args() {
        let response = r#"shell/{"command": "echo hello"}"#;
        let calls = parse_glm_style_tool_calls(response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "shell");
        assert_eq!(calls[0].1["command"], "echo hello");
    }

    #[test]
    fn parse_glm_style_multiple_calls() {
        let response = r#"shell/command>ls
browser_open/url>https://example.com"#;
        let calls = parse_glm_style_tool_calls(response);
        assert_eq!(calls.len(), 2);
    }

    #[test]
    fn parse_glm_style_tool_call_integration() {
        // Integration test: GLM format should be parsed in parse_tool_calls
        let response = "Checking...\nbrowser_open/url>https://example.com\nDone";
        let (text, calls) = parse_tool_calls(response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert!(text.contains("Checking"));
        assert!(text.contains("Done"));
    }

    #[test]
    fn parse_glm_style_rejects_non_http_url_param() {
        let response = "browser_open/url>javascript:alert(1)";
        let calls = parse_glm_style_tool_calls(response);
        assert!(calls.is_empty());
    }

    #[test]
    fn parse_tool_calls_handles_unclosed_tool_call_tag() {
        let response = "<tool_call>{\"name\":\"shell\",\"arguments\":{\"command\":\"pwd\"}}\nDone";
        let (text, calls) = parse_tool_calls(response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(calls[0].arguments["command"], "pwd");
        assert_eq!(text, "Done");
    }

    // ─────────────────────────────────────────────────────────────────────
    // TG4 (inline): parse_tool_calls robustness — malformed/edge-case inputs
    // Prevents: Pattern 4 issues #746, #418, #777, #848
    // ─────────────────────────────────────────────────────────────────────

    #[test]
    fn parse_tool_calls_empty_input_returns_empty() {
        let (text, calls) = parse_tool_calls("");
        assert!(calls.is_empty(), "empty input should produce no tool calls");
        assert!(text.is_empty(), "empty input should produce no text");
    }

    #[test]
    fn parse_tool_calls_whitespace_only_returns_empty_calls() {
        let (text, calls) = parse_tool_calls("   \n\t  ");
        assert!(calls.is_empty());
        assert!(text.is_empty() || text.trim().is_empty());
    }

    #[test]
    fn parse_tool_calls_nested_xml_tags_handled() {
        // Double-wrapped tool call should still parse the inner call
        let response = r#"<tool_call><tool_call>{"name":"echo","arguments":{"msg":"hi"}}</tool_call></tool_call>"#;
        let (_text, calls) = parse_tool_calls(response);
        // Should find at least one tool call
        assert!(
            !calls.is_empty(),
            "nested XML tags should still yield at least one tool call"
        );
    }

    #[test]
    fn parse_tool_calls_truncated_json_no_panic() {
        // Incomplete JSON inside tool_call tags
        let response = r#"<tool_call>{"name":"shell","arguments":{"command":"ls"</tool_call>"#;
        let (_text, _calls) = parse_tool_calls(response);
        // Should not panic — graceful handling of truncated JSON
    }

    #[test]
    fn parse_tool_calls_empty_json_object_in_tag() {
        let response = "<tool_call>{}</tool_call>";
        let (_text, calls) = parse_tool_calls(response);
        // Empty JSON object has no name field — should not produce valid tool call
        assert!(
            calls.is_empty(),
            "empty JSON object should not produce a tool call"
        );
    }

    #[test]
    fn parse_tool_calls_closing_tag_only_returns_text() {
        let response = "Some text </tool_call> more text";
        let (text, calls) = parse_tool_calls(response);
        assert!(
            calls.is_empty(),
            "closing tag only should not produce calls"
        );
        assert!(
            !text.is_empty(),
            "text around orphaned closing tag should be preserved"
        );
    }

    #[test]
    fn parse_tool_calls_very_large_arguments_no_panic() {
        let large_arg = "x".repeat(100_000);
        let response = format!(
            r#"<tool_call>{{"name":"echo","arguments":{{"message":"{}"}}}}</tool_call>"#,
            large_arg
        );
        let (_text, calls) = parse_tool_calls(&response);
        assert_eq!(calls.len(), 1, "large arguments should still parse");
        assert_eq!(calls[0].name, "echo");
    }

    #[test]
    fn parse_tool_calls_special_characters_in_arguments() {
        let response = r#"<tool_call>{"name":"echo","arguments":{"message":"hello \"world\" <>&'\n\t"}}</tool_call>"#;
        let (_text, calls) = parse_tool_calls(response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "echo");
    }

    #[test]
    fn parse_tool_calls_text_with_embedded_json_not_extracted() {
        // Raw JSON without any tags should NOT be extracted as a tool call
        let response = r#"Here is some data: {"name":"echo","arguments":{"message":"hi"}} end."#;
        let (_text, calls) = parse_tool_calls(response);
        assert!(
            calls.is_empty(),
            "raw JSON in text without tags should not be extracted"
        );
    }

    #[test]
    fn parse_tool_calls_multiple_formats_mixed() {
        // Mix of text and properly tagged tool call
        let response = r#"I'll help you with that.

<tool_call>
{"name":"shell","arguments":{"command":"echo hello"}}
</tool_call>

Let me check the result."#;
        let (text, calls) = parse_tool_calls(response);
        assert_eq!(
            calls.len(),
            1,
            "should extract one tool call from mixed content"
        );
        assert_eq!(calls[0].name, "shell");
        assert!(
            text.contains("help you"),
            "text before tool call should be preserved"
        );
    }

    // ─────────────────────────────────────────────────────────────────────
    // TG4 (inline): scrub_credentials edge cases
    // ─────────────────────────────────────────────────────────────────────

    #[test]
    fn scrub_credentials_empty_input() {
        let result = tool_batch::scrub_credentials("");
        assert_eq!(result, "");
    }

    #[test]
    fn scrub_credentials_no_sensitive_data() {
        let input = "normal text without any secrets";
        let result = tool_batch::scrub_credentials(input);
        assert_eq!(
            result, input,
            "non-sensitive text should pass through unchanged"
        );
    }

    #[test]
    fn scrub_credentials_short_values_not_redacted() {
        // Values shorter than 8 chars should not be redacted
        let input = r#"api_key="short""#;
        let result = tool_batch::scrub_credentials(input);
        assert_eq!(result, input, "short values should not be redacted");
    }

    // ─────────────────────────────────────────────────────────────────────
    // TG4 (inline): trim_history edge cases
    // ─────────────────────────────────────────────────────────────────────

    #[test]
    fn trim_history_empty_history() {
        let mut history: Vec<crate::providers::ChatMessage> = vec![];
        trim_history(&mut history, 10);
        assert!(history.is_empty());
    }

    #[test]
    fn trim_history_system_only() {
        let mut history = vec![crate::providers::ChatMessage::system("system prompt")];
        trim_history(&mut history, 10);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].role, "system");
    }

    #[test]
    fn trim_history_exactly_at_limit() {
        let mut history = vec![
            crate::providers::ChatMessage::system("system"),
            crate::providers::ChatMessage::user("msg 1"),
            crate::providers::ChatMessage::assistant("reply 1"),
        ];
        trim_history(&mut history, 2); // 2 non-system messages = exactly at limit
        assert_eq!(history.len(), 3, "should not trim when exactly at limit");
    }

    #[test]
    fn trim_history_removes_oldest_non_system() {
        let mut history = vec![
            crate::providers::ChatMessage::system("system"),
            crate::providers::ChatMessage::user("old msg"),
            crate::providers::ChatMessage::assistant("old reply"),
            crate::providers::ChatMessage::user("new msg"),
            crate::providers::ChatMessage::assistant("new reply"),
        ];
        trim_history(&mut history, 2);
        assert_eq!(history.len(), 3); // system + 2 kept
        assert_eq!(history[0].role, "system");
        assert_eq!(history[1].content, "new msg");
    }

    /// When `build_system_prompt_with_mode` is called with `native_tools = true`,
    /// the output must contain ZERO XML protocol artifacts. In the native path
    /// `build_tool_instructions` is never called, so the system prompt alone
    /// must be clean of XML tool-call protocol.
    #[test]
    fn native_tools_system_prompt_contains_zero_xml() {
        use crate::channels::build_system_prompt_with_mode;

        let tool_summaries: Vec<(&str, &str)> = vec![
            ("shell", "Execute shell commands"),
            ("file_read", "Read files"),
        ];

        let system_prompt = build_system_prompt_with_mode(
            std::path::Path::new("/tmp"),
            "test-model",
            &tool_summaries,
            &[],                                            // no skills
            None,                                           // no identity config
            None,                                           // no bootstrap_max_chars
            true,                                           // native_tools
            crate::config::SkillsPromptInjectionMode::Full, // skills_prompt_mode
        );

        // Must contain zero XML protocol artifacts
        assert!(
            !system_prompt.contains("<tool_call>"),
            "Native prompt must not contain <tool_call>"
        );
        assert!(
            !system_prompt.contains("</tool_call>"),
            "Native prompt must not contain </tool_call>"
        );
        assert!(
            !system_prompt.contains("<tool_result>"),
            "Native prompt must not contain <tool_result>"
        );
        assert!(
            !system_prompt.contains("</tool_result>"),
            "Native prompt must not contain </tool_result>"
        );
        assert!(
            !system_prompt.contains("## Tool Use Protocol"),
            "Native prompt must not contain XML protocol header"
        );

        // Positive: native prompt should still list tools and contain task instructions
        assert!(
            system_prompt.contains("shell"),
            "Native prompt must list tool names"
        );
        assert!(
            system_prompt.contains("## Your Task"),
            "Native prompt should contain task instructions"
        );
    }
}
