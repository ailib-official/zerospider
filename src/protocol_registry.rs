//! Scan `AI_PROTOCOL_DIR` for provider manifests and model registry entries.
//! Used by CLI `models protocol-*` and availability checks.

use ai_lib_rust::protocol::AuthConfig;
use ai_lib_rust::protocol::ProtocolManifest;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const ENV_PROTOCOL_DIR: &str = "AI_PROTOCOL_DIR";
const ENV_PROTOCOL_PATH: &str = "AI_PROTOCOL_PATH";

/// Resolve local ai-protocol checkout root (not HTTP URLs).
pub fn resolve_local_protocol_root() -> Option<PathBuf> {
    let raw = std::env::var(ENV_PROTOCOL_DIR)
        .ok()
        .or_else(|| std::env::var(ENV_PROTOCOL_PATH).ok())?;
    if raw.starts_with("http://") || raw.starts_with("https://") {
        return None;
    }
    let p = PathBuf::from(raw.trim());
    if p.is_dir() {
        Some(p)
    } else {
        None
    }
}

fn collect_provider_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let candidates = [
        root.join("dist").join("v2").join("providers"),
        root.join("v2").join("providers"),
        root.join("dist").join("v1").join("providers"),
        root.join("v1").join("providers"),
    ];
    for dir in candidates {
        if !dir.is_dir() {
            continue;
        }
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for ent in rd.flatten() {
                let path = ent.path();
                let ext = path.extension().and_then(|s| s.to_str());
                let ok = path.is_file() && matches!(ext, Some("json" | "yaml" | "yml"));
                if ok {
                    out.push(path);
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn provider_id_from_path(path: &Path) -> Option<String> {
    path.file_stem()
        .and_then(|s| s.to_str())
        .map(std::string::ToString::to_string)
}

fn required_envs(auth: &AuthConfig) -> Vec<String> {
    let mut v = Vec::new();
    if let Some(ref k) = auth.key_env {
        v.push(k.clone());
    }
    if let Some(ref t) = auth.token_env {
        v.push(t.clone());
    }
    v.sort();
    v.dedup();
    v
}

fn env_nonempty(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .is_some_and(|s| !s.trim().is_empty())
}

fn load_provider_manifest(path: &Path) -> anyhow::Result<ProtocolManifest> {
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
    let bytes = std::fs::read(path)?;
    if ext.eq_ignore_ascii_case("json") {
        return Ok(serde_json::from_slice(&bytes)?);
    }
    if ext.eq_ignore_ascii_case("yaml") || ext.eq_ignore_ascii_case("yml") {
        let s = String::from_utf8_lossy(&bytes);
        return Ok(serde_yaml::from_str(&s)?);
    }
    anyhow::bail!("unsupported provider manifest extension: {ext}");
}

/// One provider from disk with optional auth env analysis.
#[derive(Debug, Clone, Serialize)]
pub struct ProtocolProviderInfo {
    pub id: String,
    pub manifest_path: PathBuf,
    pub required_envs: Vec<String>,
    pub available: bool,
}

/// Logical model id from a registry file (`models` map keys + provider field).
#[derive(Debug, Clone, Serialize)]
pub struct ProtocolModelInfo {
    pub logical_id: String,
    pub provider: String,
    pub source_file: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProtocolRegistrySnapshot {
    pub protocol_root: PathBuf,
    pub providers: Vec<ProtocolProviderInfo>,
    pub models: Vec<ProtocolModelInfo>,
}

/// Scan provider manifests under `root` and model registries under `v1/models` / `dist/v1/models`.
pub fn scan_protocol_root(root: &Path) -> anyhow::Result<ProtocolRegistrySnapshot> {
    let mut providers = Vec::new();
    for path in collect_provider_files(root) {
        let Some(id) = provider_id_from_path(&path) else {
            continue;
        };
        let manifest = match load_provider_manifest(&path) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(path = %path.display(), "skip invalid provider manifest: {e}");
                continue;
            }
        };
        let required_envs = manifest
            .auth
            .as_ref()
            .map(required_envs)
            .unwrap_or_default();
        let available = required_envs.is_empty() || required_envs.iter().all(|n| env_nonempty(n));
        let resolved_id = if manifest.id.trim().is_empty() {
            id
        } else {
            manifest.id.clone()
        };
        providers.push(ProtocolProviderInfo {
            id: resolved_id,
            manifest_path: path,
            required_envs,
            available,
        });
    }
    providers.sort_by(|a, b| a.id.cmp(&b.id));

    let mut models = Vec::new();
    for base in [
        root.join("dist").join("v1").join("models"),
        root.join("v1").join("models"),
    ] {
        if !base.is_dir() {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&base) else {
            continue;
        };
        for ent in rd.flatten() {
            let path = ent.path();
            let ext = path.extension().and_then(|s| s.to_str());
            let prefer_json = ext == Some("json");
            let prefer_yaml = matches!(ext, Some("yaml" | "yml"));
            if !(prefer_json || prefer_yaml) {
                continue;
            }
            let reg: BTreeMap<String, serde_json::Value> = if prefer_json {
                let bytes = match std::fs::read(&path) {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                let v: serde_json::Value = match serde_json::from_slice(&bytes) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let Some(m) = v.get("models").and_then(|x| x.as_object()) else {
                    continue;
                };
                m.iter().map(|(k, val)| (k.clone(), val.clone())).collect()
            } else {
                let s = match std::fs::read_to_string(&path) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let v: serde_yaml::Value = match serde_yaml::from_str(&s) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let Some(m) = v.get("models").and_then(|x| x.as_mapping()) else {
                    continue;
                };
                let mut out = BTreeMap::new();
                for (k, val) in m {
                    let Some(ks) = k.as_str() else {
                        continue;
                    };
                    let j = serde_json::to_value(val).unwrap_or(serde_json::Value::Null);
                    out.insert(ks.to_string(), j);
                }
                out
            };

            for (logical_id, meta) in reg {
                let provider = meta
                    .get("provider")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                models.push(ProtocolModelInfo {
                    logical_id,
                    provider,
                    source_file: path.clone(),
                });
            }
        }
    }
    models.sort_by(|a, b| a.logical_id.cmp(&b.logical_id));

    Ok(ProtocolRegistrySnapshot {
        protocol_root: root.to_path_buf(),
        providers,
        models,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_empty_dir_yields_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let snap = scan_protocol_root(dir.path()).expect("scan");
        assert!(snap.providers.is_empty());
        assert!(snap.models.is_empty());
    }
}
