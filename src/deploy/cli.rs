//! CLI handlers for deploy commands.

use crate::config::Config;
use crate::deploy::remote::{DeploymentConfig, DeploymentMode, DeploymentStatus, RemoteDeployer};
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use tracing::info;

/// Handle deploy subcommands.
pub async fn handle_command(
    deploy_command: crate::deploy::DeployCommands,
    config: &Config,
) -> Result<()> {
    let deploy_config = crate::config::DeployConfig::default();
    let targets = load_deploy_config(config, &deploy_config)?;

    match deploy_command {
        crate::deploy::DeployCommands::Deploy { server } => {
            handle_deploy(&server, &targets, deploy_config.clone()).await
        }
        crate::deploy::DeployCommands::Status { server } => handle_status(&server, &targets),
        crate::deploy::DeployCommands::HealthCheck { server } => {
            handle_health_check(&server, &targets).await
        }
        crate::deploy::DeployCommands::List => handle_list(&targets),
        crate::deploy::DeployCommands::Rollback { server } => {
            handle_rollback(&server, &targets).await
        }
        crate::deploy::DeployCommands::Update { server, version } => {
            handle_update(&server, version, &targets, deploy_config.clone()).await
        }
        crate::deploy::DeployCommands::SyncConfig { server } => handle_sync_config(&server, config),
        crate::deploy::DeployCommands::Validate { server } => {
            handle_validate(&server, &targets, deploy_config.clone()).await
        }
    }
}

/// Load deploy configuration from config.
fn load_deploy_config(
    config: &Config,
    _deploy_cfg: &crate::config::DeployConfig,
) -> Result<Vec<crate::deploy::remote::DeploymentTarget>> {
    if config.deploy.servers.is_empty() {
        bail!(
            "No deployment targets configured. \
             Add targets to your config.toml under [deploy.servers]"
        );
    }

    // Convert from config types to deployment runtime types
    let targets: Result<Vec<crate::deploy::remote::DeploymentTarget>> = config
        .deploy
        .servers
        .iter()
        .map(convert_target_config)
        .collect();
    targets
}

/// Convert DeploymentTargetConfig to DeploymentTarget.
fn convert_target_config(
    cfg: &crate::config::DeploymentTargetConfig,
) -> Result<crate::deploy::remote::DeploymentTarget> {
    // Convert labels from Vec<String> to HashMap<String, String>
    let labels: HashMap<String, String> = cfg
        .labels
        .iter()
        .map(|label| {
            // Parse key:value format
            if let Some((key, value)) = label.split_once(':') {
                (key.to_string(), value.to_string())
            } else if let Some((key, value)) = label.split_once('=') {
                (key.to_string(), value.to_string())
            } else {
                // If no separator, use the label as both key and value
                (label.clone(), label.clone())
            }
        })
        .collect();

    // Convert ssh_key string to path
    let ssh_key_path = cfg.ssh_key.as_ref().map(PathBuf::from);

    Ok(crate::deploy::remote::DeploymentTarget {
        id: cfg.id.clone(),
        host: cfg.host.clone(),
        port: cfg.port,
        user: cfg.user.clone(),
        ssh_key_path,
        labels,
    })
}

/// Parse deployment mode from config.
fn parse_deployment_mode(mode_str: &str) -> DeploymentMode {
    match mode_str.to_lowercase().as_str() {
        "docker" => DeploymentMode::Docker,
        "systemd" => DeploymentMode::Systemd,
        _ => DeploymentMode::Direct,
    }
}

/// Create deployment config from settings.
fn create_deployment_config(
    settings: &crate::config::DeploymentSettingsConfig,
    version: &str,
) -> DeploymentConfig {
    // Auto-detect local binary path
    let local_binary = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .unwrap_or_else(|_| PathBuf::from("./target/release/zerospider"));

    DeploymentConfig {
        name: "zerospider".to_string(),
        version: version.to_string(),
        local_binary,
        binary_path: PathBuf::from(&settings.binary_path),
        config_path: settings.config_path.as_ref().map(PathBuf::from),
        env_vars: HashMap::new(),
        working_dir: PathBuf::from(&settings.working_dir),
        auto_start: settings.auto_start,
        health_check_interval: std::time::Duration::from_secs(settings.health_check_interval_secs),
        restart_on_failure: settings.restart_on_failure,
        max_restarts: settings.max_restarts,
        use_sudo: settings.use_sudo,
    }
}

/// Handle deploy command.
async fn handle_deploy(
    server_id: &str,
    targets: &[crate::deploy::remote::DeploymentTarget],
    deploy_config: crate::config::DeployConfig,
) -> Result<()> {
    let target = find_target(server_id, targets)?;

    info!("Starting deployment to {} ({})...", server_id, target.host);

    let mode = parse_deployment_mode(&deploy_config.settings.mode);
    let mut deployer = RemoteDeployer::new(mode);
    deployer.register_target(target.clone());

    let dep_config = create_deployment_config(&deploy_config.settings, "latest");
    deployer.set_config(dep_config);

    deployer
        .deploy(server_id, "zerospider")
        .await
        .context("Deployment failed")?;

    println!("✅ Deployed successfully to {}", server_id);

    Ok(())
}

/// Handle status command.
fn handle_status(
    server_id: &str,
    targets: &[crate::deploy::remote::DeploymentTarget],
) -> Result<()> {
    let target = find_target(server_id, targets)?;

    info!("Checking deployment status for {}...", server_id);

    let mut deployer = RemoteDeployer::new(DeploymentMode::Direct);
    deployer.register_target(target.clone());

    let status = deployer
        .get_status(server_id)
        .context("Failed to get status")?;

    println!("📊 Deployment Status for {}", server_id);
    print_status(status);

    Ok(())
}

/// Print deployment status.
fn print_status(status: &DeploymentStatus) {
    println!("   Status: {:?}", status.target_id);
    println!("   Deployed: {}", status.deployed);
    if let Some(version) = &status.version {
        println!("   Version: {}", version);
    }
    println!("   Running: {}", status.running);
    if status.healthy() {
        println!("   Health: ✅ Healthy");
    } else {
        println!("   Health: ❌ Unhealthy");
    }
}

/// Handle health-check command.
async fn handle_health_check(
    server_id: &str,
    targets: &[crate::deploy::remote::DeploymentTarget],
) -> Result<()> {
    let target = find_target(server_id, targets)?;

    info!("Running health check for {}...", server_id);

    let mut deployer = RemoteDeployer::new(DeploymentMode::Direct);
    deployer.register_target(target.clone());

    let healthy = deployer
        .health_check(server_id)
        .await
        .context("Health check failed")?;

    if healthy {
        println!("✅ Health check passed for {}", server_id);
    } else {
        bail!("❌ Health check failed for {}", server_id);
    }

    Ok(())
}

/// Handle list command.
fn handle_list(targets: &[crate::deploy::remote::DeploymentTarget]) -> Result<()> {
    if targets.is_empty() {
        println!("No deployment targets configured.");
        return Ok(());
    }

    println!("🌐 Deployment Targets ({} total):", targets.len());
    println!();
    for target in targets {
        println!("  ID: {}", target.id);
        println!("  Host: {}:{}", target.host, target.port);
        println!("  User: {}", target.user);
        if let Some(ssh_key) = &target.ssh_key_path {
            println!("  SSH Key: {}", ssh_key.display());
        }
        if !target.labels.is_empty() {
            println!("  Labels:");
            for (key, value) in &target.labels {
                println!("    {} = {}", key, value);
            }
        }
        println!();
    }

    Ok(())
}

/// Handle rollback command.
async fn handle_rollback(
    server_id: &str,
    targets: &[crate::deploy::remote::DeploymentTarget],
) -> Result<()> {
    let target = find_target(server_id, targets)?;

    info!("Rolling back deployment on {}...", server_id);

    let mut deployer = RemoteDeployer::new(DeploymentMode::Direct);
    deployer.register_target(target.clone());

    deployer
        .rollback(server_id)
        .await
        .context("Rollback failed")?;

    println!("✅ Rolled back successfully on {}", server_id);

    Ok(())
}

/// Handle update command.
async fn handle_update(
    server_id: &str,
    version: Option<String>,
    targets: &[crate::deploy::remote::DeploymentTarget],
    deploy_config: crate::config::DeployConfig,
) -> Result<()> {
    let target = find_target(server_id, targets)?;

    let version = version.unwrap_or_else(|| "latest".to_string());

    info!("Updating {} to version {}...", server_id, version);

    // For now, update is essentially the same as deploy
    // In a more sophisticated implementation, this would:
    // 1. Check current version
    // 2. Download new binary
    // 3. Perform rolling update

    println!(
        "ℹ️  Update will deploy version {} to {}",
        version, server_id
    );
    println!("   This is equivalent to a deploy operation with the specified version.");

    let mode = parse_deployment_mode(&deploy_config.settings.mode);
    let mut deployer = RemoteDeployer::new(mode);
    deployer.register_target(target.clone());

    let dep_config = create_deployment_config(&deploy_config.settings, &version);
    deployer.set_config(dep_config);

    deployer
        .deploy(server_id, "zerospider")
        .await
        .context("Update failed")?;

    println!("✅ Updated successfully to {} on {}", version, server_id);

    Ok(())
}

/// Handle sync-config command.
fn handle_sync_config(_server_id: &str, _config: &Config) -> Result<()> {
    bail!(
        "sync-config is not yet implemented. \
         To update the remote configuration, manually edit the config file on the target server \
         or redeploy with the new configuration."
    );
}

/// Find a deployment target by ID.
fn find_target<'a>(
    server_id: &str,
    targets: &'a [crate::deploy::remote::DeploymentTarget],
) -> Result<&'a crate::deploy::remote::DeploymentTarget> {
    targets
        .iter()
        .find(|t| t.id == server_id)
        .ok_or_else(|| anyhow::anyhow!("Deployment target '{server_id}' not found"))
}

/// Handle validate command.
async fn handle_validate(
    server_id: &str,
    targets: &[crate::deploy::remote::DeploymentTarget],
    deploy_config: crate::config::DeployConfig,
) -> Result<()> {
    let target = find_target(server_id, targets)?;

    info!("Validating deployment readiness for {}...", server_id);

    println!(
        "🔍 Validating deployment readiness for {} ({})...",
        server_id, target.host
    );
    println!();

    let mode = parse_deployment_mode(&deploy_config.settings.mode);

    let mut has_errors = false;
    let mut has_warnings = false;

    // Test SSH connectivity
    println!("1️⃣ Testing SSH connectivity...");
    match test_ssh_connection(target).await {
        Ok(()) => println!("   ✅ SSH connection successful"),
        Err(e) => {
            println!("   ❌ SSH connection failed: {}", e);
            has_errors = true;
        }
    }
    println!();

    // Check directory write permissions
    println!("2️⃣ Checking directory permissions...");
    let test_dirs = [
        &deploy_config.settings.binary_path,
        &deploy_config.settings.working_dir,
    ];
    for (i, _dir) in test_dirs.iter().enumerate() {
        let full_dir = test_dirs[i];
        match test_directory_permission(target, full_dir).await {
            Ok(()) => println!("   ✅ Can write to {}", full_dir),
            Err(e) => {
                println!("   ⚠️  Cannot write to {} (may need sudo): {}", full_dir, e);
                has_warnings = true;
            }
        }
    }
    println!();

    // Check mode-specific requirements
    println!("3️⃣ Checking mode-specific requirements ({:?})...", mode);
    let mode_clone = mode.clone();
    match mode_clone {
        DeploymentMode::Docker => match test_docker_available(target).await {
            Ok(()) => println!("   ✅ Docker is available"),
            Err(e) => {
                println!("   ❌ Docker check failed: {}", e);
                has_errors = true;
            }
        },
        DeploymentMode::Systemd => {
            match test_systemctl_available(target).await {
                Ok(()) => println!("   ✅ systemctl is available"),
                Err(e) => {
                    println!("   ⚠️  systemctl check failed (may need sudo): {}", e);
                    has_warnings = true;
                }
            }
            if deploy_config.settings.use_sudo {
                println!("   ℹ️  use_sudo is enabled - systemctl commands will use sudo");
            }
        }
        DeploymentMode::Direct => {
            println!("   ✅ No mode-specific requirements for Direct mode");
        }
    }
    println!();

    // Check sudo configuration
    if deploy_config.settings.use_sudo {
        println!("4️⃣ Checking sudo configuration...");
        let mut deployer = RemoteDeployer::new(mode.clone());
        deployer.register_target(target.clone());
        match test_sudo_available(target).await {
            Ok(()) => println!("   ✅ sudo is available"),
            Err(e) => {
                println!("   ⚠️  sudo check failed: {}", e);
                has_warnings = true;
            }
        }
        println!();
    }

    // Print summary
    println!("📋 Validation Summary:");
    if !has_errors && !has_warnings {
        println!("   ✅ All checks passed! Ready to deploy.");
        println!();
        println!("   To deploy, run: deploy deploy --server {}", server_id);
    } else if has_errors && !has_warnings {
        println!("   ❌ Validation failed with errors.");
        println!("   Please fix the errors above before deploying.");
    } else if !has_errors && has_warnings {
        println!("   ⚠️  Validation passed with warnings.");
        println!("   Deployment should work, but some issues may require manual intervention.");
        println!();
        println!(
            "   To deploy despite warnings, run: deploy deploy --server {}",
            server_id
        );
    } else {
        println!("   ❌ Validation failed with both errors and warnings.");
        println!("   Please fix the issues above before deploying.");
    }

    if has_errors {
        bail!("Validation failed with errors");
    }

    Ok(())
}

/// Test SSH connectivity.
async fn test_ssh_connection(target: &crate::deploy::remote::DeploymentTarget) -> Result<()> {
    let output = tokio::process::Command::new("ssh")
        .arg("-o")
        .arg("StrictHostKeyChecking=no")
        .arg("-o")
        .arg("ConnectTimeout=5")
        .arg("-p")
        .arg(target.port.to_string())
        .arg(format!("{}@{}", target.user, target.host))
        .arg("echo 'SSH connection successful'")
        .output()
        .await
        .context("SSH execution failed")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("SSH command failed: {}", stderr);
    }

    Ok(())
}

/// Test directory write permission.
async fn test_directory_permission(
    target: &crate::deploy::remote::DeploymentTarget,
    dir: &str,
) -> Result<()> {
    let test_file = format!("{}/.test_write_{}", dir, std::process::id());

    // Try to create a temp file
    let _ = tokio::process::Command::new("ssh")
        .arg("-o")
        .arg("StrictHostKeyChecking=no")
        .arg("-p")
        .arg(target.port.to_string())
        .arg(format!("{}@{}", target.user, target.host))
        .arg(format!("touch {} && rm -f {}", test_file, test_file))
        .output()
        .await
        .context("Directory permission test failed")?;

    Ok(())
}

/// Test if Docker is available.
async fn test_docker_available(target: &crate::deploy::remote::DeploymentTarget) -> Result<()> {
    let output = tokio::process::Command::new("ssh")
        .arg("-o")
        .arg("StrictHostKeyChecking=no")
        .arg("-p")
        .arg(target.port.to_string())
        .arg(format!("{}@{}", target.user, target.host))
        .arg("docker --version")
        .output()
        .await
        .context("Docker availability check failed")?;

    if !output.status.success() {
        bail!("Docker is not installed or user lacks permissions");
    }

    Ok(())
}

/// Test if systemctl is available.
async fn test_systemctl_available(target: &crate::deploy::remote::DeploymentTarget) -> Result<()> {
    let output = tokio::process::Command::new("ssh")
        .arg("-o")
        .arg("StrictHostKeyChecking=no")
        .arg("-p")
        .arg(target.port.to_string())
        .arg(format!("{}@{}", target.user, target.host))
        .arg("systemctl --version")
        .output()
        .await
        .context("systemctl availability check failed")?;

    if !output.status.success() {
        bail!("systemctl is not installed or user lacks permissions");
    }

    Ok(())
}

/// Test if sudo is available.
async fn test_sudo_available(target: &crate::deploy::remote::DeploymentTarget) -> Result<()> {
    let output = tokio::process::Command::new("ssh")
        .arg("-o")
        .arg("StrictHostKeyChecking=no")
        .arg("-p")
        .arg(target.port.to_string())
        .arg(format!("{}@{}", target.user, target.host))
        .arg("sudo -n true")
        .output()
        .await
        .context("sudo availability check failed")?;

    if !output.status.success() {
        bail!("sudo is not configured or user lacks permissions");
    }

    Ok(())
}
