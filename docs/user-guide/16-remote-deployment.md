# Remote Deployment (v0.3.0)

## Overview

ZeroSpider provides controlled remote deployment capabilities, allowing you to deploy and manage AI agents on remote servers via SSH. This feature is available with the `--features remote-deploy` compile flag.

## Features

- **Multiple deployment modes**: Direct binary deployment, Docker containers, and systemd services
- **Health monitoring**: Automated health checks and status reporting
- **Rollback support**: Safe rollback to previous deployments
- **Configuration sync**: Synchronize configuration to remote servers
- **Target management**: Organize deployment targets with labels

## Quick Start

### Building with Remote Deploy Feature

```bash
cargo build --release --features remote-deploy
```

### Bootstrap Remote Servers

**Important**: Before deploying, run the bootstrap script ONCE on each remote server to prepare the environment:

```bash
# Option 1: Run directly on remote server
ssh user@remote-server
wget https://raw.githubusercontent.com/ailib-official/zerospider/main/bootstrap.sh
chmod +x bootstrap.sh
sudo bash bootstrap.sh
```

```bash
# Option 2: Copy from local and run
scp bootstrap.sh user@remote-server:/tmp/
ssh user@remote-server 'sudo bash /tmp/bootstrap.sh'
```

#### What Bootstrap Script Does

The bootstrap script (`bootstrap.sh`) automatically:

1. **Creates deploy user** (if not exists)
2. **Sets up directories**: `/opt/zerospider`, `/opt/zerospider/logs`
3. **Sets file permissions**: Ownership for deploy user on all directories
4. **Configures sudoers**: Passwordless sudo for deploy user for:
   - systemctl commands (daemon-reload, enable, start, stop, restart)
   - Directory creation and ownership
   - Binary installation
5. **Adds to docker group** (if Docker is installed)
6. **Creates systemd service file**: `/etc/systemd/system/zerospider.service`
7. **Creates marker file**: `/opt/zerospider/.bootstrap-complete` to prevent re-running

### Configuration

Add deployment targets to your `~/.zerospider/config.toml`:

```toml
[deploy]
[[deploy.servers]]
id = "prod-001"
host = "192.168.1.100"
port = 22
user = "deploy"
ssh_key = "~/.ssh/id_ed25519"
labels = ["env:production", "region:us-west"]

[[deploy.servers]]
id = "staging-001"
host = "192.168.1.101"
port = 2222
user = "deploy"
ssh_key = "~/.ssh/deploy_key"
labels = ["env:staging", "region:us-west"]

[deploy.settings]
mode = "systemd"                     # Options: direct, docker, systemd
binary_path = "/usr/local/bin/zerospider"
working_dir = "/opt/zerospider"
config_path = "/opt/zerospider/config.toml"
auto_start = true
health_check_interval_secs = 30
restart_on_failure = true
max_restarts = 3
use_sudo = true                       # Use sudo for systemctl/docker commands
```

### Deploying to a Server

```bash
# Deploy to the prod-001 server
zerospider deploy deploy --server prod-001

# Check the deployment status
zerospider deploy status --server prod-001

# Run a health check
zerospider deploy health-check --server prod-001

# Validate deployment readiness before deploying (NEW)
zerospider deploy validate --server prod-001
```

## Commands

### deploy

Deploy ZeroSpider to a remote server:

```bash
zerospider deploy deploy --server <server-id>
```

Example:
```bash
zerospider deploy deploy --server prod-001
```

Process:
1. Validates deployment target configuration
2. Creates remote working directory
3. Uploads the ZeroSpider binary
4. Configures the server based on deployment mode
5. Starts the service (if `auto_start = true`)

### status

Display deployment status for a specific server:

```bash
zerospider deploy status --server <server-id>
```

Example output:
```
📊 Deployment Status for prod-001
   Status: prod-001
   Deployed: true
   Version: latest
   Running: true
   Health: ✅ Healthy
```

### health-check

Run a health check on a deployed server:

```bash
zerospider deploy health-check --server <server-id>
```

The health check verifies that:
- The ZeroSpider process is running
- The service is responding to requests

### list

List all configured deployment targets:

```bash
zerospider deploy list
```

Example output:
```
🌐 Deployment Targets (2 total):

  ID              HOST               MODE         LABELS
  ─────────────── ───────────────── ──────────── ──────
  prod-001        192.168.1.100      direct       env=production,region=us-west
  staging-001     192.168.1.101      direct       env=staging,region=us-west
```

### rollback

Rollback to the previous deployment:

```bash
zerospider deploy rollback --server <server-id>
```

This will:
1. Stop the current ZeroSpider service
2. Restore the previous version
3. Restart the service

### update

Update to a specific version:

```bash
zerospider deploy update --server <server-id> --version <version>
```

If version is not specified, updates to "latest" (current binary).

Example:
```bash
zerospider deploy update --server prod-001 --version v0.3.0
```

### sync-config

Synchronize the local configuration to the remote server:

```bash
zerospider deploy sync-config --server <server-id>
```

This uploads your local `~/.zerospider/config.toml` to the remote server with the path specified in `deploy.settings.config_path`.

## Deployment Modes

### Mode Comparison

| Mode | Requirements | Setup | Advantages |
|------|-------------|-------|-----------|
| **Direct** | Write access to `/usr/local/bin/`, `/opt/zerospider` | Run bootstrap.sh | Simple, no runtime dependencies |
| **Docker** | Docker daemon, deploy user in docker group | Run bootstrap.sh | Containerized, easy version management |
| **Systemd** | systemctl, sudo configured in bootstrap.sh | Run bootstrap.sh | Auto-restart, managed service, logs in journald |

### Direct Mode

Deploys ZeroSpider as a standalone binary process:

```toml
[deploy.settings]
mode = "direct"
```

Recommended for:
- Simple deployments
- Development environments
- Servers without container runtimes

### Docker Mode

Deploys ZeroSpider in a Docker container:

```toml
[deploy.settings]
mode = "docker"
```

Recommended for:
- Production environments
- Isolated runtime environments
- Easy version management

Requirements:
- Docker installed on remote server
- User has Docker permissions

### Systemd Mode

Deploys ZeroSpider as a systemd service:

```toml
[deploy.settings]
mode = "systemd"
```

Recommended for:
- Production Linux servers
- Automatic restart on boot
- Log management via `journalctl`

## SSH Key Management

### Using Default SSH Key

If no `ssh_key` is specified, ZeroSpider uses the default SSH key path:
- `~/.ssh/identity`
- `~/.ssh/id_rsa`
- `~/.ssh/id_ed25519`

### Using Custom SSH Key

```toml
[[deploy.servers]]
id = "prod-001"
host = "192.168.1.100"
ssh_key = "/path/to/custom/key"
```

### Security Best Practices

1. Use SSH keys instead of passwords
2. Restrict SSH key permissions: `chmod 600 ~/.ssh/id_ed25519`
3. Use dedicated deploy user with minimal privileges
4. Enable `config.autonomy.workspace_only` on remote servers

## Troubleshooting

### Connection Issues

If deployment fails with SSH connection errors:

```bash
# Test SSH connection manually
ssh -i ~/.ssh/id_ed25519 deploy@192.168.1.100 -p 22

# Check SSH key permissions
ls -la ~/.ssh/id_ed25519  # Should be -rw------- (600)
```

### Permission Issues

**Symptom**: Permission denied when creating directories or installing binary:

```
Permission denied: mkdir -p /opt/zerospider
```

**Solution**: Run bootstrap.sh on remote server:
```bash
ssh deploy@remote-server 'sudo bash < /path/to/bootstrap.sh'
```

If bootstrap script was already run, ensure directories exist:
```bash
# SSH into the server
ssh deploy@192.168.1.100

# Check directory permissions
ls -la /opt/zerospider

# Ensure the deploy user has write access
sudo chown -R deploy:deploy /opt/zerospider
```

### systemctl Requires Permissions

**Symptom**:
```
Permission denied: systemctl daemon-reload
```

**Solution 1** (Recommended): Run bootstrap.sh (includes sudoers config)

**Solution 2** (Manual):
```bash
# On remote server, run as root:
echo "deploy ALL=(ALL) NOPASSWD: /usr/bin/systemctl daemon-reload, \
    /usr/bin/systemctl enable zerospider, \
    /usr/bin/systemctl start zerospider, \
    /usr/bin/systemctl stop zerospider, \
    /usr/bin/systemctl restart zerospider" | \
    sudo tee /etc/sudoers.d/deployer-zerospider
sudo chmod 0440 /etc/sudoers.d/deployer-zerospider
```

### Docker Permission Denied

**Symptom**:
```
permission denied while trying to connect to the Docker daemon socket
```

**Solution**: Add deploy user to docker group:
```bash
ssh root@remote-server 'usermod -aG docker deploy'
# User must re-login for changes to take effect:
ssh deploy@remote-server  # Logout and login again
```

### Docker: Unknown Image

**Symptom**:
```
Error: pull access denied for zerospider:latest
```

**Solution 1** (if using private registry): Configure Docker credentials

**Solution 2** (if using public hub): Build custom image first:
```bash
# On local machine:
docker build -t zerospider:latest .
# Tag and push to registry
docker tag zerospider:latest registry.example.com/zerospider:latest
docker push registry.example.com/zerospider:latest
```

### Service Not Starting

If the service starts but immediately crashes:

```bash
# Check if the process is running
pgrep zerospider

# Check systemd logs (if using systemd mode)
sudo systemctl status zerospider
sudo journalctl -u zerospider -n 50

# Check logs in working directory
tail -f /opt/zerospider/logs/zerospider.log

# Manual test - run binary directly
cd /opt/zerospider && /usr/local/bin/zerospider
```

## Advanced Configuration

### Multiple Environments

Keep separate deployment targets for different environments:

```toml
# Development
[[deploy.servers]]
id = "dev-001"
host = "dev.example.com"
labels = ["env:development"]

# Staging
[[deploy.servers]]
id = "staging-001"
host = "staging.example.com"
labels = ["env:staging"]

# Production
[[deploy.servers]]
id = "prod-001"
host = "prod.example.com"
labels = ["env:production"]
```

### Labels for Grouping

Use labels to organize and query deployment targets:

```toml
[[deploy.servers]]
id = "us-west-prod-001"
host = "192.168.1.100"
labels = ["region:us-west", "env:production", "service:agent"]

[[deploy.servers]]
id = "us-east-prod-001"
host = "192.168.1.200"
labels = ["region:us-east", "env:production", "service:agent"]
```

Example queries (future feature):
```bash
zerospider deploy list --labels env:production,region:us-west
```

## Security Considerations

### SSH Security

1. **Use SSH keys**: Never use password authentication
2. **Disable password auth**: On remote servers, set `PasswordAuthentication no` in `/etc/ssh/sshd_config`
3. **Use non-root user**: Deploy and run as a non-privileged user
4. **Limit SSH access**: Use firewall rules to restrict SSH access

### ZeroSpider Configuration

1. **Enable workspace-only mode** for remote deployments:
   ```toml
   [autonomy]
   workspace_only = true
   ```

2. **Restrict allowed commands**:
   ```toml
   [autonomy]
   allowed_commands = ["file_read", "file_write"]
   ```

3. **Set cost limits**:
   ```toml
   [cost]
   max_cost_per_day_cents = 1000
   ```

## Limitations

- Current implementation does not parallelize deployments
- Automatic rollback on deployment failure is not yet implemented
- `sync-config` command is a placeholder (not yet implemented)
- Labels for filtering deployment targets are not yet supported in the CLI

## Future Improvements

- **Parallel deployments**: Deploy to multiple servers simultaneously
- **Canary deployments**: Roll out updates to a subset of targets first
- **Blue-green deployments**: Maintain two production environments for zero-downtime updates
- **Metrics collection**: Collect and display deployment metrics from remote servers
- **Auto-scaling**: Automatically scale deployment based on load

## See Also

- [Getting Started](01-getting-started.md)
- [Configuration](19-config.md)
- [Security](17-security.md)
- [Deployment](15-deployment.md)
