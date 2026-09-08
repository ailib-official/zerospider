use crate::agent::dispatcher::ToolDispatcher;
use crate::agent::memory_loader::{DefaultMemoryLoader, MemoryLoader};
use crate::agent::prompt::{PromptContext, SystemPromptBuilder};
use crate::approval::{ApprovalHub, ApprovalManager, HumanInputHub};
use crate::cli_render::{prefix_agent_lines, RenderOpts};
use crate::config::{Config, DEFAULT_PROTOCOL_MODEL_ID};
use crate::memory::{self, Memory, MemoryCategory};
use crate::observability::{Observer, ObserverEvent};
use crate::providers::{ChatMessage, ConversationMessage, Provider};
use crate::security::PolicyHandle;
use crate::tools::{HumanInputAttach, Tool, ToolSpec};
use anyhow::Result;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use velaclaw_agent_runtime::{conversation_from_tool_loop_history, reintegrate_prepared_chat};

pub struct Agent {
    #[cfg(feature = "ai-protocol")]
    execution: Option<crate::execution::ExecutionHandle>,
    provider: Box<dyn Provider>,
    tools: Vec<Box<dyn Tool>>,
    tool_specs: Vec<ToolSpec>,
    memory: Arc<dyn Memory>,
    observer: Arc<dyn Observer>,
    prompt_builder: SystemPromptBuilder,
    tool_dispatcher: Box<dyn ToolDispatcher>,
    memory_loader: Box<dyn MemoryLoader>,
    config: crate::config::AgentConfig,
    model_name: String,
    temperature: f64,
    workspace_dir: std::path::PathBuf,
    identity_config: crate::config::IdentityConfig,
    skills: Vec<crate::skills::Skill>,
    skills_prompt_mode: crate::config::SkillsPromptInjectionMode,
    auto_save: bool,
    /// VL-MEM-001: fresh session on agent construct; Conversation autosave/recall scoped here.
    session_id: String,
    history: Vec<ConversationMessage>,
    classification_config: crate::config::QueryClassificationConfig,
    available_hints: Vec<String>,
    peer_logical_ids: Vec<String>,
    model_routes: Vec<crate::config::ModelRouteConfig>,
    security: PolicyHandle,
    gateway_approval: Option<(ApprovalManager, Arc<ApprovalHub>)>,
    /// Shared attach slot for `request_human_input` (same Arc as the tool).
    human_input_attach: HumanInputAttach,
    /// Active hub when gateway HITL is enabled (for secret_slot resolution).
    human_input_hub: Option<Arc<HumanInputHub>>,
    /// When set, `turn` / `run_interactive` render Markdown and prefix agent lines for CLI.
    cli_render: Option<RenderOpts>,
    /// CR-CAP-003: opt-in intent→Tag→index route host context (default-off).
    #[cfg(feature = "ai-protocol")]
    intent_route_host: Option<crate::agent::intent_route::IntentRouteHost>,
    /// ORCH-HOST-001: opt-in host Decide context (default-off).
    #[cfg(feature = "ai-protocol")]
    host_decide_host: Option<crate::orchestration::HostDecideHost>,
    /// Explicit user pick (Web `model_id` / CLI `-p/--model`); beats host_decide.
    explicit_model: Option<String>,
    /// Last turn's model decision (observe / UX honesty).
    #[cfg(feature = "ai-protocol")]
    last_turn_model: Option<crate::orchestration::TurnModelDecision>,
    /// Per-turn cancel (Web Stop / CLI double-Esc).
    cancellation_token: Option<tokio_util::sync::CancellationToken>,
    /// Optional progress fan-out for Web WS frames.
    progress_tx: Option<tokio::sync::mpsc::Sender<crate::agent::turn_progress::TurnProgress>>,
    /// VL-MA-004: Plan blocks mutating tools; default Build.
    host_phase: crate::agent::host_phase::HostPhase,
    /// Per-node shell probe governor for the current live DAG (VL-NA-040).
    hop_probes: HashMap<String, Arc<std::sync::Mutex<crate::agent::probe_dedup::HopProbeGovernor>>>,
    current_hop_probe: Option<Arc<std::sync::Mutex<crate::agent::probe_dedup::HopProbeGovernor>>>,
}

pub struct AgentBuilder {
    #[cfg(feature = "ai-protocol")]
    execution: Option<crate::execution::ExecutionHandle>,
    provider: Option<Box<dyn Provider>>,
    tools: Option<Vec<Box<dyn Tool>>>,
    memory: Option<Arc<dyn Memory>>,
    observer: Option<Arc<dyn Observer>>,
    prompt_builder: Option<SystemPromptBuilder>,
    tool_dispatcher: Option<Box<dyn ToolDispatcher>>,
    memory_loader: Option<Box<dyn MemoryLoader>>,
    config: Option<crate::config::AgentConfig>,
    model_name: Option<String>,
    temperature: Option<f64>,
    workspace_dir: Option<std::path::PathBuf>,
    identity_config: Option<crate::config::IdentityConfig>,
    skills: Option<Vec<crate::skills::Skill>>,
    skills_prompt_mode: Option<crate::config::SkillsPromptInjectionMode>,
    auto_save: Option<bool>,
    classification_config: Option<crate::config::QueryClassificationConfig>,
    available_hints: Option<Vec<String>>,
    peer_logical_ids: Option<Vec<String>>,
    model_routes: Option<Vec<crate::config::ModelRouteConfig>>,
    security: Option<PolicyHandle>,
    human_input_attach: Option<HumanInputAttach>,
    #[cfg(feature = "ai-protocol")]
    intent_route_host: Option<crate::agent::intent_route::IntentRouteHost>,
    #[cfg(feature = "ai-protocol")]
    host_decide_host: Option<crate::orchestration::HostDecideHost>,
    explicit_model: Option<String>,
}

impl AgentBuilder {
    pub fn new() -> Self {
        Self {
            #[cfg(feature = "ai-protocol")]
            execution: None,
            provider: None,
            tools: None,
            memory: None,
            observer: None,
            prompt_builder: None,
            tool_dispatcher: None,
            memory_loader: None,
            config: None,
            model_name: None,
            temperature: None,
            workspace_dir: None,
            identity_config: None,
            skills: None,
            skills_prompt_mode: None,
            auto_save: None,
            classification_config: None,
            available_hints: None,
            peer_logical_ids: None,
            model_routes: None,
            security: None,
            human_input_attach: None,
            #[cfg(feature = "ai-protocol")]
            intent_route_host: None,
            #[cfg(feature = "ai-protocol")]
            host_decide_host: None,
            explicit_model: None,
        }
    }

    pub fn provider(mut self, provider: Box<dyn Provider>) -> Self {
        self.provider = Some(provider);
        self
    }

    #[cfg(feature = "ai-protocol")]
    pub fn execution(mut self, execution: Option<crate::execution::ExecutionHandle>) -> Self {
        self.execution = execution;
        self
    }

    pub fn tools(mut self, tools: Vec<Box<dyn Tool>>) -> Self {
        self.tools = Some(tools);
        self
    }

    pub fn memory(mut self, memory: Arc<dyn Memory>) -> Self {
        self.memory = Some(memory);
        self
    }

    pub fn observer(mut self, observer: Arc<dyn Observer>) -> Self {
        self.observer = Some(observer);
        self
    }

    pub fn prompt_builder(mut self, prompt_builder: SystemPromptBuilder) -> Self {
        self.prompt_builder = Some(prompt_builder);
        self
    }

    pub fn tool_dispatcher(mut self, tool_dispatcher: Box<dyn ToolDispatcher>) -> Self {
        self.tool_dispatcher = Some(tool_dispatcher);
        self
    }

    pub fn memory_loader(mut self, memory_loader: Box<dyn MemoryLoader>) -> Self {
        self.memory_loader = Some(memory_loader);
        self
    }

    pub fn config(mut self, config: crate::config::AgentConfig) -> Self {
        self.config = Some(config);
        self
    }

    pub fn model_name(mut self, model_name: String) -> Self {
        self.model_name = Some(model_name);
        self
    }

    pub fn temperature(mut self, temperature: f64) -> Self {
        self.temperature = Some(temperature);
        self
    }

    pub fn workspace_dir(mut self, workspace_dir: std::path::PathBuf) -> Self {
        self.workspace_dir = Some(workspace_dir);
        self
    }

    pub fn identity_config(mut self, identity_config: crate::config::IdentityConfig) -> Self {
        self.identity_config = Some(identity_config);
        self
    }

    pub fn skills(mut self, skills: Vec<crate::skills::Skill>) -> Self {
        self.skills = Some(skills);
        self
    }

    pub fn skills_prompt_mode(
        mut self,
        skills_prompt_mode: crate::config::SkillsPromptInjectionMode,
    ) -> Self {
        self.skills_prompt_mode = Some(skills_prompt_mode);
        self
    }

    pub fn auto_save(mut self, auto_save: bool) -> Self {
        self.auto_save = Some(auto_save);
        self
    }

    pub fn classification_config(
        mut self,
        classification_config: crate::config::QueryClassificationConfig,
    ) -> Self {
        self.classification_config = Some(classification_config);
        self
    }

    pub fn available_hints(mut self, available_hints: Vec<String>) -> Self {
        self.available_hints = Some(available_hints);
        self
    }

    pub fn peer_logical_ids(mut self, peer_logical_ids: Vec<String>) -> Self {
        self.peer_logical_ids = Some(peer_logical_ids);
        self
    }

    pub fn model_routes(mut self, model_routes: Vec<crate::config::ModelRouteConfig>) -> Self {
        self.model_routes = Some(model_routes);
        self
    }

    pub fn security(mut self, security: PolicyHandle) -> Self {
        self.security = Some(security);
        self
    }

    pub fn human_input_attach(mut self, attach: HumanInputAttach) -> Self {
        self.human_input_attach = Some(attach);
        self
    }

    #[cfg(feature = "ai-protocol")]
    pub fn intent_route_host(
        mut self,
        intent_route_host: Option<crate::agent::intent_route::IntentRouteHost>,
    ) -> Self {
        self.intent_route_host = intent_route_host;
        self
    }

    #[cfg(feature = "ai-protocol")]
    pub fn host_decide_host(
        mut self,
        host_decide_host: Option<crate::orchestration::HostDecideHost>,
    ) -> Self {
        self.host_decide_host = host_decide_host;
        self
    }

    pub fn explicit_model(mut self, explicit_model: Option<String>) -> Self {
        self.explicit_model = explicit_model;
        self
    }

    pub fn build(self) -> Result<Agent> {
        let tools = self
            .tools
            .ok_or_else(|| anyhow::anyhow!("tools are required"))?;
        let tool_specs = tools.iter().map(|tool| tool.spec()).collect();

        Ok(Agent {
            #[cfg(feature = "ai-protocol")]
            execution: self.execution,
            provider: self
                .provider
                .ok_or_else(|| anyhow::anyhow!("provider is required"))?,
            tools,
            tool_specs,
            memory: self
                .memory
                .ok_or_else(|| anyhow::anyhow!("memory is required"))?,
            observer: self
                .observer
                .ok_or_else(|| anyhow::anyhow!("observer is required"))?,
            prompt_builder: self
                .prompt_builder
                .unwrap_or_else(SystemPromptBuilder::with_defaults),
            tool_dispatcher: self
                .tool_dispatcher
                .ok_or_else(|| anyhow::anyhow!("tool_dispatcher is required"))?,
            memory_loader: self
                .memory_loader
                .unwrap_or_else(|| Box::new(DefaultMemoryLoader::default())),
            config: self.config.unwrap_or_default(),
            model_name: self
                .model_name
                .unwrap_or_else(|| DEFAULT_PROTOCOL_MODEL_ID.into()),
            temperature: self.temperature.unwrap_or(0.7),
            workspace_dir: self
                .workspace_dir
                .unwrap_or_else(|| std::path::PathBuf::from(".")),
            identity_config: self.identity_config.unwrap_or_default(),
            skills: self.skills.unwrap_or_default(),
            skills_prompt_mode: self.skills_prompt_mode.unwrap_or_default(),
            auto_save: self.auto_save.unwrap_or(false),
            session_id: memory::new_session_id(),
            history: Vec::new(),
            classification_config: self.classification_config.unwrap_or_default(),
            available_hints: self.available_hints.unwrap_or_default(),
            peer_logical_ids: self.peer_logical_ids.unwrap_or_default(),
            model_routes: self.model_routes.unwrap_or_default(),
            security: self
                .security
                .ok_or_else(|| anyhow::anyhow!("security is required"))?,
            gateway_approval: None,
            human_input_attach: self
                .human_input_attach
                .unwrap_or_else(|| Arc::new(Mutex::new(None))),
            human_input_hub: None,
            cli_render: None,
            #[cfg(feature = "ai-protocol")]
            intent_route_host: self.intent_route_host,
            #[cfg(feature = "ai-protocol")]
            host_decide_host: self.host_decide_host,
            explicit_model: self.explicit_model,
            #[cfg(feature = "ai-protocol")]
            last_turn_model: None,
            cancellation_token: None,
            progress_tx: None,
            host_phase: crate::agent::host_phase::HostPhase::Build,
            hop_probes: HashMap::new(),
            current_hop_probe: None,
        })
    }
}

impl Agent {
    pub fn builder() -> AgentBuilder {
        AgentBuilder::new()
    }

    pub fn history(&self) -> &[ConversationMessage] {
        &self.history
    }

    pub fn clear_history(&mut self) {
        self.history.clear();
    }

    /// Start a fresh memory session (Conversation/Daily isolation); Core unchanged.
    pub fn start_new_session(&mut self) {
        self.session_id = memory::new_session_id();
        self.history.clear();
    }

    /// Align Agent memory / host_decide session key with an external session id (Web chat).
    pub fn set_session_id(&mut self, session_id: impl Into<String>) {
        let id = session_id.into();
        if !id.trim().is_empty() {
            self.session_id = id;
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Mark an explicit user model pick (Web picker / CLI flags). Beats `host_decide`.
    pub fn set_explicit_model(&mut self, model: Option<String>) {
        self.explicit_model = model.filter(|m| !m.trim().is_empty());
    }

    #[cfg(feature = "ai-protocol")]
    pub fn last_turn_model(&self) -> Option<&crate::orchestration::TurnModelDecision> {
        self.last_turn_model.as_ref()
    }

    /// Enable CLI Markdown rendering and `>>` speaker prefixes for this agent session.
    pub fn set_cli_render(&mut self, opts: RenderOpts) {
        self.cli_render = Some(opts);
    }

    fn format_agent_output(&self, text: &str) -> String {
        if let Some(render_opts) = self.cli_render {
            let rendered = render_opts.render(text);
            prefix_agent_lines(&rendered, render_opts.style)
        } else {
            text.to_string()
        }
    }

    /// Cancel token for the next `turn` (Web Stop / CLI double-Esc).
    pub fn set_cancellation_token(&mut self, token: Option<tokio_util::sync::CancellationToken>) {
        self.cancellation_token = token;
    }

    /// Progress sink for the next `turn` (WebSocket status/step frames).
    pub fn set_progress_tx(
        &mut self,
        tx: Option<tokio::sync::mpsc::Sender<crate::agent::turn_progress::TurnProgress>>,
    ) {
        self.progress_tx = tx;
    }

    fn emit_turn_progress(&self, progress: crate::agent::turn_progress::TurnProgress) {
        if self.cli_render.is_some() {
            crate::agent::turn_progress::print_cli_progress(&progress, None);
        }
        if let Some(tx) = &self.progress_tx {
            let _ = tx.try_send(progress);
        }
    }

    #[cfg(feature = "ai-protocol")]
    fn push_operator_note(&self, prefix: &mut String, text: &str) {
        if let Some(progress) =
            crate::agent::bounded_dag_delivery::append_operator_chunk(prefix, text)
        {
            self.emit_turn_progress(progress);
        }
    }

    #[cfg(feature = "ai-protocol")]
    fn flush_hop_probe_notices(
        &mut self,
        probe: &Arc<std::sync::Mutex<crate::agent::probe_dedup::HopProbeGovernor>>,
        prefix: &mut String,
    ) {
        self.current_hop_probe = None;
        for note in probe
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain_notices()
        {
            self.push_operator_note(prefix, &note);
        }
    }

    #[cfg(feature = "ai-protocol")]
    fn end_live_graph_host_state(&mut self) {
        self.security.set_graph_scratch_rel(None);
        self.current_hop_probe = None;
        self.hop_probes.clear();
    }

    fn session_work_model(&self) -> &str {
        self.explicit_model
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(self.model_name.as_str())
    }

    fn dag_contact_labels(
        &self,
        dag: &crate::agent::dag_runner::DagManifest,
        order: &[String],
    ) -> std::collections::HashMap<String, String> {
        crate::agent::bounded_dag_live::dag_contact_labels(
            self.provider.as_ref(),
            dag,
            order,
            self.session_work_model(),
            &self.available_hints,
        )
    }

    pub fn set_host_phase(&mut self, phase: crate::agent::host_phase::HostPhase) {
        self.host_phase = phase;
    }

    /// Enable interactive tool approval for gateway/Web chat (`VL-UI-004`).
    pub fn enable_gateway_approval(
        &mut self,
        hub: Arc<ApprovalHub>,
        config: &crate::config::Config,
    ) -> anyhow::Result<()> {
        let manager = crate::config::create_approval_manager(config)?;
        self.gateway_approval = Some((manager, hub));
        Ok(())
    }

    /// Enable interactive human-input prompts (choice / text / secret / handoff).
    pub fn enable_gateway_hitl(&mut self, hub: Arc<HumanInputHub>) {
        *self.human_input_attach.lock() = Some(Arc::clone(&hub));
        self.human_input_hub = Some(hub);
    }

    pub fn from_config(config: &Config) -> Result<Self> {
        let assembled = crate::agent::assemble::assemble_runtime(
            config,
            crate::config::BootstrapOptions {
                with_embedding_routes: true,
            },
        )?;
        let security = assembled.boot.security;
        let memory = assembled.boot.memory;
        let observer = assembled.boot.observer;
        let tools = assembled.boot.tools;
        let human_input_attach = assembled.boot.human_input_attach;
        let provider = assembled.provider;
        let model_name = assembled.model_name;
        let tool_dispatcher = assembled.tool_dispatcher;

        let available_hints: Vec<String> =
            config.model_routes.iter().map(|r| r.hint.clone()).collect();

        #[cfg(feature = "ai-protocol")]
        let builder = Agent::builder()
            .execution(assembled.execution)
            .provider(provider);
        #[cfg(not(feature = "ai-protocol"))]
        let builder = Agent::builder().provider(provider);

        let builder = builder
            .tools(tools)
            .memory(memory)
            .observer(observer)
            .tool_dispatcher(tool_dispatcher)
            .memory_loader(Box::new(DefaultMemoryLoader::new(
                5,
                config.memory.min_relevance_score,
            )))
            .prompt_builder(SystemPromptBuilder::with_defaults())
            .config(config.agent.clone())
            .model_name(model_name)
            .temperature(config.default_temperature)
            .workspace_dir(config.workspace_dir.clone())
            .classification_config(config.query_classification.clone())
            .available_hints(available_hints)
            .peer_logical_ids(crate::agent::loop_::logical_ids_from_config(config))
            .model_routes(config.model_routes.clone())
            .identity_config(config.identity.clone())
            .skills(crate::skills::load_skills_with_config(
                &config.workspace_dir,
                config,
            ))
            .skills_prompt_mode(config.skills.prompt_injection_mode)
            .auto_save(config.memory.auto_save)
            .security(security)
            .human_input_attach(human_input_attach);

        #[cfg(feature = "ai-protocol")]
        let builder = builder.intent_route_host(Some(
            crate::agent::intent_route::IntentRouteHost::from_config(config),
        ));

        #[cfg(feature = "ai-protocol")]
        let builder = builder.host_decide_host(Some(
            crate::orchestration::HostDecideHost::from_config(config),
        ));

        builder.build()
    }

    /// BYOK execution handle when the agent was built from config (VL-EVO-001).
    #[cfg(feature = "ai-protocol")]
    pub fn execution(&self) -> Option<&crate::execution::ExecutionHandle> {
        self.execution.as_ref()
    }

    /// Mid-loop message-count safety net (not a second orch pipeline).
    /// Full compact+layered runs at turn start/end via [`prepare_history_after_turn`].
    fn trim_conversation_cap(&mut self) {
        let max = self.config.max_history_messages;
        if self.history.len() <= max {
            return;
        }

        let mut system_messages = Vec::new();
        let mut other_messages = Vec::new();

        for msg in self.history.drain(..) {
            match &msg {
                ConversationMessage::Chat(chat) if chat.role == "system" => {
                    system_messages.push(msg);
                }
                _ => other_messages.push(msg),
            }
        }

        if other_messages.len() > max {
            let drop_count = other_messages.len() - max;
            other_messages.drain(0..drop_count);
        }

        self.history = system_messages;
        self.history.extend(other_messages);
    }

    /// End-of-turn history prepare (GOV-007 / VL-CTX-001): compact + layered or trim.
    async fn prepare_history_after_turn(&mut self) -> Result<()> {
        self.prepare_conversation_history().await
    }

    /// Run [`prepare_turn_history`] on Chat frames and reintegrate without
    /// reordering native `AssistantToolCalls` / `ToolResults` frames.
    async fn prepare_conversation_history(&mut self) -> Result<()> {
        self.prepare_conversation_history_inner(true, None).await
    }

    async fn prepare_conversation_history_inner(
        &mut self,
        include_turn_retrieve: bool,
        hop_model: Option<&str>,
    ) -> Result<()> {
        let original_chat_count = self
            .history
            .iter()
            .filter(|m| matches!(m, ConversationMessage::Chat(_)))
            .count();
        let mut chat_hist: Vec<ChatMessage> = self
            .history
            .iter()
            .filter_map(|m| match m {
                ConversationMessage::Chat(c) => Some(c.clone()),
                _ => None,
            })
            .collect();
        if chat_hist.is_empty() {
            self.trim_conversation_cap();
            return Ok(());
        }
        let summarizer = crate::agent::context_orch::HistorySummarizer {
            provider: self.provider.as_ref(),
            model: &self.model_name,
        };
        let extra_chunks = if include_turn_retrieve {
            crate::agent::context_contract::retrieve_turn_extra_chunks(
                &self.workspace_dir,
                self.memory.as_ref(),
                chat_hist
                    .iter()
                    .rev()
                    .find(|m| m.role == "user")
                    .map(|m| m.content.as_str())
                    .unwrap_or(""),
                Some(self.session_id.as_str()),
            )
            .await
        } else {
            Vec::new()
        };
        let context_window = self.hop_envelope_window(hop_model);
        crate::agent::context_orch::prepare_turn_history(
            &mut chat_hist,
            crate::agent::context_orch::PrepareHistoryOpts {
                layered: self.config.envelope_assemble,
                compact_context: self.config.compact_context,
                async_pool: self.config.envelope_assemble_async,
                max_history: self.config.max_history_messages,
                extra_chunks: &extra_chunks,
                context_window,
                summarizer: Some(&summarizer),
            },
        )
        .await
        .map_err(|err| {
            crate::agent::envelope_pilot::annotate_hop_budget_error(
                err,
                hop_model.unwrap_or(&self.model_name),
                context_window,
            )
        })?;
        self.history = reintegrate_prepared_chat(&self.history, chat_hist, original_chat_count);
        self.trim_conversation_cap();
        Ok(())
    }

    fn hop_envelope_window(&self, hop_model: Option<&str>) -> Option<u32> {
        #[cfg(feature = "ai-protocol")]
        {
            let routes = self
                .intent_route_host
                .as_ref()
                .map(|h| h.model_routes.as_slice())
                .unwrap_or(&[]);
            if let Some(hop) = hop_model {
                crate::protocol_registry::lookup_hop_context_window(hop, routes)
            } else {
                crate::protocol_registry::lookup_context_window(&self.model_name)
            }
        }
        #[cfg(not(feature = "ai-protocol"))]
        {
            let _ = hop_model;
            None
        }
    }

    fn build_system_prompt(&self) -> Result<String> {
        let instructions = self.tool_dispatcher.prompt_instructions(&self.tools);
        let ctx = PromptContext {
            workspace_dir: &self.workspace_dir,
            model_name: &self.model_name,
            tools: &self.tools,
            skills: &self.skills,
            skills_prompt_mode: self.skills_prompt_mode,
            identity_config: Some(&self.identity_config),
            dispatcher_instructions: &instructions,
            prompt_mode: crate::agent::prompt_composer::PromptMode::Full,
            compact_tool_catalog: false,
        };
        let mut prompt = self.prompt_builder.build(&ctx)?;
        if let Some(note) = self.host_phase.system_note() {
            prompt.push_str("\n\n");
            prompt.push_str(note);
        }
        Ok(prompt)
    }

    fn build_work_node_system_prompt(&self) -> Result<String> {
        let instructions = self.tool_dispatcher.prompt_instructions(&self.tools);
        let ctx = PromptContext {
            workspace_dir: &self.workspace_dir,
            model_name: &self.model_name,
            tools: &self.tools,
            skills: &self.skills,
            skills_prompt_mode: crate::config::SkillsPromptInjectionMode::Compact,
            identity_config: Some(&self.identity_config),
            dispatcher_instructions: &instructions,
            prompt_mode: crate::agent::prompt_composer::PromptMode::Minimal,
            compact_tool_catalog: true,
        };
        let mut prompt = self.prompt_builder.build(&ctx)?;
        if let Some(note) = self.host_phase.system_note() {
            prompt.push_str("\n\n");
            prompt.push_str(note);
        }
        Ok(prompt)
    }

    fn classify_model(&self, user_message: &str) -> Result<String> {
        #[cfg(feature = "ai-protocol")]
        {
            let req = crate::orchestration::TurnModelRequest {
                user_message,
                session_key: self.session_id.as_str(),
                default_model: self.model_name.as_str(),
                explicit_model: self.explicit_model.as_deref(),
                host_decide: self.host_decide_host.as_ref(),
                intent_route: self.intent_route_host.as_ref(),
                classification: &self.classification_config,
                available_hints: &self.available_hints,
            };
            let decision = crate::orchestration::resolve_turn_model(&req)?;
            Ok(decision.model)
        }
        #[cfg(not(feature = "ai-protocol"))]
        {
            let _ = user_message;
            Ok(super::classifier::resolve_model_for_message(
                &self.classification_config,
                &self.available_hints,
                &self.model_name,
                user_message,
            ))
        }
    }

    /// Resolve turn model and remember the decision for observe/UX.
    fn classify_model_tracked(&mut self, user_message: &str) -> Result<String> {
        #[cfg(feature = "ai-protocol")]
        {
            let req = crate::orchestration::TurnModelRequest {
                user_message,
                session_key: self.session_id.as_str(),
                default_model: self.model_name.as_str(),
                explicit_model: self.explicit_model.as_deref(),
                host_decide: self.host_decide_host.as_ref(),
                intent_route: self.intent_route_host.as_ref(),
                classification: &self.classification_config,
                available_hints: &self.available_hints,
            };
            let decision = crate::orchestration::resolve_turn_model(&req)?;
            let model = decision.model.clone();
            self.last_turn_model = Some(decision);
            Ok(model)
        }
        #[cfg(not(feature = "ai-protocol"))]
        self.classify_model(user_message)
    }

    /// Ensure the system prompt is the first history entry (for Web UI history seeding).
    pub fn ensure_system_prompt(&mut self) -> Result<()> {
        if self.history.is_empty() {
            let system_prompt = self.build_system_prompt()?;
            self.history
                .push(ConversationMessage::Chat(ChatMessage::system(
                    system_prompt,
                )));
        }
        Ok(())
    }

    /// Append a chat message to history (used when seeding prior Web UI turns).
    pub fn push_chat_message(&mut self, message: ChatMessage) {
        self.history.push(ConversationMessage::Chat(message));
    }

    pub async fn turn(&mut self, user_message: &str) -> Result<String> {
        self.ensure_system_prompt()?;

        if self.auto_save {
            if let Err(err) = self
                .memory
                .store(
                    "user_msg",
                    user_message,
                    MemoryCategory::Conversation,
                    Some(self.session_id.as_str()),
                )
                .await
            {
                tracing::warn!(error = %err, "auto_save failed to store user message");
            }
        }

        let context = self
            .memory_loader
            .load_context(
                self.memory.as_ref(),
                user_message,
                Some(self.session_id.as_str()),
            )
            .await
            .unwrap_or_default();

        let enriched = if context.is_empty() {
            user_message.to_string()
        } else {
            format!("{context}{user_message}")
        };

        self.history
            .push(ConversationMessage::Chat(ChatMessage::user(
                enriched.clone(),
            )));

        #[cfg(feature = "ai-protocol")]
        let skip_pre_envelope = crate::agent::bounded_dag_live::skip_session_prepare_for_live(
            self.config.bounded_dag_live,
            self.host_phase,
        );
        #[cfg(not(feature = "ai-protocol"))]
        let skip_pre_envelope = false;
        if !skip_pre_envelope {
            self.prepare_conversation_history().await?;
        }

        #[cfg(feature = "ai-protocol")]
        let hop = if self.config.bounded_dag_live {
            let hop_history: Vec<ChatMessage> = self
                .history
                .iter()
                .filter_map(|m| match m {
                    ConversationMessage::Chat(c) => Some(c.clone()),
                    _ => None,
                })
                .collect();
            crate::agent::bounded_dag_live::live_first_hop(
                &self.config,
                self.memory.as_ref(),
                self.session_id.as_str(),
                self.provider.as_ref(),
                &self.model_name,
                user_message,
                &hop_history,
                self.temperature,
                self.host_phase,
            )
            .await?
        } else {
            crate::agent::bounded_dag_live::LiveFirstHop::SingleWork
        };
        #[cfg(feature = "ai-protocol")]
        let (mut use_live_dag, planned_from_first, chat_only_reply) = {
            use crate::agent::bounded_dag_live::LiveFirstHop;
            match hop {
                LiveFirstHop::Plan(plan) => (true, Some(plan), None),
                LiveFirstHop::ChatOnly { reply } => (false, None, Some(reply)),
                LiveFirstHop::SingleWork => (false, None, None),
            }
        };
        #[cfg(not(feature = "ai-protocol"))]
        let use_live_dag = false;

        #[cfg(feature = "ai-protocol")]
        if self.config.bounded_dag_live && !use_live_dag {
            use crate::agent::bounded_dag_live::{observe_turn_outcome, ObserveVerdict};
            self.note_session_default("live_first_hop");
            let text = if let Some(reply) = chat_only_reply {
                reply
            } else {
                self.invoke_tool_loop_resolved_with(self.model_name.clone(), true)
                    .await?
            };
            if crate::agent::bounded_dag_delivery::hop_body_closes_graph(&text) {
                let visible =
                    crate::agent::bounded_dag_delivery::ensure_user_visible(user_message, &text);
                self.history
                    .push(ConversationMessage::Chat(ChatMessage::assistant(&visible)));
                self.prepare_history_after_turn().await?;
                return Ok(visible);
            }
            let verdict = observe_turn_outcome(
                self.provider.as_ref(),
                &self.model_name,
                self.temperature,
                user_message,
                &text,
                None,
                0,
                0,
                ObserveVerdict::Continue,
            )
            .await?;
            if verdict == ObserveVerdict::ReplanRemaining {
                tracing::info!(
                    target: "bounded_dag_live",
                    "observe upgraded turn to plan_dag"
                );
                use_live_dag = true;
            } else {
                self.history
                    .push(ConversationMessage::Chat(ChatMessage::assistant(&text)));
                self.prepare_history_after_turn().await?;
                return Ok(text);
            }
        }

        #[cfg(feature = "ai-protocol")]
        if use_live_dag {
            use crate::agent::bounded_dag_live::{
                format_work_node_stop, live_dag_progress, work_node_user_task,
            };
            use crate::agent::host_phase::HostPhase;
            use crate::agent::loop_::is_tool_loop_cancelled;
            use std::collections::HashSet;
            let mut planned = if let Some(plan) = planned_from_first {
                self.note_session_default("live_first_hop_plan");
                plan
            } else {
                self.resolve_live_dag(user_message).await?
            };
            if self.host_phase == HostPhase::Plan {
                return Ok(planned.preview_with_contact(&self.model_name, &self.available_hints));
            }
            let dag_id = planned.dag.id.clone();
            let mut node_count = planned.order.len();
            let mut outline = planned.brief_outline(user_message);
            let mut chat_hist: Vec<ChatMessage> = self
                .history
                .iter()
                .filter_map(|m| match m {
                    ConversationMessage::Chat(c) => Some(c.clone()),
                    _ => None,
                })
                .collect();
            let mut graph_task = match &planned.graph_task_override {
                Some(task) => task.clone(),
                None => {
                    work_node_user_task(
                        self.memory.as_ref(),
                        self.session_id.as_str(),
                        &chat_hist,
                        user_message,
                    )
                    .await
                }
            };
            let mut prior: Vec<String> = Vec::new();
            let mut completed: HashSet<String> = HashSet::new();
            for id in planned.order.iter().take(planned.resume_from) {
                prior.push(id.clone());
                completed.insert(id.clone());
            }
            let mut last_body = String::new();
            let mut operator_prefix = String::new();
            self.hop_probes.clear();
            self.current_hop_probe = None;
            let contacts = self.dag_contact_labels(&planned.dag, &planned.order);
            self.emit_turn_progress(live_dag_progress(
                &dag_id,
                planned.used_fallback,
                &outline,
                &planned.dag,
                &planned.order,
                None,
                &completed,
                None,
                Some(&contacts),
            ));
            self.push_operator_note(
                &mut operator_prefix,
                &crate::agent::bounded_dag_live::operator_plan_gist(
                    user_message,
                    &planned.dag,
                    &planned.order,
                    planned.used_fallback,
                ),
            );
            let work_sys = self.build_work_node_system_prompt()?;
            let mut force_default = false;
            let mut auto_used = false;
            let mut index = planned.resume_from;
            while index < planned.order.len() {
                let id = planned.order[index].clone();
                let node = planned
                    .dag
                    .nodes
                    .iter()
                    .find(|n| n.id == id)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("bounded DAG missing node {id}"))?;
                let mut retrieve = crate::agent::bounded_dag_context::node_retrieve_texts(
                    self.memory.as_ref(),
                    &self.workspace_dir,
                    self.session_id.as_str(),
                    &node,
                    &prior,
                )
                .await;
                let _ = crate::agent::bounded_dag_context::ensure_graph_scratch(
                    &self.workspace_dir,
                    self.session_id.as_str(),
                    dag_id.as_str(),
                );
                let scratch = crate::agent::bounded_dag_context::graph_scratch_rel(
                    self.session_id.as_str(),
                    dag_id.as_str(),
                );
                self.security.set_graph_scratch_rel(Some(scratch));
                if let Some(listing) = crate::agent::bounded_dag_context::scratch_retrieve_text(
                    &self.workspace_dir,
                    self.session_id.as_str(),
                    dag_id.as_str(),
                ) {
                    retrieve.push(listing);
                }
                if let Ok(Some(fail)) = crate::agent::bounded_dag_live::load_dag_fail(
                    self.memory.as_ref(),
                    self.session_id.as_str(),
                )
                .await
                {
                    retrieve.push(crate::agent::bounded_dag_context::cached_fail_block(&fail));
                }
                let contact = crate::agent::bounded_dag_context::contact_for_live_node(
                    &node,
                    self.session_work_model(),
                    &self.available_hints,
                    force_default,
                );
                crate::agent::bounded_dag_context::reset_chat_scope(
                    &mut chat_hist,
                    &crate::agent::bounded_dag_context::NodeWorkPacket {
                        dag_id: dag_id.as_str(),
                        node: &node,
                        index: index + 1,
                        node_count,
                        user_task: &graph_task,
                        retrieve_texts: &retrieve,
                    },
                    Some(work_sys.as_str()),
                );
                self.history = chat_hist
                    .iter()
                    .cloned()
                    .map(ConversationMessage::Chat)
                    .collect();
                self.prepare_conversation_history_inner(false, Some(contact.model.as_str()))
                    .await?;
                self.last_turn_model = Some(contact.to_turn_decision());
                let probe_rc = self
                    .hop_probes
                    .entry(id.clone())
                    .or_insert_with(|| {
                        Arc::new(std::sync::Mutex::new(
                            crate::agent::probe_dedup::HopProbeGovernor::new(),
                        ))
                    })
                    .clone();
                self.current_hop_probe = Some(probe_rc.clone());
                let contacts = self.dag_contact_labels(&planned.dag, &planned.order);
                self.emit_turn_progress(live_dag_progress(
                    &dag_id,
                    planned.used_fallback,
                    &outline,
                    &planned.dag,
                    &planned.order,
                    Some(id.as_str()),
                    &completed,
                    None,
                    Some(&contacts),
                ));
                let text = match self.invoke_tool_loop_resolved(contact.model.clone()).await {
                    Ok(text) => {
                        self.flush_hop_probe_notices(&probe_rc, &mut operator_prefix);
                        let close = probe_rc.lock().map(|g| g.hop_close()).unwrap_or_default();
                        if close == crate::agent::hop_stop::HopClose::PolicyDeny {
                            let _ = crate::agent::bounded_dag_live::store_dag_fail(
                                self.memory.as_ref(),
                                self.session_id.as_str(),
                                &crate::agent::bounded_dag_live::policy_deny_fail_cursor(
                                    &node.id, index, &dag_id,
                                ),
                            )
                            .await;
                            let stop = format_work_node_stop(
                                user_message,
                                &node.id,
                                "repeated policy denials of the same class",
                                index + 1,
                                node_count,
                            );
                            self.push_operator_note(&mut operator_prefix, &stop);
                            self.end_live_graph_host_state();
                            return Ok(operator_prefix);
                        }
                        text
                    }
                    Err(err) if is_tool_loop_cancelled(&err) => {
                        let _ = crate::agent::bounded_dag_live::store_dag_fail(
                            self.memory.as_ref(),
                            self.session_id.as_str(),
                            &crate::agent::bounded_dag_live::cancelled_fail_cursor(
                                &node.id, index, &dag_id,
                            ),
                        )
                        .await;
                        self.current_hop_probe = None;
                        self.security.set_graph_scratch_rel(None);
                        return Err(err);
                    }
                    Err(err) => {
                        self.flush_hop_probe_notices(&probe_rc, &mut operator_prefix);
                        let err_s = format!("{err:#}");
                        let class = crate::providers::hint_peer::classify_hop_error(&err_s);
                        match crate::agent::bounded_dag_live::decide_work_node_fail(
                            self.config.dag_fail_auto_replan,
                            auto_used,
                            &err_s,
                        ) {
                            crate::agent::bounded_dag_live::WorkNodeFailDecision::RetrySame {
                                force_default: fd,
                            } => {
                                tracing::warn!(
                                    node = %node.id,
                                    class = class.as_str(),
                                    "dag_fail_auto_replan: retrying node"
                                );
                                auto_used = true;
                                force_default = force_default || fd;
                                let _ = crate::agent::bounded_dag_live::store_dag_fail(
                                    self.memory.as_ref(),
                                    self.session_id.as_str(),
                                    &crate::agent::bounded_dag_live::DagFailCursor {
                                        node_id: node.id.clone(),
                                        index,
                                        err: err_s,
                                        dag_id: dag_id.clone(),
                                        auto_replan_count: 1,
                                        fail_class: class.as_str().into(),
                                    },
                                )
                                .await;
                                continue;
                            }
                            crate::agent::bounded_dag_live::WorkNodeFailDecision::Stop => {
                                let contacts =
                                    self.dag_contact_labels(&planned.dag, &planned.order);
                                self.emit_turn_progress(live_dag_progress(
                                    &dag_id,
                                    planned.used_fallback,
                                    &outline,
                                    &planned.dag,
                                    &planned.order,
                                    None,
                                    &completed,
                                    Some(id.as_str()),
                                    Some(&contacts),
                                ));
                                let stop = format_work_node_stop(
                                    user_message,
                                    &node.id,
                                    &err_s,
                                    index + 1,
                                    node_count,
                                );
                                let _ = crate::agent::bounded_dag_live::store_dag_fail(
                                    self.memory.as_ref(),
                                    self.session_id.as_str(),
                                    &crate::agent::bounded_dag_live::DagFailCursor {
                                        node_id: node.id.clone(),
                                        index,
                                        err: err_s,
                                        dag_id: dag_id.clone(),
                                        auto_replan_count: u32::from(auto_used),
                                        fail_class: class.as_str().into(),
                                    },
                                )
                                .await;
                                self.push_operator_note(&mut operator_prefix, &stop);
                                self.end_live_graph_host_state();
                                return Ok(operator_prefix);
                            }
                        }
                    }
                };
                if let Err(err) = crate::agent::bounded_dag_context::store_node_artifact(
                    self.memory.as_ref(),
                    self.session_id.as_str(),
                    &node.id,
                    &text,
                )
                .await
                {
                    tracing::debug!(error = %err, node = %node.id, "bounded DAG artifact store skipped");
                }
                completed.insert(node.id.clone());
                last_body = text;
                prior.push(node.id.clone());
                chat_hist = self
                    .history
                    .iter()
                    .filter_map(|m| match m {
                        ConversationMessage::Chat(c) => Some(c.clone()),
                        _ => None,
                    })
                    .collect();
                let contacts = self.dag_contact_labels(&planned.dag, &planned.order);
                self.emit_turn_progress(live_dag_progress(
                    &dag_id,
                    planned.used_fallback,
                    &outline,
                    &planned.dag,
                    &planned.order,
                    None,
                    &completed,
                    None,
                    Some(&contacts),
                ));
                let remaining = planned.order.len().saturating_sub(index + 1);
                if crate::agent::bounded_dag_delivery::should_emit_mid_hop_note(remaining) {
                    self.push_operator_note(
                        &mut operator_prefix,
                        &crate::agent::bounded_dag_delivery::mid_hop_operator_note(
                            &graph_task,
                            &id,
                            &last_body,
                            None,
                        ),
                    );
                }
                if crate::agent::bounded_dag_delivery::last_hop_ends_graph(remaining)
                    || crate::agent::bounded_dag_delivery::hop_body_closes_graph(&last_body)
                {
                    let _ = crate::agent::bounded_dag_live::clear_dag_fail(
                        self.memory.as_ref(),
                        self.session_id.as_str(),
                    )
                    .await;
                    self.end_live_graph_host_state();
                    return self
                        .parlor_live_reply(&graph_task, &last_body, &operator_prefix)
                        .await;
                }
                let verdict = crate::agent::bounded_dag_live::observe_turn_outcome(
                    self.provider.as_ref(),
                    &self.model_name,
                    self.temperature,
                    user_message,
                    &last_body,
                    Some(id.as_str()),
                    remaining,
                    node_count,
                    crate::agent::bounded_dag_live::ObserveVerdict::Continue,
                )
                .await?;
                match verdict {
                    crate::agent::bounded_dag_live::ObserveVerdict::Stop => {
                        let _ = crate::agent::bounded_dag_live::clear_dag_fail(
                            self.memory.as_ref(),
                            self.session_id.as_str(),
                        )
                        .await;
                        self.end_live_graph_host_state();
                        return self
                            .parlor_live_reply(&graph_task, &last_body, &operator_prefix)
                            .await;
                    }
                    crate::agent::bounded_dag_live::ObserveVerdict::ReplanRemaining
                        if remaining > 0 && !auto_used =>
                    {
                        auto_used = true;
                        if let Some(spliced) =
                            crate::agent::bounded_dag_live::replan_remaining_after_observe(
                                &self.config,
                                self.memory.as_ref(),
                                self.session_id.as_str(),
                                self.provider.as_ref(),
                                &self.model_name,
                                self.temperature,
                                &planned,
                                &id,
                                index,
                                user_message,
                                "observe_off_goal",
                            )
                            .await?
                        {
                            planned = spliced;
                            node_count = planned.order.len();
                            outline = planned.brief_outline(user_message);
                            if let Some(task) = &planned.graph_task_override {
                                graph_task = task.clone();
                            }
                            index = planned.resume_from;
                            continue;
                        }
                        index += 1;
                    }
                    _ => {
                        index += 1;
                    }
                }
            }
            let _ = crate::agent::bounded_dag_live::clear_dag_fail(
                self.memory.as_ref(),
                self.session_id.as_str(),
            )
            .await;
            let raw = if last_body.is_empty() {
                outline
            } else {
                last_body
            };
            self.end_live_graph_host_state();
            return self
                .parlor_live_reply(&graph_task, &raw, &operator_prefix)
                .await;
        }

        self.invoke_tool_loop_once(user_message).await
    }

    #[cfg(feature = "ai-protocol")]
    async fn parlor_live_reply(
        &self,
        user_task: &str,
        last_body: &str,
        prefix: &str,
    ) -> Result<String> {
        let hist: Vec<&str> = self
            .history
            .iter()
            .filter_map(|m| match m {
                ConversationMessage::Chat(c) if c.role == "assistant" => Some(c.content.as_str()),
                _ => None,
            })
            .collect();
        let prior = crate::agent::bounded_dag_delivery::collect_prior_exclusivity(
            std::iter::once(prefix).chain(hist),
        );
        let parlor = crate::agent::bounded_dag_delivery::host_delivery(
            self.provider.as_ref(),
            &self.model_name,
            self.temperature,
            user_task,
            last_body,
            &prior,
        )
        .await?;
        Ok(parlor)
    }

    #[cfg(feature = "ai-protocol")]
    fn note_session_default(&mut self, reason: &str) {
        self.last_turn_model = Some(crate::orchestration::TurnModelDecision {
            model: self.model_name.clone(),
            source: crate::orchestration::TurnModelSource::DefaultModel,
            reason: reason.into(),
        });
    }

    #[cfg(feature = "ai-protocol")]
    async fn resolve_live_dag(
        &mut self,
        user_message: &str,
    ) -> Result<crate::agent::bounded_dag_live::PlannedLiveDag> {
        use crate::agent::bounded_dag_live::prepare_session_live_dag;

        let planned = prepare_session_live_dag(
            &self.config,
            self.memory.as_ref(),
            self.session_id.as_str(),
            self.provider.as_ref(),
            &self.model_name,
            user_message,
            self.temperature,
            self.host_phase,
        )
        .await?;
        self.note_session_default("bounded_dag_planner_session_default");
        Ok(planned)
    }

    /// One `run_tool_call_loop` over current history (GOV-007 canonical body).
    async fn invoke_tool_loop_once(&mut self, user_message: &str) -> Result<String> {
        let effective_model = self.classify_model_tracked(user_message)?;
        self.invoke_tool_loop_resolved_with(effective_model, true)
            .await
    }

    async fn invoke_tool_loop_resolved(&mut self, effective_model: String) -> Result<String> {
        self.invoke_tool_loop_resolved_with(effective_model, true)
            .await
    }

    async fn invoke_tool_loop_resolved_with(
        &mut self,
        effective_model: String,
        allow_tools: bool,
    ) -> Result<String> {
        // VL-CTX-002 / GOV-007: single tool-iteration body (`run_tool_call_loop`).
        // ApprovalHub / HumanInputHub stay as backend adapters via gate_extras.
        let mut loop_history = self.tool_dispatcher.to_provider_messages(&self.history);
        let provider_name = crate::protocol_registry::provider_id_from_logical(&effective_model);
        #[cfg(feature = "ai-protocol")]
        let text_tool_result_history = self
            .execution
            .as_ref()
            .map(|e| e.tool_calling_policy().native_strategy == ai_lib_rust::NativeStrategy::Hybrid)
            .unwrap_or(false);
        #[cfg(not(feature = "ai-protocol"))]
        let text_tool_result_history = !self.tool_dispatcher.should_send_tool_specs();

        let gate_extras = crate::agent::tool_batch::ToolBatchGateExtras {
            approval_hub: self
                .gateway_approval
                .as_ref()
                .map(|(_, hub)| Arc::clone(hub)),
            human_input_hub: self.human_input_hub.clone(),
            host_phase: self.host_phase,
        };
        let approval_mgr = self.gateway_approval.as_ref().map(|(mgr, _)| mgr);

        let soft_fail = crate::agent::loop_::SoftFailLoopCtx {
            session_key: self.session_id.as_str(),
            config: None,
            #[cfg(feature = "ai-protocol")]
            host_decide: self.host_decide_host.as_ref(),
            surface: velaclaw_agent_runtime::SoftFailSurface::Web,
            peer_logical_ids: &self.peer_logical_ids,
            model_routes: self.model_routes.as_slice(),
            session_model: self
                .explicit_model
                .as_deref()
                .or(Some(self.model_name.as_str())),
            probe: self.current_hop_probe.as_ref().map(|c| c.as_ref()),
        };

        let render_opts = self.cli_render.unwrap_or(RenderOpts {
            style: crate::cli_render::RenderStyle {
                ansi: false,
                markdown: true,
            },
            fold_lines: 10,
            fold_enabled: false,
        });

        let observer: Arc<dyn Observer> = if let Some(tx) = &self.progress_tx {
            Arc::new(crate::agent::turn_progress::ProgressObserver::forwarding(
                Arc::clone(&self.observer),
                tx.clone(),
            ))
        } else if self.cli_render.is_some() || self.cancellation_token.is_some() {
            Arc::new(crate::agent::turn_progress::ProgressObserver::cli(
                Arc::clone(&self.observer),
            ))
        } else {
            Arc::clone(&self.observer)
        };

        let empty_tools: Vec<Box<dyn crate::tools::Tool>> = Vec::new();
        let tools = if allow_tools {
            self.tools.as_slice()
        } else {
            empty_tools.as_slice()
        };
        let response = crate::agent::loop_::run_tool_call_loop(
            self.provider.as_ref(),
            &mut loop_history,
            tools,
            observer.as_ref(),
            provider_name,
            &effective_model,
            self.temperature,
            self.cli_render.is_none(),
            approval_mgr,
            "web",
            &crate::config::MultimodalConfig::default(),
            self.config.max_tool_iterations,
            self.cancellation_token.clone(),
            None,
            Some(self.tool_dispatcher.as_ref()),
            Some(&self.security),
            None,
            text_tool_result_history,
            render_opts,
            None,
            Some(soft_fail),
            Some(&gate_extras),
        )
        .await?;

        // Restore structured ConversationMessage variants (GOV-007 / VL-CTX-002).
        // `run_tool_call_loop` mutates provider-shaped Chat frames; blanket
        // `Chat(...)` mapping would collapse AssistantToolCalls / ToolResults.
        self.history = conversation_from_tool_loop_history(&loop_history);
        self.prepare_history_after_turn().await?;
        Ok(response)
    }

    pub async fn run_single(&mut self, message: &str) -> Result<String> {
        self.turn(message).await
    }

    pub async fn run_interactive(&mut self) -> Result<()> {
        let render_opts = self
            .cli_render
            .unwrap_or_else(RenderOpts::interactive_default);
        self.cli_render = Some(render_opts);

        println!("🦀 VelaClaw Interactive Mode");
        println!("Type /quit to exit. During a turn, press Esc twice to stop.\n");

        let (tx, mut rx) = tokio::sync::mpsc::channel(32);
        let cli = crate::channels::CliChannel::with_render_opts(render_opts);

        let listen_handle = tokio::spawn(async move {
            let _ = crate::channels::Channel::listen(&cli, tx).await;
        });

        while let Some(msg) = rx.recv().await {
            let cancel = crate::agent::turn_cancel::CliTurnCancel::begin();
            self.set_cancellation_token(Some(cancel.token()));
            let result = self.turn(&msg.content).await;
            self.set_cancellation_token(None);
            let response = match cancel.conclude(result).await {
                crate::agent::turn_cancel::TurnFinish::Completed(resp) => resp,
                crate::agent::turn_cancel::TurnFinish::Cancelled => {
                    eprintln!("{}\n", crate::agent::turn_cancel::STOPPED_USER_MESSAGE);
                    continue;
                }
                crate::agent::turn_cancel::TurnFinish::Failed(e) => {
                    eprintln!("\nError: {e}\n");
                    continue;
                }
            };
            let formatted = self.format_agent_output(&response);
            println!("\n{formatted}\n");
        }

        listen_handle.abort();
        Ok(())
    }
}

pub async fn run(
    config: Config,
    message: Option<String>,
    provider_override: Option<String>,
    model_override: Option<String>,
    temperature: f64,
) -> Result<()> {
    let start = Instant::now();

    let mut effective_config = config;
    if let Some(p) = provider_override {
        effective_config.default_provider = Some(p);
    }
    if let Some(m) = model_override {
        effective_config.default_model = Some(m);
    }
    effective_config.default_temperature = temperature;

    let mut agent = Agent::from_config(&effective_config)?;

    let provider_name = effective_config
        .default_provider
        .as_deref()
        .unwrap_or(DEFAULT_PROTOCOL_MODEL_ID)
        .to_string();
    let model_name = effective_config
        .default_model
        .as_deref()
        .unwrap_or(DEFAULT_PROTOCOL_MODEL_ID)
        .to_string();

    agent.observer.record_event(&ObserverEvent::AgentStart {
        provider: provider_name.clone(),
        model: model_name.clone(),
    });

    let render_opts = RenderOpts::from_config(
        effective_config.cli_render.as_ref(),
        false,
        false,
        message.is_none(),
    );
    agent.set_cli_render(render_opts);

    if let Some(msg) = message {
        let response = agent.run_single(&msg).await?;
        println!("{}", agent.format_agent_output(&response));
    } else {
        agent.run_interactive().await?;
    }

    agent.observer.record_event(&ObserverEvent::AgentEnd {
        provider: provider_name,
        model: model_name,
        duration: start.elapsed(),
        tokens_used: None,
        cost_usd: None,
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::dispatcher::{NativeToolDispatcher, XmlToolDispatcher};
    use crate::providers::ChatRequest;
    use crate::security::SecurityPolicy;
    use crate::tools::ToolExecutionContext;
    use async_trait::async_trait;
    use parking_lot::Mutex;

    struct MockProvider {
        responses: Mutex<Vec<crate::providers::ChatResponse>>,
    }

    #[async_trait]
    impl Provider for MockProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> Result<String> {
            Ok("ok".into())
        }

        async fn chat(
            &self,
            _request: ChatRequest<'_>,
            _model: &str,
            _temperature: f64,
        ) -> Result<crate::providers::ChatResponse> {
            let mut guard = self.responses.lock();
            if guard.is_empty() {
                return Ok(crate::providers::ChatResponse {
                    text: Some("done".into()),
                    tool_calls: vec![],
                });
            }
            Ok(guard.remove(0))
        }
    }

    struct MockTool;

    #[async_trait]
    impl Tool for MockTool {
        fn name(&self) -> &str {
            "echo"
        }

        fn description(&self) -> &str {
            "echo"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolExecutionContext,
        ) -> Result<crate::tools::ToolResult> {
            Ok(crate::tools::ToolResult {
                success: true,
                output: "tool-out".into(),
                error: None,
            })
        }
    }

    fn test_security() -> PolicyHandle {
        PolicyHandle::new(SecurityPolicy::from_config(
            &crate::config::AutonomyConfig::default(),
            &std::path::PathBuf::from("/tmp"),
        ))
    }

    #[tokio::test]
    async fn turn_without_tools_returns_text() {
        let provider = Box::new(MockProvider {
            responses: Mutex::new(vec![crate::providers::ChatResponse {
                text: Some("hello".into()),
                tool_calls: vec![],
            }]),
        });

        let memory_cfg = crate::config::MemoryConfig {
            backend: "none".into(),
            ..crate::config::MemoryConfig::default()
        };
        let mem: Arc<dyn Memory> = Arc::from(
            crate::memory::create_memory(&memory_cfg, std::path::Path::new("/tmp"), None)
                .expect("memory creation should succeed with valid config"),
        );

        let observer: Arc<dyn Observer> = Arc::from(crate::observability::NoopObserver {});
        let mut agent = Agent::builder()
            .provider(provider)
            .tools(vec![Box::new(MockTool)])
            .memory(mem)
            .observer(observer)
            .tool_dispatcher(Box::new(XmlToolDispatcher::default()))
            .workspace_dir(std::path::PathBuf::from("/tmp"))
            .security(test_security())
            .build()
            .expect("agent builder should succeed with valid config");

        let response = agent.turn("hi").await.unwrap();
        assert_eq!(response, "hello");
    }

    #[tokio::test]
    async fn turn_with_native_dispatcher_handles_tool_results_variant() {
        let provider = Box::new(MockProvider {
            responses: Mutex::new(vec![
                crate::providers::ChatResponse {
                    text: Some(String::new()),
                    tool_calls: vec![crate::providers::ToolCall {
                        id: "tc1".into(),
                        name: "echo".into(),
                        arguments: "{}".into(),
                    }],
                },
                crate::providers::ChatResponse {
                    text: Some("done".into()),
                    tool_calls: vec![],
                },
            ]),
        });

        let memory_cfg = crate::config::MemoryConfig {
            backend: "none".into(),
            ..crate::config::MemoryConfig::default()
        };
        let mem: Arc<dyn Memory> = Arc::from(
            crate::memory::create_memory(&memory_cfg, std::path::Path::new("/tmp"), None)
                .expect("memory creation should succeed with valid config"),
        );

        let observer: Arc<dyn Observer> = Arc::from(crate::observability::NoopObserver {});
        let mut agent = Agent::builder()
            .provider(provider)
            .tools(vec![Box::new(MockTool)])
            .memory(mem)
            .observer(observer)
            .tool_dispatcher(Box::new(NativeToolDispatcher::default()))
            .workspace_dir(std::path::PathBuf::from("/tmp"))
            .security(test_security())
            .build()
            .expect("agent builder should succeed with valid config");

        let response = agent.turn("hi").await.unwrap();
        assert_eq!(response, "done");
        assert!(agent
            .history()
            .iter()
            .any(|msg| matches!(msg, ConversationMessage::ToolResults(_))));
    }

    #[tokio::test]
    async fn turn_repairs_unparsed_tool_markup_into_ir() {
        let bad = "<tool_call>\nNOT_JSON\n</tool_call>";
        let repair = r#"[{"name":"echo","arguments":{}}]"#;
        let provider = Box::new(MockProvider {
            responses: Mutex::new(vec![
                crate::providers::ChatResponse {
                    text: Some(bad.into()),
                    tool_calls: vec![],
                },
                crate::providers::ChatResponse {
                    text: Some(repair.into()),
                    tool_calls: vec![],
                },
                crate::providers::ChatResponse {
                    text: Some("done".into()),
                    tool_calls: vec![],
                },
            ]),
        });

        let memory_cfg = crate::config::MemoryConfig {
            backend: "none".into(),
            ..crate::config::MemoryConfig::default()
        };
        let mem: Arc<dyn Memory> = Arc::from(
            crate::memory::create_memory(&memory_cfg, std::path::Path::new("/tmp"), None)
                .expect("memory creation should succeed with valid config"),
        );

        let observer: Arc<dyn Observer> = Arc::from(crate::observability::NoopObserver {});
        let mut agent = Agent::builder()
            .provider(provider)
            .tools(vec![Box::new(MockTool)])
            .memory(mem)
            .observer(observer)
            .tool_dispatcher(Box::new(NativeToolDispatcher::default()))
            .workspace_dir(std::path::PathBuf::from("/tmp"))
            .security(test_security())
            .build()
            .expect("agent builder should succeed with valid config");

        let response = agent.turn("hi").await.unwrap();
        assert_eq!(response, "done");
        assert!(agent
            .history()
            .iter()
            .any(|msg| matches!(msg, ConversationMessage::ToolResults(_))));
        assert!(!agent.history().iter().any(|msg| matches!(
            msg,
            ConversationMessage::Chat(m) if m.role == "user" && m.content.contains("invalid format")
        )));
    }

    #[tokio::test]
    async fn turn_strips_markup_when_repair_empty() {
        let bad = "<tool_call>\nNOT_JSON\n</tool_call>";
        let provider = Box::new(MockProvider {
            responses: Mutex::new(vec![
                crate::providers::ChatResponse {
                    text: Some(bad.into()),
                    tool_calls: vec![],
                },
                crate::providers::ChatResponse {
                    text: Some("[]".into()),
                    tool_calls: vec![],
                },
            ]),
        });

        let memory_cfg = crate::config::MemoryConfig {
            backend: "none".into(),
            ..crate::config::MemoryConfig::default()
        };
        let mem: Arc<dyn Memory> = Arc::from(
            crate::memory::create_memory(&memory_cfg, std::path::Path::new("/tmp"), None)
                .expect("memory"),
        );
        let observer: Arc<dyn Observer> = Arc::from(crate::observability::NoopObserver {});
        let mut agent = Agent::builder()
            .provider(provider)
            .tools(vec![Box::new(MockTool)])
            .memory(mem)
            .observer(observer)
            .tool_dispatcher(Box::new(XmlToolDispatcher::default()))
            .workspace_dir(std::path::PathBuf::from("/tmp"))
            .security(test_security())
            .build()
            .expect("agent");

        let response = agent.turn("hi").await.unwrap();
        assert!(!response.contains("<tool_call"));
        assert!(response.contains("tool-format recovery exhausted"));
        let hist = agent.history();
        assert!(!hist.iter().any(|msg| matches!(
            msg,
            ConversationMessage::Chat(m) if m.role == "user" && m.content.contains("invalid format")
        )));
    }

    #[tokio::test]
    async fn turn_peer_continues_after_repair_miss() {
        let bad = "<tool_call>\nNOT_JSON\n</tool_call>";
        let tool_xml = "<tool_call>\n{\"name\":\"echo\",\"arguments\":{}}\n</tool_call>";
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        struct PeerMock {
            responses: Mutex<Vec<crate::providers::ChatResponse>>,
            seen: Arc<Mutex<Vec<String>>>,
        }
        #[async_trait]
        impl Provider for PeerMock {
            async fn chat_with_system(
                &self,
                _system_prompt: Option<&str>,
                _message: &str,
                _model: &str,
                _temperature: f64,
            ) -> Result<String> {
                Ok("ok".into())
            }
            async fn chat(
                &self,
                _request: ChatRequest<'_>,
                model: &str,
                _temperature: f64,
            ) -> Result<crate::providers::ChatResponse> {
                self.seen.lock().push(model.to_string());
                let mut guard = self.responses.lock();
                if guard.is_empty() {
                    return Ok(crate::providers::ChatResponse {
                        text: Some("done".into()),
                        tool_calls: vec![],
                    });
                }
                Ok(guard.remove(0))
            }
        }

        let provider = Box::new(PeerMock {
            responses: Mutex::new(vec![
                crate::providers::ChatResponse {
                    text: Some(bad.into()),
                    tool_calls: vec![],
                },
                crate::providers::ChatResponse {
                    text: Some("[]".into()),
                    tool_calls: vec![],
                },
                crate::providers::ChatResponse {
                    text: Some(tool_xml.into()),
                    tool_calls: vec![],
                },
                crate::providers::ChatResponse {
                    text: Some("done".into()),
                    tool_calls: vec![],
                },
            ]),
            seen: Arc::clone(&seen),
        });
        let memory_cfg = crate::config::MemoryConfig {
            backend: "none".into(),
            ..crate::config::MemoryConfig::default()
        };
        let mem: Arc<dyn Memory> = Arc::from(
            crate::memory::create_memory(&memory_cfg, std::path::Path::new("/tmp"), None)
                .expect("memory"),
        );
        let observer: Arc<dyn Observer> = Arc::from(crate::observability::NoopObserver {});
        let mut agent = Agent::builder()
            .provider(provider)
            .tools(vec![Box::new(MockTool)])
            .memory(mem)
            .observer(observer)
            .tool_dispatcher(Box::new(XmlToolDispatcher::default()))
            .workspace_dir(std::path::PathBuf::from("/tmp"))
            .security(test_security())
            .peer_logical_ids(vec!["peer/test-model".into()])
            .build()
            .expect("agent");

        let response = agent.turn("hi").await.unwrap();
        assert_eq!(response, "done");
        let models = seen.lock().clone();
        assert!(
            models.iter().any(|m| m == "peer/test-model"),
            "expected peer hop, saw {models:?}"
        );
        assert!(!response.contains("tool-format recovery exhausted"));
    }

    #[test]
    fn format_agent_output_prefixes_lines_when_cli_render_set() {
        let provider = Box::new(MockProvider {
            responses: Mutex::new(vec![]),
        });
        let memory_cfg = crate::config::MemoryConfig {
            backend: "none".into(),
            ..crate::config::MemoryConfig::default()
        };
        let mem: Arc<dyn Memory> = Arc::from(
            crate::memory::create_memory(&memory_cfg, std::path::Path::new("/tmp"), None)
                .expect("memory"),
        );
        let observer: Arc<dyn Observer> = Arc::from(crate::observability::NoopObserver {});
        let mut agent = Agent::builder()
            .provider(provider)
            .tools(vec![Box::new(MockTool)])
            .memory(mem)
            .observer(observer)
            .tool_dispatcher(Box::new(XmlToolDispatcher::default()))
            .workspace_dir(std::path::PathBuf::from("/tmp"))
            .security(test_security())
            .build()
            .expect("agent");
        agent.set_cli_render(RenderOpts {
            style: crate::cli_render::RenderStyle {
                ansi: false,
                markdown: false,
            },
            fold_lines: 10,
            fold_enabled: false,
        });
        let out = agent.format_agent_output("line one\nline two");
        assert!(out.starts_with(">> line one"));
        assert!(out.contains("\nline two"));
        assert!(!out.contains(">> line two"));
    }
}
