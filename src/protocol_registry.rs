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

/// Provider manifests under `dist/v2` → `v2` → `dist/v1` → `v1` (first stem wins).
pub(crate) fn collect_provider_files(root: &Path) -> Vec<PathBuf> {
    // Higher-priority directories first; one manifest per provider stem.
    let candidates = [
        root.join("dist").join("v2").join("providers"),
        root.join("v2").join("providers"),
        root.join("dist").join("v1").join("providers"),
        root.join("v1").join("providers"),
    ];
    let mut by_stem: BTreeMap<String, PathBuf> = BTreeMap::new();
    for dir in candidates {
        if !dir.is_dir() {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for ent in rd.flatten() {
            let path = ent.path();
            let ext = path.extension().and_then(|s| s.to_str());
            let ok = path.is_file() && matches!(ext, Some("json" | "yaml" | "yml"));
            if !ok {
                continue;
            }
            let Some(stem) = provider_id_from_path(&path) else {
                continue;
            };
            by_stem.entry(stem).or_insert(path);
        }
    }
    by_stem.into_values().collect()
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProtocolRegistrySnapshot {
    pub protocol_root: PathBuf,
    pub providers: Vec<ProtocolProviderInfo>,
    pub models: Vec<ProtocolModelInfo>,
}

fn context_window_from_meta(meta: &serde_json::Value) -> Option<u32> {
    meta.get("context_window")
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
}

pub(crate) fn load_manifest_value(path: &Path) -> anyhow::Result<serde_json::Value> {
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
    let bytes = std::fs::read(path)?;
    if ext.eq_ignore_ascii_case("json") {
        return Ok(serde_json::from_slice(&bytes)?);
    }
    if ext.eq_ignore_ascii_case("yaml") || ext.eq_ignore_ascii_case("yml") {
        let s = String::from_utf8_lossy(&bytes);
        let v: serde_yaml::Value = serde_yaml::from_str(&s)?;
        return Ok(serde_json::to_value(v)?);
    }
    anyhow::bail!("unsupported provider manifest extension: {ext}");
}

fn upsert_model(models: &mut Vec<ProtocolModelInfo>, entry: ProtocolModelInfo) {
    if let Some(existing) = models.iter_mut().find(|m| m.logical_id == entry.logical_id) {
        if existing.context_window.is_none() {
            existing.context_window = entry.context_window;
        }
        return;
    }
    models.push(entry);
}

/// If `model` (or a suffix/prefix form) is listed under provider `deprecated`
/// with `maps_to`, return that successor wire id.
///
/// PT-NIM-002 tombstones stay in the catalog for old config; hosts must rewrite
/// before the first HTTP hop so NIM HTTP 410 is not the live primary.
#[must_use]
pub fn maps_to_from_manifest_value(raw: &serde_json::Value, model: &str) -> Option<String> {
    let model = model.trim();
    if model.is_empty() {
        return None;
    }
    let suffix = model.rsplit('/').next().unwrap_or(model);
    let dep = raw.get("deprecated").and_then(|v| v.as_object())?;
    for (key, spec) in dep {
        let hit = key == model
            || key == suffix
            || model.ends_with(key)
            || key.ends_with(&format!("/{suffix}"))
            || key.ends_with(suffix);
        if !hit {
            continue;
        }
        let maps = spec.get("maps_to").and_then(|v| v.as_str())?.trim();
        if maps.is_empty() {
            continue;
        }
        return Some(maps.to_string());
    }
    None
}

/// Scan `$AI_PROTOCOL_DIR` provider manifests for a `deprecated.maps_to` successor.
#[must_use]
pub fn protocol_maps_to(model: &str) -> Option<String> {
    let root = resolve_local_protocol_root()?;
    for path in collect_provider_files(&root) {
        let Ok(raw) = load_manifest_value(&path) else {
            continue;
        };
        if let Some(succ) = maps_to_from_manifest_value(&raw, model) {
            return Some(succ);
        }
    }
    None
}

/// Rewrite a routed `(provider, model)` pair when the model id is tombstoned.
#[must_use]
pub fn rewrite_tombstoned_route(provider: &str, model: &str) -> (String, String) {
    let Some(succ) = protocol_maps_to(model).or_else(|| protocol_maps_to(provider)) else {
        return (provider.to_string(), model.to_string());
    };
    let old_tail = model.rsplit('/').next().unwrap_or(model);
    let new_tail = succ.rsplit('/').next().unwrap_or(succ.as_str());
    let new_model = if succ.contains('/') {
        succ.clone()
    } else if let Some((prefix, _)) = model.rsplit_once('/') {
        format!("{prefix}/{succ}")
    } else {
        succ.clone()
    };
    let new_provider = provider.replace(old_tail, new_tail);
    tracing::info!(
        target: "protocol_registry",
        from_model = model,
        to_model = new_model.as_str(),
        "rewrote tombstoned route via protocol maps_to"
    );
    (new_provider, new_model)
}

/// Compose a host logical id from provider + protocol catalog/wire key.
///
/// Protocol YAML keys stay wire/catalog ids (e.g. `deepseek-ai/deepseek-v4-flash`
/// under nvidia). Host lists and routing use `provider/wire` so the first
/// slash segment is always a real provider id. Keys that already start with
/// `{provider}/` are left unchanged (no double prefix).
#[must_use]
pub fn compose_logical_model_id(provider_id: &str, wire_or_key: &str) -> String {
    let provider_id = provider_id.trim();
    let wire_or_key = wire_or_key.trim();
    if provider_id.is_empty() {
        return wire_or_key.to_string();
    }
    if wire_or_key.is_empty() {
        return provider_id.to_string();
    }
    if wire_or_key == provider_id || wire_or_key.starts_with(&format!("{provider_id}/")) {
        wire_or_key.to_string()
    } else {
        format!("{provider_id}/{wire_or_key}")
    }
}

fn ingest_provider_metadata_models(
    models: &mut Vec<ProtocolModelInfo>,
    provider_id: &str,
    path: &Path,
) -> bool {
    let Ok(raw) = load_manifest_value(path) else {
        return false;
    };
    let Some(metadata_models) = raw
        .get("metadata")
        .and_then(|m| m.get("models"))
        .and_then(serde_json::Value::as_object)
    else {
        return false;
    };
    for (model_key, meta) in metadata_models {
        let logical_id = compose_logical_model_id(provider_id, model_key);
        upsert_model(
            models,
            ProtocolModelInfo {
                logical_id,
                provider: provider_id.to_string(),
                source_file: path.to_path_buf(),
                context_window: context_window_from_meta(meta),
            },
        );
    }
    true
}

fn provider_id_from_manifest_value(raw: &serde_json::Value, stem: &str) -> String {
    raw.get("id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(stem)
        .to_string()
}

impl ProtocolRegistrySnapshot {
    /// Resolve `context_window` tokens for a logical model id (exact or suffix match).
    #[must_use]
    pub fn context_window_for(&self, model_id: &str) -> Option<u32> {
        let model_id = model_id.trim();
        if model_id.is_empty() {
            return None;
        }
        if let Some(found) = self
            .models
            .iter()
            .find(|m| m.logical_id == model_id)
            .and_then(|m| m.context_window)
        {
            return Some(found);
        }
        let suffix = model_id.rsplit('/').next().unwrap_or(model_id);
        self.models
            .iter()
            .filter(|m| {
                m.logical_id == suffix
                    || m.logical_id.ends_with(&format!("/{suffix}"))
                    || m.logical_id == format!("{}/{}", m.provider, suffix)
            })
            .find_map(|m| m.context_window)
    }

    /// Find a provider by id (case-sensitive exact match).
    #[must_use]
    pub fn provider_by_id(&self, provider_id: &str) -> Option<&ProtocolProviderInfo> {
        let provider_id = provider_id.trim();
        self.providers.iter().find(|p| p.id == provider_id)
    }

    /// Resolve a logical model id (exact, or `provider/model` suffix forms).
    #[must_use]
    pub fn model_by_logical_id(&self, model_id: &str) -> Option<&ProtocolModelInfo> {
        let model_id = model_id.trim();
        if model_id.is_empty() {
            return None;
        }
        if let Some(found) = self.models.iter().find(|m| m.logical_id == model_id) {
            return Some(found);
        }
        let suffix = model_id.rsplit('/').next().unwrap_or(model_id);
        self.models.iter().find(|m| {
            m.logical_id == suffix
                || m.logical_id.ends_with(&format!("/{suffix}"))
                || m.logical_id == format!("{}/{}", m.provider, suffix)
        })
    }

    /// Resolve a picker/session model id that may be a bare aggregator wire id.
    ///
    /// - Exact logical id hit → that entry.
    /// - First segment is a known provider → treat as already-composed logical.
    /// - Otherwise unique match on `logical_id == raw` or `…/{raw}` (wire remap).
    /// - Ambiguous / no match → `None` (caller keeps status quo).
    #[must_use]
    pub fn resolve_chat_model_id(&self, raw: &str) -> Option<&ProtocolModelInfo> {
        let raw = raw.trim();
        if raw.is_empty() {
            return None;
        }
        if let Some(found) = self.models.iter().find(|m| m.logical_id == raw) {
            return Some(found);
        }
        let first = provider_id_from_logical(raw);
        if self.provider_by_id(first).is_some() {
            return self.model_by_logical_id(raw);
        }
        let needle = format!("/{raw}");
        let mut hits = self
            .models
            .iter()
            .filter(|m| m.logical_id == raw || m.logical_id.ends_with(&needle));
        let first_hit = hits.next()?;
        if hits.next().is_some() {
            return None;
        }
        Some(first_hit)
    }
}

/// Whether a provider manifest exposes a chat-capable endpoint key.
///
/// Accepts `endpoints.chat` or `endpoints.chat_openai` (ai-lib chat op aliases).
/// Returns `None` when the file cannot be parsed or has no `endpoints` map.
#[must_use]
pub fn manifest_has_chat_endpoint(path: &Path) -> Option<bool> {
    let raw = load_manifest_value(path).ok()?;
    let endpoints = raw.get("endpoints")?.as_object()?;
    Some(endpoints.contains_key("chat") || endpoints.contains_key("chat_openai"))
}

/// Provider id segment from `provider`, `provider/model`, or `protocol:provider/model`.
#[must_use]
pub fn provider_id_from_logical(raw: &str) -> &str {
    let raw = raw.trim();
    let raw = raw.strip_prefix("protocol:").map(str::trim).unwrap_or(raw);
    raw.split_once('/').map(|(p, _)| p).unwrap_or(raw)
}

/// Map `hint:<name>` to the `[[model_routes]]` logical model; otherwise return `model_or_hint`.
#[must_use]
pub fn resolve_route_logical_model(
    model_or_hint: &str,
    routes: &[crate::config::ModelRouteConfig],
) -> String {
    let raw = model_or_hint.trim();
    if let Some(hint) = raw.strip_prefix("hint:") {
        if let Some(route) = routes
            .iter()
            .find(|r| r.hint.eq_ignore_ascii_case(hint.trim()))
        {
            return route.model.clone();
        }
    }
    raw.to_string()
}

fn route_row_matches_raw(raw: &str, provider: &str, model: &str) -> bool {
    let logical = compose_logical_model_id(provider, model);
    raw.eq_ignore_ascii_case(logical.trim()) || raw.eq_ignore_ascii_case(model.trim())
}

/// Stable logical id so `hint:code` and its route / fallback peers compare equal.
#[must_use]
pub fn physical_route_key(
    model_or_hint: &str,
    routes: &[crate::config::ModelRouteConfig],
) -> String {
    let raw = model_or_hint.trim();
    if let Some(hint) = raw.strip_prefix("hint:") {
        if let Some(route) = routes
            .iter()
            .find(|r| r.hint.eq_ignore_ascii_case(hint.trim()))
        {
            return compose_logical_model_id(&route.provider, &route.model);
        }
    }
    for route in routes {
        if route_row_matches_raw(raw, &route.provider, &route.model) {
            return compose_logical_model_id(&route.provider, &route.model);
        }
        for peer in &route.fallbacks {
            if route_row_matches_raw(raw, &peer.provider, &peer.model) {
                return compose_logical_model_id(&peer.provider, &peer.model);
            }
        }
    }
    raw.to_string()
}

/// Protocol `context_window` for this hop (`hint:` resolved via `[[model_routes]]`).
#[must_use]
pub fn lookup_hop_context_window(
    model_or_hint: &str,
    routes: &[crate::config::ModelRouteConfig],
) -> Option<u32> {
    lookup_context_window(&resolve_route_logical_model(model_or_hint, routes))
}

/// Lookup `context_window` for a model from the local ai-protocol registry cache.
#[must_use]
pub fn lookup_context_window(model_id: &str) -> Option<u32> {
    #[cfg(feature = "ai-protocol")]
    {
        use std::sync::OnceLock;
        static CACHE: OnceLock<Option<ProtocolRegistrySnapshot>> = OnceLock::new();
        let snap = CACHE.get_or_init(|| {
            resolve_local_protocol_root().and_then(|root| scan_protocol_root(&root).ok())
        });
        snap.as_ref()?.context_window_for(model_id)
    }
    #[cfg(not(feature = "ai-protocol"))]
    {
        let _ = model_id;
        None
    }
}

/// Scan provider manifests under `root` and model registries under `v1/models` / `dist/v1/models`.
pub fn scan_protocol_root(root: &Path) -> anyhow::Result<ProtocolRegistrySnapshot> {
    let mut providers = Vec::new();
    let mut models = Vec::new();
    for path in collect_provider_files(root) {
        let Some(stem_id) = provider_id_from_path(&path) else {
            continue;
        };
        match load_provider_manifest(&path) {
            Ok(manifest) => {
                let required_envs = ai_lib_rust::credentials::required_envs(&manifest);
                let has_auth = ai_lib_rust::credentials::primary_auth(&manifest).is_some();
                let available = !has_auth
                    || ai_lib_rust::credentials::resolve_credential(&manifest, None)
                        .secret()
                        .is_some();
                let resolved_id = if manifest.id.trim().is_empty() {
                    stem_id.clone()
                } else {
                    manifest.id.clone()
                };
                providers.push(ProtocolProviderInfo {
                    id: resolved_id.clone(),
                    manifest_path: path.clone(),
                    required_envs,
                    available,
                });
                ingest_provider_metadata_models(&mut models, &resolved_id, &path);
            }
            Err(e) => {
                let Ok(raw) = load_manifest_value(&path) else {
                    tracing::warn!(path = %path.display(), "skip invalid provider manifest: {e}");
                    continue;
                };
                let resolved_id = provider_id_from_manifest_value(&raw, &stem_id);
                if ingest_provider_metadata_models(&mut models, &resolved_id, &path) {
                    tracing::debug!(
                        path = %path.display(),
                        provider = %resolved_id,
                        error = %e,
                        "provider manifest skipped strict validation; indexed metadata.models only"
                    );
                } else {
                    tracing::warn!(path = %path.display(), "skip invalid provider manifest: {e}");
                }
            }
        }
    }
    providers.sort_by(|a, b| a.id.cmp(&b.id));

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

            for (map_key, meta) in reg {
                let provider = meta
                    .get("provider")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                let wire = meta
                    .get("model_id")
                    .and_then(|x| x.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(map_key.as_str());
                let logical_id = if provider.is_empty() {
                    map_key
                } else {
                    compose_logical_model_id(&provider, wire)
                };
                upsert_model(
                    &mut models,
                    ProtocolModelInfo {
                        logical_id,
                        provider,
                        source_file: path.clone(),
                        context_window: context_window_from_meta(&meta),
                    },
                );
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

/// Experimental (VL-GEN-001): inspect a model's generative capability + L-Exec path.
///
/// Fail-closed: omitted `model_capabilities.<key>` is not treated as true.
/// Does not call vendor HTTP.
#[derive(Debug, Clone, Serialize)]
pub struct GenerativeCapabilityInspect {
    pub logical_id: String,
    pub provider: String,
    pub model: String,
    pub capability: String,
    pub capability_declared: bool,
    pub endpoint_path: Option<String>,
    pub adapter: Option<String>,
    pub allowed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fail_closed_reason: Option<String>,
}

const GENERATIVE_KEYS: &[&str] = &["image_generation", "speech_to_text", "text_to_speech"];

fn parse_generative_capability(capability: &str) -> anyhow::Result<&str> {
    let capability = capability.trim();
    GENERATIVE_KEYS
        .iter()
        .copied()
        .find(|k| *k == capability)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "unknown generative capability `{capability}`; expected one of {}",
                GENERATIVE_KEYS.join(", ")
            )
        })
}

fn inspect_loaded(
    manifest: &ProtocolManifest,
    provider: &str,
    model: &str,
    capability: &str,
) -> GenerativeCapabilityInspect {
    let capability_declared = manifest.supports_generative_for_model(model, capability);
    // Inspect-only: do not enable `ai-lib-rust/generative` (HTTP drivers).
    let (endpoint_path, adapter) = match manifest.endpoints.as_ref().and_then(|e| e.get(capability))
    {
        Some(ep) => (Some(ep.path.clone()), ep.adapter.clone()),
        None => (None, None),
    };
    let allowed = capability_declared && endpoint_path.is_some();
    let fail_closed_reason = if allowed {
        None
    } else if !capability_declared {
        Some(format!(
            "model `{model}` does not declare model_capabilities.{capability}=true (omit≠false fail-closed)"
        ))
    } else {
        Some(format!(
            "manifest endpoints.{capability} missing; declare PT-GEN-002 L-Exec map"
        ))
    };
    GenerativeCapabilityInspect {
        logical_id: compose_logical_model_id(provider, model),
        provider: provider.to_string(),
        model: model.to_string(),
        capability: capability.to_string(),
        capability_declared,
        endpoint_path,
        adapter,
        allowed,
        fail_closed_reason,
    }
}

/// Inspect `provider/model` + PT-GEN capability key against local manifests.
pub fn inspect_generative_capability(
    root: &Path,
    logical: &str,
    capability: &str,
) -> anyhow::Result<GenerativeCapabilityInspect> {
    let capability = parse_generative_capability(capability)?;
    let provider = provider_id_from_logical(logical).to_string();
    let model = {
        let raw = logical.trim();
        let raw = raw.strip_prefix("protocol:").map(str::trim).unwrap_or(raw);
        raw.split_once('/')
            .map(|(_, m)| m.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("expected provider/model logical id, got `{logical}`"))?
    };

    let mut found: Option<ProtocolManifest> = None;
    for path in collect_provider_files(root) {
        let Some(stem) = provider_id_from_path(&path) else {
            continue;
        };
        let Ok(manifest) = load_provider_manifest(&path) else {
            continue;
        };
        let id = if manifest.id.trim().is_empty() {
            stem.clone()
        } else {
            manifest.id.clone()
        };
        if id == provider || stem == provider {
            found = Some(manifest);
            break;
        }
    }
    let Some(manifest) = found else {
        anyhow::bail!(
            "no provider manifest for `{provider}` under {}",
            root.display()
        );
    };

    Ok(inspect_loaded(&manifest, &provider, &model, capability))
}

/// One-pass listing of PT-GEN inspect rows for every `metadata.models` key.
pub fn list_generative_capabilities(
    root: &Path,
    capability_filter: Option<&str>,
) -> anyhow::Result<Vec<GenerativeCapabilityInspect>> {
    let keys: Vec<&str> = match capability_filter {
        Some(raw) => vec![parse_generative_capability(raw)?],
        None => GENERATIVE_KEYS.to_vec(),
    };
    let mut out = Vec::new();
    for path in collect_provider_files(root) {
        let Some(stem) = provider_id_from_path(&path) else {
            continue;
        };
        let Ok(manifest) = load_provider_manifest(&path) else {
            continue;
        };
        let provider = if manifest.id.trim().is_empty() {
            stem
        } else {
            manifest.id.clone()
        };
        let Ok(raw) = load_manifest_value(&path) else {
            continue;
        };
        let Some(models) = raw
            .get("metadata")
            .and_then(|m| m.get("models"))
            .and_then(serde_json::Value::as_object)
        else {
            continue;
        };
        for model_key in models.keys() {
            for cap in &keys {
                out.push(inspect_loaded(&manifest, &provider, model_key, cap));
            }
        }
    }
    out.sort_by(|a, b| {
        a.logical_id
            .cmp(&b.logical_id)
            .then_with(|| a.capability.cmp(&b.capability))
    });
    Ok(out)
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
        let _env = EnvGuard::set("VELACLAW_PT074_TOKEN", Some("test-token"));
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
    token_env: VELACLAW_PT074_TOKEN
"#,
        )
        .expect("manifest");

        let snap = scan_protocol_root(dir.path()).expect("scan");
        let provider = snap
            .providers
            .iter()
            .find(|provider| provider.id == "pt074")
            .expect("provider");
        assert_eq!(provider.required_envs, vec!["VELACLAW_PT074_TOKEN"]);
        assert!(provider.available);
    }

    #[test]
    fn scan_provider_uses_ai_lib_conventional_env_fallback() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _manifest_env = EnvGuard::set("VELACLAW_PT074_MISSING_TOKEN", None);
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
    token_env: VELACLAW_PT074_MISSING_TOKEN
"#,
        )
        .expect("manifest");

        let snap = scan_protocol_root(dir.path()).expect("scan");
        let provider = snap
            .providers
            .iter()
            .find(|provider| provider.id == "pt074")
            .expect("provider");
        assert_eq!(provider.required_envs, vec!["VELACLAW_PT074_MISSING_TOKEN"]);
        assert!(provider.available);
    }

    #[test]
    fn resolve_route_logical_model_maps_hint() {
        let routes = [crate::config::ModelRouteConfig {
            hint: "code".into(),
            provider: "deepseek".into(),
            model: "deepseek/deepseek-v4-flash".into(),
            api_key: None,
            fallbacks: Vec::new(),
        }];
        assert_eq!(
            resolve_route_logical_model("hint:code", &routes),
            "deepseek/deepseek-v4-flash"
        );
        assert_eq!(
            resolve_route_logical_model("deepseek/deepseek-v4-flash", &routes),
            "deepseek/deepseek-v4-flash"
        );
        assert_eq!(
            resolve_route_logical_model("hint:unknown", &routes),
            "hint:unknown"
        );
        assert_eq!(
            physical_route_key("hint:code", &routes),
            physical_route_key("deepseek/deepseek-v4-flash", &routes)
        );
        assert_ne!(
            physical_route_key("deepseek", &routes),
            physical_route_key("hint:code", &routes)
        );
    }

    #[test]
    fn compose_logical_model_id_prefixes_wire_keys() {
        assert_eq!(
            compose_logical_model_id("nvidia", "deepseek-ai/deepseek-v4-flash"),
            "nvidia/deepseek-ai/deepseek-v4-flash"
        );
        assert_eq!(
            compose_logical_model_id("nvidia", "nvidia/nemotron-mini-4b-instruct"),
            "nvidia/nemotron-mini-4b-instruct"
        );
        assert_eq!(
            compose_logical_model_id("openai", "gpt-4o"),
            "openai/gpt-4o"
        );
        assert_eq!(
            compose_logical_model_id("deepseek", "deepseek-v4-flash"),
            "deepseek/deepseek-v4-flash"
        );
        assert_eq!(compose_logical_model_id("", "bare/wire"), "bare/wire");
    }

    #[test]
    fn scan_lenient_manifest_without_status_indexes_metadata_models() {
        let dir = tempfile::tempdir().expect("tempdir");
        let providers = dir.path().join("v2").join("providers");
        fs::create_dir_all(&providers).expect("provider dir");
        fs::write(
            providers.join("azure.yaml"),
            r#"
id: azure
name: Azure
metadata:
  models:
    gpt-4o:
      context_window: 128000
"#,
        )
        .expect("manifest");

        let snap = scan_protocol_root(dir.path()).expect("scan");
        assert!(
            snap.providers.is_empty(),
            "strict parse should skip provider entry without status"
        );
        assert_eq!(snap.context_window_for("azure/gpt-4o"), Some(128_000));
    }

    #[test]
    fn scan_composes_org_qualified_wire_keys_under_provider() {
        let dir = tempfile::tempdir().expect("tempdir");
        let providers = dir.path().join("v2").join("providers");
        fs::create_dir_all(&providers).expect("provider dir");
        fs::write(
            providers.join("nvidia.yaml"),
            r#"
id: nvidia
name: NVIDIA
metadata:
  models:
    deepseek-ai/deepseek-v4-flash:
      context_window: 1000000
    nvidia/nemotron-mini-4b-instruct:
      context_window: 4096
"#,
        )
        .expect("manifest");

        let models_dir = dir.path().join("v1").join("models");
        fs::create_dir_all(&models_dir).expect("models dir");
        fs::write(
            models_dir.join("nvidia.yaml"),
            r#"
models:
  "deepseek-ai/deepseek-v4-pro":
    provider: nvidia
    model_id: "deepseek-ai/deepseek-v4-pro"
    context_window: 1000000
"#,
        )
        .expect("v1 registry");

        let snap = scan_protocol_root(dir.path()).expect("scan");
        assert!(snap
            .models
            .iter()
            .any(|m| m.logical_id == "nvidia/deepseek-ai/deepseek-v4-flash"
                && m.provider == "nvidia"));
        assert!(snap
            .models
            .iter()
            .any(|m| m.logical_id == "nvidia/nemotron-mini-4b-instruct"));
        assert!(snap.models.iter().any(
            |m| m.logical_id == "nvidia/deepseek-ai/deepseek-v4-pro" && m.provider == "nvidia"
        ));
        assert!(!snap
            .models
            .iter()
            .any(|m| m.logical_id == "deepseek-ai/deepseek-v4-flash"));

        let remapped = snap
            .resolve_chat_model_id("deepseek-ai/deepseek-v4-flash")
            .expect("wire remap");
        assert_eq!(remapped.logical_id, "nvidia/deepseek-ai/deepseek-v4-flash");
        assert_eq!(remapped.provider, "nvidia");
    }

    #[test]
    fn scan_provider_metadata_models_extracts_context_window() {
        let fixture =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ai-protocol-min");
        let snap = scan_protocol_root(&fixture).expect("scan fixture");
        let cw = snap
            .context_window_for("openai/gpt-5.3-codex-spark")
            .or_else(|| snap.context_window_for("gpt-5.3-codex-spark"));
        assert_eq!(cw, Some(128_000));
    }

    #[test]
    fn provider_and_model_lookup_helpers() {
        let fixture =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ai-protocol-min");
        let snap = scan_protocol_root(&fixture).expect("scan fixture");
        assert!(snap.provider_by_id("openai").is_some());
        assert!(snap
            .model_by_logical_id("openai/gpt-5.3-codex-spark")
            .is_some());
        assert_eq!(
            provider_id_from_logical("deepseek/deepseek-v4-flash"),
            "deepseek"
        );
        assert_eq!(provider_id_from_logical("deepseek"), "deepseek");
        assert_eq!(
            provider_id_from_logical("protocol:openai/gpt-5.2"),
            "openai"
        );
        assert_eq!(provider_id_from_logical("protocol:deepseek"), "deepseek");
    }

    #[test]
    fn manifest_has_chat_endpoint_detects_chat_keys() {
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/ai-protocol-min/v2/providers/openai.yaml");
        assert_eq!(manifest_has_chat_endpoint(&fixture), Some(true));

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nochat.yaml");
        fs::write(
            &path,
            r#"
id: nochat
endpoints:
  embeddings:
    path: /v1/embeddings
"#,
        )
        .expect("write");
    }

    #[test]
    fn inspect_generative_fail_closed_on_omit_and_allows_declared() {
        let dir = tempfile::tempdir().expect("tempdir");
        let providers = dir.path().join("v2").join("providers");
        fs::create_dir_all(&providers).expect("provider dir");
        fs::write(
            providers.join("genprov.yaml"),
            r#"
id: genprov
protocol_version: v2-alpha
provider_id: genprov
name: Gen
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
    token_env: VELACLAW_GEN_TOKEN
endpoints:
  image_generation:
    path: /images/generations
    method: POST
    adapter: openai
metadata:
  models:
    img-1:
      model_capabilities:
        image_generation: true
    chat-1:
      context_window: 128
"#,
        )
        .expect("manifest");

        let ok = inspect_generative_capability(dir.path(), "genprov/img-1", "image_generation")
            .expect("inspect img");
        assert!(ok.allowed);
        assert_eq!(ok.endpoint_path.as_deref(), Some("/images/generations"));

        let omit = inspect_generative_capability(dir.path(), "genprov/chat-1", "image_generation")
            .expect("inspect omit");
        assert!(!omit.allowed);
        assert!(!omit.capability_declared);
        assert!(omit
            .fail_closed_reason
            .as_deref()
            .unwrap_or("")
            .contains("omit"));

        let unknown =
            inspect_generative_capability(dir.path(), "genprov/img-1", "video_generation");
        assert!(unknown.is_err());
    }

    #[test]
    fn inspect_generative_declared_without_lexec_fails_closed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let providers = dir.path().join("v2").join("providers");
        fs::create_dir_all(&providers).expect("provider dir");
        fs::write(
            providers.join("genprov.yaml"),
            r#"
id: genprov
protocol_version: v2-alpha
provider_id: genprov
name: Gen
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
    token_env: VELACLAW_GEN_TOKEN
metadata:
  models:
    img-1:
      model_capabilities:
        image_generation: true
"#,
        )
        .expect("manifest");

        let out = inspect_generative_capability(dir.path(), "genprov/img-1", "image_generation")
            .expect("inspect");
        assert!(out.capability_declared);
        assert!(!out.allowed);
        assert!(out.endpoint_path.is_none());
        assert!(out
            .fail_closed_reason
            .as_deref()
            .unwrap_or("")
            .contains("endpoints.image_generation"));
    }

    #[test]
    fn list_generative_capabilities_one_pass_fail_closed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let providers = dir.path().join("v2").join("providers");
        fs::create_dir_all(&providers).expect("provider dir");
        fs::write(
            providers.join("genprov.yaml"),
            r#"
id: genprov
protocol_version: v2-alpha
provider_id: genprov
name: Gen
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
    token_env: VELACLAW_GEN_TOKEN
endpoints:
  image_generation:
    path: /images/generations
    method: POST
    adapter: openai
metadata:
  models:
    img-1:
      model_capabilities:
        image_generation: true
    chat-1:
      context_window: 128
"#,
        )
        .expect("manifest");

        let rows =
            list_generative_capabilities(dir.path(), Some("image_generation")).expect("list image");
        assert_eq!(rows.len(), 2);
        let img = rows.iter().find(|r| r.model == "img-1").expect("img-1");
        assert!(img.allowed);
        assert_eq!(img.endpoint_path.as_deref(), Some("/images/generations"));
        let chat = rows.iter().find(|r| r.model == "chat-1").expect("chat-1");
        assert!(!chat.allowed);
        assert!(!chat.capability_declared);

        let err = list_generative_capabilities(dir.path(), Some("video_generation"));
        assert!(err.is_err());

        let all = list_generative_capabilities(dir.path(), None).expect("list all");
        assert_eq!(all.len(), 6);
        assert!(all
            .iter()
            .any(|r| r.model == "img-1" && r.capability == "speech_to_text" && !r.allowed));
    }

    #[test]
    fn maps_to_rewrites_nemotron_49b_v15_tombstone() {
        let raw = serde_json::json!({
            "deprecated": {
                "nvidia/llama-3.3-nemotron-super-49b-v1.5": {
                    "maps_to": "nvidia/llama-3.1-nemotron-70b-instruct"
                }
            }
        });
        assert_eq!(
            maps_to_from_manifest_value(&raw, "nvidia/llama-3.3-nemotron-super-49b-v1.5")
                .as_deref(),
            Some("nvidia/llama-3.1-nemotron-70b-instruct")
        );
        assert_eq!(
            maps_to_from_manifest_value(&raw, "llama-3.3-nemotron-super-49b-v1.5").as_deref(),
            Some("nvidia/llama-3.1-nemotron-70b-instruct")
        );
        assert!(
            maps_to_from_manifest_value(&raw, "nvidia/llama-3.1-nemotron-70b-instruct").is_none()
        );
    }
}
