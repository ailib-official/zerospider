//! CLI command dispatch helpers extracted from `main` (VL-REVIEW-004).

use anyhow::{bail, Result};
use tracing::info;
use velaclaw::config::DEFAULT_PROTOCOL_MODEL_ID;
use velaclaw::{
    agent, channels, cron, daemon, doctor, gateway, hardware, integrations, memory, migration,
    onboard, peripherals, providers, service, skills, status, ChannelCommands, Config,
};

use crate::{deploy, handle_auth_command, Commands, ConfigCommands, DoctorCommands, ModelCommands};

/// Handle `velaclaw models ...` subcommands.
pub async fn handle_models_command(config: &Config, model_command: ModelCommands) -> Result<()> {
    match model_command {
        ModelCommands::Refresh { provider, force } => {
            let config_for_refresh = config.clone();
            tokio::task::spawn_blocking(move || {
                onboard::run_models_refresh(&config_for_refresh, provider.as_deref(), force)
            })
            .await
            .map_err(|e| anyhow::anyhow!("models refresh task failed: {e}"))?
        }
        #[cfg(feature = "ai-protocol")]
        ModelCommands::ProtocolProviders { json } => {
            use velaclaw::protocol_registry::{resolve_local_protocol_root, scan_protocol_root};
            let Some(root) = resolve_local_protocol_root() else {
                anyhow::bail!(
                    "Set AI_PROTOCOL_DIR to a local ai-protocol checkout (not a URL) to list manifests."
                );
            };
            let snap = scan_protocol_root(&root)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&snap)?);
            } else {
                println!("Protocol root: {}\n", snap.protocol_root.display());
                println!("{:<24} {:<6} REQUIRED_ENVS", "PROVIDER_ID", "OK");
                for p in &snap.providers {
                    println!(
                        "{:<24} {:<6} [{}]",
                        p.id,
                        if p.available { "yes" } else { "no" },
                        p.required_envs.join(", ")
                    );
                }
            }
            Ok(())
        }
        #[cfg(not(feature = "ai-protocol"))]
        ModelCommands::ProtocolProviders { .. } => {
            anyhow::bail!("Rebuild with --features ai-protocol to use this command.")
        }
        #[cfg(feature = "ai-protocol")]
        ModelCommands::ProtocolModels { json } => {
            use velaclaw::protocol_registry::{resolve_local_protocol_root, scan_protocol_root};
            let Some(root) = resolve_local_protocol_root() else {
                anyhow::bail!(
                    "Set AI_PROTOCOL_DIR to a local ai-protocol checkout (not a URL) to list models."
                );
            };
            let snap = scan_protocol_root(&root)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&snap.models)?);
            } else {
                println!("Models under {}:\n", root.display());
                println!("{:<40} PROVIDER", "LOGICAL_ID");
                for m in &snap.models {
                    println!("{:<40} {}", m.logical_id, m.provider);
                }
            }
            Ok(())
        }
        #[cfg(not(feature = "ai-protocol"))]
        ModelCommands::ProtocolModels { .. } => {
            anyhow::bail!("Rebuild with --features ai-protocol to use this command.")
        }
        #[cfg(feature = "ai-protocol")]
        ModelCommands::ProtocolGenerative {
            model,
            capability,
            json,
        } => {
            use velaclaw::protocol_registry::{
                inspect_generative_capability, resolve_local_protocol_root,
            };
            let Some(root) = resolve_local_protocol_root() else {
                anyhow::bail!("Set AI_PROTOCOL_DIR to a local ai-protocol checkout (not a URL).");
            };
            let info = inspect_generative_capability(&root, &model, &capability)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&info)?);
            } else {
                println!("logical:    {}", info.logical_id);
                println!("capability: {}", info.capability);
                println!("declared:   {}", info.capability_declared);
                println!(
                    "endpoint:   {}",
                    info.endpoint_path.as_deref().unwrap_or("-")
                );
                println!("adapter:    {}", info.adapter.as_deref().unwrap_or("-"));
                println!("allowed:    {}", info.allowed);
                if let Some(reason) = &info.fail_closed_reason {
                    println!("reason:     {reason}");
                }
            }
            Ok(())
        }
        #[cfg(not(feature = "ai-protocol"))]
        ModelCommands::ProtocolGenerative { .. } => {
            anyhow::bail!("Rebuild with --features ai-protocol to use this command.")
        }
    }
}

/// Handle `velaclaw providers`.
pub fn handle_providers_command(config: &Config) -> Result<()> {
    let providers = providers::list_providers();
    let current = config
        .default_provider
        .as_deref()
        .unwrap_or(DEFAULT_PROTOCOL_MODEL_ID)
        .trim()
        .to_ascii_lowercase();
    println!("Supported providers ({} total):\n", providers.len());
    println!("  ID (use in config)  DESCRIPTION");
    println!("  ─────────────────── ───────────");
    for p in &providers {
        let is_active = p.name.eq_ignore_ascii_case(&current)
            || p.aliases
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(&current));
        let marker = if is_active { " (active)" } else { "" };
        let local_tag = if p.local { " [local]" } else { "" };
        let aliases = if p.aliases.is_empty() {
            String::new()
        } else {
            format!("  (aliases: {})", p.aliases.join(", "))
        };
        println!(
            "  {:<19} {}{}{}{}",
            p.name, p.display_name, local_tag, marker, aliases
        );
    }
    println!();
    println!("Set default: edit default_provider in config, or run `velaclaw onboard`.");
    println!("\n  Legacy string keys and custom:<URL> endpoints were removed in ZS-ML-015.");
    println!("  Use provider/model ids backed by ai-protocol manifests.");
    Ok(())
}

/// Run onboard / quick-setup / channels-only flows.
pub async fn run_onboard_command(
    interactive: bool,
    force: bool,
    channels_only: bool,
    api_key: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    memory: Option<String>,
) -> Result<()> {
    if interactive && channels_only {
        bail!("Use either --interactive or --channels-only, not both");
    }
    if channels_only
        && (api_key.is_some() || provider.is_some() || model.is_some() || memory.is_some())
    {
        bail!("--channels-only does not accept --api-key, --provider, --model, or --memory");
    }
    if channels_only && force {
        bail!("--channels-only does not accept --force");
    }
    let config = if channels_only {
        onboard::wizard::run_channels_repair_wizard().await
    } else if interactive {
        onboard::wizard::run_wizard(force).await
    } else {
        onboard::wizard::run_quick_setup(
            api_key.as_deref(),
            provider.as_deref(),
            model.as_deref(),
            memory.as_deref(),
            force,
        )
        .await
    }?;
    if std::env::var("VELACLAW_AUTOSTART_CHANNELS").as_deref() == Ok("1") {
        channels::start_channels(config).await?;
    }
    Ok(())
}

/// Dispatch a configured CLI command (config already loaded).
pub async fn dispatch_configured_command(command: Commands, config: Config) -> Result<()> {
    match command {
        Commands::Onboard { .. }
        | Commands::Completions { .. }
        | Commands::Doctor {
            doctor_command: Some(DoctorCommands::Maintenance),
        } => unreachable!("handled before config load"),

        Commands::Agent {
            message,
            provider,
            model,
            temperature,
            peripheral,
            no_color,
            no_fold,
            plan,
            session_id,
        } => {
            let host_phase = if plan {
                velaclaw::agent::host_phase::HostPhase::Plan
            } else {
                velaclaw::agent::host_phase::HostPhase::Build
            };
            Box::pin(agent::run(
                config,
                message,
                provider,
                model,
                temperature,
                peripheral,
                no_color,
                no_fold,
                velaclaw::agent::AgentRunOpts {
                    extra_prompt_phases: &[],
                    host_phase,
                    chat_session_id: session_id,
                    persist_chat_session: true,
                },
            ))
            .await
            .map(|_| ())
        }

        Commands::Undo => {
            let msg =
                velaclaw::agent::workspace_undo::restore_tracked_if_git(&config.workspace_dir)?;
            println!("{msg}");
            Ok(())
        }

        Commands::Gateway { port, host } => {
            let port = port.unwrap_or(config.gateway.port);
            let host = host.unwrap_or_else(|| config.gateway.host.clone());
            if port == 0 {
                info!("🚀 Starting VelaClaw Gateway on {host} (random port)");
            } else {
                info!("🚀 Starting VelaClaw Gateway on {host}:{port}");
            }
            gateway::run_gateway(&host, port, config).await
        }

        Commands::Daemon { port, host } => {
            let port = port.unwrap_or(config.gateway.port);
            let host = host.unwrap_or_else(|| config.gateway.host.clone());
            if port == 0 {
                info!("🧠 Starting VelaClaw Daemon on {host} (random port)");
            } else {
                info!("🧠 Starting VelaClaw Daemon on {host}:{port}");
            }
            daemon::run(config, host, port).await
        }

        Commands::Status => status::print_status(&config),

        Commands::Cron { cron_command } => cron::handle_command(cron_command, &config),

        Commands::Models { model_command } => handle_models_command(&config, model_command).await,

        Commands::Providers => handle_providers_command(&config),

        Commands::Service {
            service_command,
            service_init,
        } => {
            let init_system = service_init.parse()?;
            service::handle_command(&service_command, &config, init_system)
        }

        Commands::Doctor { doctor_command } => match doctor_command {
            Some(DoctorCommands::Maintenance) => unreachable!("handled before config load"),
            Some(DoctorCommands::L4ShadowSummary { .. }) => {
                unreachable!("handled before config load")
            }
            Some(DoctorCommands::Models {
                provider,
                use_cache,
            }) => {
                let config_for_models = config.clone();
                tokio::task::spawn_blocking(move || {
                    doctor::run_models(&config_for_models, provider.as_deref(), use_cache)
                })
                .await
                .map_err(|e| anyhow::anyhow!("doctor models task failed: {e}"))?
            }
            Some(DoctorCommands::TemplateDag {
                fixture,
                message,
                compact,
            }) => {
                #[cfg(feature = "ai-protocol")]
                {
                    let _ = config;
                    let path = std::path::PathBuf::from(fixture);
                    doctor::run_template_dag_fixture(&path, &message, compact).map(|_| ())
                }
                #[cfg(not(feature = "ai-protocol"))]
                {
                    let _ = (config, fixture, message, compact);
                    anyhow::bail!(
                        "`velaclaw doctor template-dag` requires the `ai-protocol` Cargo feature"
                    )
                }
            }
            Some(DoctorCommands::CandidateDag {
                candidate,
                fallback,
                message,
                compact,
                stagnation_limit,
            }) => {
                #[cfg(feature = "ai-protocol")]
                {
                    let _ = config;
                    let candidate_path = std::path::PathBuf::from(candidate);
                    let fallback_path = fallback.map(std::path::PathBuf::from);
                    doctor::run_candidate_dag_fixture(
                        &candidate_path,
                        fallback_path.as_deref(),
                        &message,
                        compact,
                        stagnation_limit,
                    )
                    .map(|_| ())
                }
                #[cfg(not(feature = "ai-protocol"))]
                {
                    let _ = (
                        config,
                        candidate,
                        fallback,
                        message,
                        compact,
                        stagnation_limit,
                    );
                    anyhow::bail!(
                        "`velaclaw doctor candidate-dag` requires the `ai-protocol` Cargo feature"
                    )
                }
            }
            Some(DoctorCommands::Capabilities {
                tag,
                rebuild,
                reachable_only,
            }) => {
                #[cfg(feature = "ai-protocol")]
                {
                    doctor::run_capabilities(&config, tag.as_deref(), rebuild, reachable_only)
                }
                #[cfg(not(feature = "ai-protocol"))]
                {
                    let _ = (config, tag, rebuild, reachable_only);
                    anyhow::bail!(
                        "`velaclaw doctor capabilities` requires the `ai-protocol` Cargo feature"
                    )
                }
            }
            Some(DoctorCommands::Generative {
                capability,
                reachable_only,
                json,
            }) => {
                #[cfg(feature = "ai-protocol")]
                {
                    let _ = config;
                    doctor::run_generative(capability.as_deref(), reachable_only, json)
                }
                #[cfg(not(feature = "ai-protocol"))]
                {
                    let _ = (config, capability, reachable_only, json);
                    anyhow::bail!(
                        "`velaclaw doctor generative` requires the `ai-protocol` Cargo feature"
                    )
                }
            }
            Some(DoctorCommands::IntentRoute {
                message,
                hint,
                tag,
                rebuild,
                force,
                persist,
            }) => {
                #[cfg(feature = "ai-protocol")]
                {
                    doctor::run_intent_route(
                        &config,
                        &message,
                        hint.as_deref(),
                        tag.as_deref(),
                        rebuild,
                        force,
                        persist,
                    )
                }
                #[cfg(not(feature = "ai-protocol"))]
                {
                    let _ = (config, message, hint, tag, rebuild, force, persist);
                    anyhow::bail!(
                        "`velaclaw doctor intent-route` requires the `ai-protocol` Cargo feature"
                    )
                }
            }
            Some(DoctorCommands::Routing) => {
                #[cfg(feature = "ai-protocol")]
                {
                    doctor::run_routing(&config)
                }
                #[cfg(not(feature = "ai-protocol"))]
                {
                    let _ = config;
                    anyhow::bail!(
                        "`velaclaw doctor routing` requires the `ai-protocol` Cargo feature"
                    )
                }
            }
            Some(DoctorCommands::HostDecide {
                message,
                tag,
                force,
                set_override,
                clear_override,
                session_key,
            }) => {
                #[cfg(feature = "ai-protocol")]
                {
                    doctor::run_host_decide(
                        &config,
                        &message,
                        tag.as_deref(),
                        force,
                        set_override.as_deref(),
                        clear_override,
                        &session_key,
                    )
                }
                #[cfg(not(feature = "ai-protocol"))]
                {
                    let _ = (
                        config,
                        message,
                        tag,
                        force,
                        set_override,
                        clear_override,
                        session_key,
                    );
                    anyhow::bail!(
                        "`velaclaw doctor host-decide` requires the `ai-protocol` Cargo feature"
                    )
                }
            }
            Some(DoctorCommands::DagView {
                fixture,
                tag,
                set_override,
                session_key,
            }) => {
                #[cfg(feature = "ai-protocol")]
                {
                    let path = std::path::PathBuf::from(fixture);
                    doctor::run_dag_view(
                        &config,
                        &path,
                        tag.as_deref(),
                        set_override.as_deref(),
                        &session_key,
                    )
                }
                #[cfg(not(feature = "ai-protocol"))]
                {
                    let _ = (config, fixture, tag, set_override, session_key);
                    anyhow::bail!(
                        "`velaclaw doctor dag-view` requires the `ai-protocol` Cargo feature"
                    )
                }
            }
            Some(DoctorCommands::DagEmit {
                candidate,
                fallback,
                message,
                compact,
                stagnation_limit,
            }) => {
                #[cfg(feature = "ai-protocol")]
                {
                    let candidate_path = std::path::PathBuf::from(candidate);
                    let fallback_path = fallback.map(std::path::PathBuf::from);
                    doctor::run_dag_emit(
                        &candidate_path,
                        fallback_path.as_deref(),
                        &message,
                        compact,
                        stagnation_limit,
                    )
                }
                #[cfg(not(feature = "ai-protocol"))]
                {
                    let _ = (candidate, fallback, message, compact, stagnation_limit);
                    anyhow::bail!(
                        "`velaclaw doctor dag-emit` requires the `ai-protocol` Cargo feature"
                    )
                }
            }
            Some(DoctorCommands::DagPlan {
                message,
                fallback,
                force,
                compact,
                stagnation_limit,
                temperature,
            }) => {
                #[cfg(feature = "ai-protocol")]
                {
                    let fallback_path = fallback.map(std::path::PathBuf::from);
                    doctor::run_dag_plan(
                        &config,
                        &message,
                        fallback_path.as_deref(),
                        force,
                        compact,
                        stagnation_limit,
                        temperature,
                    )
                    .await
                }
                #[cfg(not(feature = "ai-protocol"))]
                {
                    let _ = (
                        config,
                        message,
                        fallback,
                        force,
                        compact,
                        stagnation_limit,
                        temperature,
                    );
                    anyhow::bail!(
                        "`velaclaw doctor dag-plan` requires the `ai-protocol` Cargo feature"
                    )
                }
            }
            None => doctor::run(&config),
        },

        Commands::Channel { channel_command } => match channel_command {
            ChannelCommands::Start => channels::start_channels(config).await,
            ChannelCommands::Doctor => channels::doctor_channels(config).await,
            other => channels::handle_command(other, &config).await,
        },

        Commands::Integrations {
            integration_command,
        } => integrations::handle_command(integration_command, &config),

        Commands::Skills { skill_command } => skills::handle_command(skill_command, &config),

        Commands::Migrate { migrate_command } => {
            migration::handle_command(migrate_command, &config).await
        }

        Commands::Memory { memory_command } => {
            memory::cli::handle_command(memory_command, &config).await
        }

        Commands::Auth { auth_command } => handle_auth_command(auth_command, &config).await,

        Commands::Hardware { hardware_command } => {
            hardware::handle_command(hardware_command.clone(), &config)
        }

        Commands::Peripheral { peripheral_command } => {
            peripherals::handle_command(peripheral_command.clone(), &config).await
        }

        Commands::Deploy { deploy_command } => {
            deploy::cli::handle_command(deploy_command, &config).await
        }
        Commands::Config { config_command } => match config_command {
            ConfigCommands::Schema => {
                let schema = schemars::schema_for!(Config);
                println!(
                    "{}",
                    serde_json::to_string_pretty(&schema).expect("failed to serialize JSON Schema")
                );
                Ok(())
            }
        },
    }
}
