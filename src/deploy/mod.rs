//! Deployment module for remote server management.

pub mod cli;
pub mod remote;

/// DeployCommands for deploy subcommands.
#[derive(clap::Parser, Debug)]
pub enum DeployCommands {
    /// Deploy ZeroClaw to a remote server
    Deploy {
        /// Server ID to deploy to
        #[arg(short, long)]
        server: String,
    },
    /// Show deployment status for a server
    Status {
        /// Server ID to check status for
        #[arg(short, long)]
        server: String,
    },
    /// Run health check for a deployed server
    HealthCheck {
        /// Server ID to run health check for
        #[arg(short, long)]
        server: String,
    },
    /// List all configured deployment targets
    List,
    /// Rollback to previous deployment
    Rollback {
        /// Server ID to rollback
        #[arg(short, long)]
        server: String,
    },
    /// Update to new version
    Update {
        /// Server ID to update
        #[arg(short, long)]
        server: String,
        /// Version to deploy (default: latest)
        #[arg(long)]
        version: Option<String>,
    },
    /// Sync configuration to remote server
    SyncConfig {
        /// Server ID to sync config to
        #[arg(short, long)]
        server: String,
    },
    /// Validate deployment readiness for a server
    Validate {
        /// Server ID to validate
        #[arg(short, long)]
        server: String,
    },
}
