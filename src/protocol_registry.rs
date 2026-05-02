//! Scan `AI_PROTOCOL_DIR` for provider manifests and model registry entries.
//! Used by CLI `models protocol-*` and availability checks.

use ai_lib_rust::protocol::ProtocolManifest;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const ENV_PROTOCOL_DIR: &str = "AI_PROTOCOL_DIR";
const ENV_PROTOCOL_PATH: &str = "AI_PROTOCOL_PATH";

/// Parse a value of `AI_PROTOCOL_DIR` / `AI_PROTOCOL_PATH`.
///
/// Returns a directory only for **local** paths (not `http`/`https` URLs) that exist on disk.
/// Used by the onboard wizard, CLI, and tests so rules stay in one place.
pub fn protocol_root_from_path_value(raw: &str) -> Option<PathBuf> {
    let t = raw.trim();
    if t.is_empty() || t.starts_with("http://") || t.starts_with("https://") {
        return None;
    }
    let p = PathBuf::from(t);
    if p.is_dir() {
        Some(p)
    } else {
        None
    }
}

/// Resolve local ai-protocol checkout root (not HTTP URLs).
pub fn resolve_local_protocol_root() -> Option<PathBuf> {
    let raw = std::env::var(ENV_PROTOCOL_DIR)
        .ok()
        .or_else(|| std::env::var(ENV_PROTOCOL_PATH).ok())?;
    protocol_root_from_path_value(&raw)
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
        let required_envs = ai_lib_rust::credentials::required_envs(&manifest);
        let has_auth = ai_lib_rust::credentials::primary_auth(&manifest).is_some();
        let resolved = ai_lib_rust::credentials::resolve_credential(&manifest, None);
        let available = !has_auth || resolved.secret().is_some();
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
    use std::fs;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        key: &'static str,
        old: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: Option<&str>) -> Self {
            let old = std::env::var(key).ok();
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
            Self { key, old }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.old.as_ref() {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[test]
    fn scan_empty_dir_yields_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let snap = scan_protocol_root(dir.path()).expect("scan");
        assert!(snap.providers.is_empty());
        assert!(snap.models.is_empty());
    }

    #[test]
    fn protocol_root_from_path_rejects_http_urls() {
        assert!(protocol_root_from_path_value("https://example.com/proto").is_none());
        assert!(protocol_root_from_path_value("http://localhost/x").is_none());
    }

    #[test]
    fn protocol_root_from_path_accepts_existing_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path();
        let got = protocol_root_from_path_value(p.to_str().expect("utf8 path"));
        assert_eq!(got.as_deref(), Some(p));
    }

    #[test]
    fn scan_provider_uses_ai_lib_endpoint_auth_availability() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _env = EnvGuard::set("ZEROSPIDER_PT074_TOKEN", Some("test-token"));
        let dir = tempfile::tempdir().expect("tempdir");
        let providers = dir.path().join("v2").join("providers");
        fs::create_dir_all(&providers).expect("provider dir");
        fs::write(
            providers.join("pt074.yaml"),
            r#"
id: pt074
protocol_version: v2-alpha
provider_id: pt074-provider
name: PT-074 Provider
version: v2
status: stable
category: ai_provider
official_url: https://example.com
support_contact: support@example.com
capabilities: [chat]
endpoint:
  base_url: https://example.com/v1
  auth:
    type: bearer
    token_env: ZEROSPIDER_PT074_TOKEN
"#,
        )
        .expect("manifest");

        let snap = scan_protocol_root(dir.path()).expect("scan");
        let provider = snap
            .providers
            .iter()
            .find(|provider| provider.id == "pt074")
            .expect("provider");
        assert_eq!(provider.required_envs, vec!["ZEROSPIDER_PT074_TOKEN"]);
        assert!(provider.available);
    }

    #[test]
    fn scan_provider_uses_ai_lib_conventional_env_fallback() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _manifest_env = EnvGuard::set("ZEROSPIDER_PT074_MISSING_TOKEN", None);
        let _conventional_env = EnvGuard::set("PT074_PROVIDER_API_KEY", Some("test-token"));
        let dir = tempfile::tempdir().expect("tempdir");
        let providers = dir.path().join("v2").join("providers");
        fs::create_dir_all(&providers).expect("provider dir");
        fs::write(
            providers.join("pt074.yaml"),
            r#"
id: pt074
protocol_version: v2-alpha
provider_id: pt074-provider
name: PT-074 Provider
version: v2
status: stable
category: ai_provider
official_url: https://example.com
support_contact: support@example.com
capabilities: [chat]
endpoint:
  base_url: https://example.com/v1
  auth:
    type: bearer
    token_env: ZEROSPIDER_PT074_MISSING_TOKEN
"#,
        )
        .expect("manifest");

        let snap = scan_protocol_root(dir.path()).expect("scan");
        let provider = snap
            .providers
            .iter()
            .find(|provider| provider.id == "pt074")
            .expect("provider");
        assert_eq!(
            provider.required_envs,
            vec!["ZEROSPIDER_PT074_MISSING_TOKEN"]
        );
        assert!(provider.available);
    }
}
