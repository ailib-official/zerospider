pub mod backend;
pub mod chunker;
pub mod cli;
pub mod embeddings;
pub mod hygiene;
pub mod lucid;
pub mod markdown;
pub mod none;
#[cfg(feature = "memory-postgres")]
pub mod postgres;
pub mod response_cache;
pub mod snapshot;
pub mod sqlite;
pub mod traits;
pub mod vector;

#[allow(unused_imports)]
pub use backend::{
    classify_memory_backend, default_memory_backend_key, memory_backend_profile,
    selectable_memory_backends, MemoryBackendKind, MemoryBackendProfile,
};
pub use lucid::LucidMemory;
pub use markdown::MarkdownMemory;
pub use none::NoneMemory;
#[cfg(feature = "memory-postgres")]
pub use postgres::PostgresMemory;
pub use response_cache::ResponseCache;
pub use sqlite::SqliteMemory;
pub use traits::Memory;
#[allow(unused_imports)]
pub use traits::{MemoryCategory, MemoryEntry};

use crate::config::{EmbeddingRouteConfig, MemoryConfig, StorageProviderConfig};
#[cfg(feature = "memory-postgres")]
use anyhow::Context;
use std::path::Path;
use std::sync::Arc;

fn create_memory_with_builders<F, G>(
    backend_name: &str,
    workspace_dir: &Path,
    mut sqlite_builder: F,
    mut postgres_builder: G,
    unknown_context: &str,
) -> anyhow::Result<Box<dyn Memory>>
where
    F: FnMut() -> anyhow::Result<SqliteMemory>,
    G: FnMut() -> anyhow::Result<Box<dyn Memory>>,
{
    match classify_memory_backend(backend_name) {
        MemoryBackendKind::Sqlite => Ok(Box::new(sqlite_builder()?)),
        MemoryBackendKind::Lucid => {
            let local = sqlite_builder()?;
            Ok(Box::new(LucidMemory::new(workspace_dir, local)))
        }
        MemoryBackendKind::Postgres => postgres_builder(),
        MemoryBackendKind::Markdown => Ok(Box::new(MarkdownMemory::new(workspace_dir))),
        MemoryBackendKind::None => Ok(Box::new(NoneMemory::new())),
        MemoryBackendKind::Unknown => {
            tracing::warn!(
                "Unknown memory backend '{backend_name}'{unknown_context}, falling back to markdown"
            );
            Ok(Box::new(MarkdownMemory::new(workspace_dir)))
        }
    }
}

pub fn effective_memory_backend_name(
    memory_backend: &str,
    storage_provider: Option<&StorageProviderConfig>,
) -> String {
    if let Some(override_provider) = storage_provider
        .map(|cfg| cfg.provider.trim())
        .filter(|provider| !provider.is_empty())
    {
        return override_provider.to_ascii_lowercase();
    }

    memory_backend.trim().to_ascii_lowercase()
}

/// Legacy auto-save key used for model-authored assistant summaries.
/// These entries are treated as untrusted context and should not be re-injected.
pub fn is_assistant_autosave_key(key: &str) -> bool {
    let normalized = key.trim().to_ascii_lowercase();
    normalized == "assistant_resp" || normalized.starts_with("assistant_resp_")
}

/// Whether a recalled entry may be injected into the current CLI/agent session.
///
/// - `Core` always injects (long-term facts; matches `/new` preservation).
/// - `Conversation` / `Daily` / `Custom` inject only when `entry.session_id`
///   equals `current_session`.
/// - Legacy rows with `session_id = None` are treated as other-session and
///   excluded when a current session is active (stops cross-session bleed).
#[must_use]
pub fn should_inject_for_session(entry: &MemoryEntry, current_session: Option<&str>) -> bool {
    // DAG internodal keys are session-namespaced; never treat another graph's
    // `dag_art:` row as Core-like global knowledge (VL-NA-041).
    if let Some(current) = current_session {
        if let Some(rest) = entry.key.strip_prefix("dag_art:") {
            if !rest.starts_with(&format!("{current}:")) {
                return false;
            }
        }
    }
    if matches!(entry.category, MemoryCategory::Core) {
        return true;
    }
    match (current_session, entry.session_id.as_deref()) {
        (Some(current), Some(entry_sid)) => current == entry_sid,
        // No active session filter → keep prior global behavior for callers
        // that have not opted into session scoping.
        (None, _) => true,
        // Active session + legacy/other-session row → do not inject.
        (Some(_), None) => false,
    }
}

/// How many newest Conversation rows to keep when consolidating.
pub const CONSOLIDATION_CONVERSATION_KEEP: usize = 2;

/// Deterministic Conversation fold. **Not** hygiene (file archive) and **not** LLM summarization.
///
/// Keeps every Core decision; collapses older Conversation rows into one summary entry.
#[must_use]
pub fn consolidate_entries(entries: &[MemoryEntry], conversation_keep: usize) -> Vec<MemoryEntry> {
    let mut core = Vec::new();
    let mut daily = Vec::new();
    let mut conversation = Vec::new();
    let mut custom = Vec::new();
    for entry in entries {
        match entry.category {
            MemoryCategory::Core => core.push(entry.clone()),
            MemoryCategory::Daily => daily.push(entry.clone()),
            MemoryCategory::Conversation => conversation.push(entry.clone()),
            MemoryCategory::Custom(_) => custom.push(entry.clone()),
        }
    }
    conversation.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
    let folded_conversation = if conversation.len() <= conversation_keep {
        conversation
    } else {
        let drop_n = conversation.len() - conversation_keep;
        let (old, recent) = conversation.split_at(drop_n);
        let summary_body = old
            .iter()
            .map(|entry| format!("{}: {}", entry.key, entry.content))
            .collect::<Vec<_>>()
            .join("\n");
        let mut out = vec![MemoryEntry {
            id: "consolidated-conversation".into(),
            key: "consolidated_conversation".into(),
            content: summary_body,
            category: MemoryCategory::Conversation,
            timestamp: old
                .last()
                .map(|entry| entry.timestamp.clone())
                .unwrap_or_default(),
            session_id: old.first().and_then(|entry| entry.session_id.clone()),
            score: None,
        }];
        out.extend(recent.iter().cloned());
        out
    };
    let mut out = core;
    out.extend(daily);
    out.extend(custom);
    out.extend(folded_conversation);
    out
}

/// Allocate a fresh interactive / one-shot session id (VL-MEM-001).
#[must_use]
pub fn new_session_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[derive(Clone, PartialEq, Eq)]
struct ResolvedEmbeddingConfig {
    provider: String,
    model: String,
    dimensions: usize,
    api_key: Option<String>,
}

impl std::fmt::Debug for ResolvedEmbeddingConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedEmbeddingConfig")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("dimensions", &self.dimensions)
            .field("api_key", &self.api_key.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
}

fn resolve_embedding_config(
    config: &MemoryConfig,
    embedding_routes: &[EmbeddingRouteConfig],
    api_key: Option<&str>,
) -> ResolvedEmbeddingConfig {
    let fallback_api_key = api_key
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let fallback = ResolvedEmbeddingConfig {
        provider: config.embedding_provider.trim().to_string(),
        model: config.embedding_model.trim().to_string(),
        dimensions: config.embedding_dimensions,
        api_key: fallback_api_key.clone(),
    };

    let Some(hint) = config
        .embedding_model
        .strip_prefix("hint:")
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return fallback;
    };

    let Some(route) = embedding_routes
        .iter()
        .find(|route| route.hint.trim() == hint)
    else {
        tracing::warn!(
            hint,
            "Unknown embedding route hint; falling back to [memory] embedding settings"
        );
        return fallback;
    };

    let provider = route.provider.trim();
    let model = route.model.trim();
    let dimensions = route.dimensions.unwrap_or(config.embedding_dimensions);
    if provider.is_empty() || model.is_empty() || dimensions == 0 {
        tracing::warn!(
            hint,
            "Invalid embedding route configuration; falling back to [memory] embedding settings"
        );
        return fallback;
    }

    let routed_api_key = route
        .api_key
        .as_deref()
        .map(str::trim)
        .filter(|value: &&str| !value.is_empty())
        .map(|value| value.to_string());

    ResolvedEmbeddingConfig {
        provider: provider.to_string(),
        model: model.to_string(),
        dimensions,
        api_key: routed_api_key.or(fallback_api_key),
    }
}

/// Factory: create the right memory backend from config
pub fn create_memory(
    config: &MemoryConfig,
    workspace_dir: &Path,
    api_key: Option<&str>,
) -> anyhow::Result<Box<dyn Memory>> {
    create_memory_with_storage_and_routes(config, &[], None, workspace_dir, api_key)
}

/// Factory: create memory with optional storage-provider override.
pub fn create_memory_with_storage(
    config: &MemoryConfig,
    storage_provider: Option<&StorageProviderConfig>,
    workspace_dir: &Path,
    api_key: Option<&str>,
) -> anyhow::Result<Box<dyn Memory>> {
    create_memory_with_storage_and_routes(config, &[], storage_provider, workspace_dir, api_key)
}

/// Factory: create memory with optional storage-provider override and embedding routes.
pub fn create_memory_with_storage_and_routes(
    config: &MemoryConfig,
    embedding_routes: &[EmbeddingRouteConfig],
    storage_provider: Option<&StorageProviderConfig>,
    workspace_dir: &Path,
    api_key: Option<&str>,
) -> anyhow::Result<Box<dyn Memory>> {
    let backend_name = effective_memory_backend_name(&config.backend, storage_provider);
    let backend_kind = classify_memory_backend(&backend_name);
    let resolved_embedding = resolve_embedding_config(config, embedding_routes, api_key);

    // Best-effort memory hygiene/retention pass (throttled by state file).
    if let Err(e) = hygiene::run_if_due(config, workspace_dir) {
        tracing::warn!("memory hygiene skipped: {e}");
    }

    // If snapshot_on_hygiene is enabled, export core memories during hygiene.
    if config.snapshot_enabled
        && config.snapshot_on_hygiene
        && matches!(
            backend_kind,
            MemoryBackendKind::Sqlite | MemoryBackendKind::Lucid
        )
    {
        if let Err(e) = snapshot::export_snapshot(workspace_dir) {
            tracing::warn!("memory snapshot skipped: {e}");
        }
    }

    // Auto-hydration: if brain.db is missing but MEMORY_SNAPSHOT.md exists,
    // restore the "soul" from the snapshot before creating the backend.
    if config.auto_hydrate
        && matches!(
            backend_kind,
            MemoryBackendKind::Sqlite | MemoryBackendKind::Lucid
        )
        && snapshot::should_hydrate(workspace_dir)
    {
        tracing::info!("🧬 Cold boot detected — hydrating from MEMORY_SNAPSHOT.md");
        match snapshot::hydrate_from_snapshot(workspace_dir) {
            Ok(count) => {
                if count > 0 {
                    tracing::info!("🧬 Hydrated {count} core memories from snapshot");
                }
            }
            Err(e) => {
                tracing::warn!("memory hydration failed: {e}");
            }
        }
    }

    fn build_sqlite_memory(
        config: &MemoryConfig,
        workspace_dir: &Path,
        resolved_embedding: &ResolvedEmbeddingConfig,
    ) -> anyhow::Result<SqliteMemory> {
        let embedder: Arc<dyn embeddings::EmbeddingProvider> =
            Arc::from(embeddings::create_embedding_provider(
                &resolved_embedding.provider,
                resolved_embedding.api_key.as_deref(),
                &resolved_embedding.model,
                resolved_embedding.dimensions,
            ));

        #[allow(clippy::cast_possible_truncation)]
        let mem = SqliteMemory::with_embedder(
            workspace_dir,
            embedder,
            config.vector_weight as f32,
            config.keyword_weight as f32,
            config.embedding_cache_size,
            config.sqlite_open_timeout_secs,
        )?;
        Ok(mem)
    }

    #[cfg(feature = "memory-postgres")]
    fn build_postgres_memory(
        storage_provider: Option<&StorageProviderConfig>,
    ) -> anyhow::Result<Box<dyn Memory>> {
        let storage_provider = storage_provider
            .context("memory backend 'postgres' requires [storage.provider.config] settings")?;
        let db_url = storage_provider
            .db_url
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .context(
                "memory backend 'postgres' requires [storage.provider.config].db_url (or dbURL)",
            )?;

        let memory = PostgresMemory::new(
            db_url,
            &storage_provider.schema,
            &storage_provider.table,
            storage_provider.connect_timeout_secs,
        )?;
        Ok(Box::new(memory))
    }

    #[cfg(not(feature = "memory-postgres"))]
    fn build_postgres_memory(
        _storage_provider: Option<&StorageProviderConfig>,
    ) -> anyhow::Result<Box<dyn Memory>> {
        anyhow::bail!(
            "memory backend 'postgres' requested but this build was compiled without `memory-postgres`; rebuild with `--features memory-postgres`"
        );
    }

    create_memory_with_builders(
        &backend_name,
        workspace_dir,
        || build_sqlite_memory(config, workspace_dir, &resolved_embedding),
        || build_postgres_memory(storage_provider),
        "",
    )
}

pub fn create_memory_for_migration(
    backend: &str,
    workspace_dir: &Path,
) -> anyhow::Result<Box<dyn Memory>> {
    if matches!(classify_memory_backend(backend), MemoryBackendKind::None) {
        anyhow::bail!(
            "memory backend 'none' disables persistence; choose sqlite, lucid, or markdown before migration"
        );
    }

    if matches!(
        classify_memory_backend(backend),
        MemoryBackendKind::Postgres
    ) {
        anyhow::bail!(
            "memory migration for backend 'postgres' is unsupported; migrate with sqlite or markdown first"
        );
    }

    create_memory_with_builders(
        backend,
        workspace_dir,
        || SqliteMemory::new(workspace_dir),
        || anyhow::bail!("postgres backend is not available in migration context"),
        " during migration",
    )
}

/// Factory: create an optional response cache from config.
pub fn create_response_cache(config: &MemoryConfig, workspace_dir: &Path) -> Option<ResponseCache> {
    if !config.response_cache_enabled {
        return None;
    }

    match ResponseCache::new(
        workspace_dir,
        config.response_cache_ttl_minutes,
        config.response_cache_max_entries,
    ) {
        Ok(cache) => {
            tracing::info!(
                "💾 Response cache enabled (TTL: {}min, max: {} entries)",
                config.response_cache_ttl_minutes,
                config.response_cache_max_entries
            );
            Some(cache)
        }
        Err(e) => {
            tracing::warn!("Response cache disabled due to error: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{EmbeddingRouteConfig, StorageProviderConfig};
    use tempfile::TempDir;

    #[test]
    fn factory_sqlite() {
        let tmp = TempDir::new().unwrap();
        let cfg = MemoryConfig {
            backend: "sqlite".into(),
            ..MemoryConfig::default()
        };
        let mem = create_memory(&cfg, tmp.path(), None).unwrap();
        assert_eq!(mem.name(), "sqlite");
    }

    #[test]
    fn assistant_autosave_key_detection_matches_legacy_patterns() {
        assert!(is_assistant_autosave_key("assistant_resp"));
        assert!(is_assistant_autosave_key("assistant_resp_1234"));
        assert!(is_assistant_autosave_key("ASSISTANT_RESP_abcd"));
        assert!(!is_assistant_autosave_key("assistant_response"));
        assert!(!is_assistant_autosave_key("user_msg_1234"));
    }

    #[test]
    fn should_inject_for_session_keeps_core_across_sessions() {
        let core = MemoryEntry {
            id: "1".into(),
            key: "pref".into(),
            content: "likes tea".into(),
            category: MemoryCategory::Core,
            timestamp: "now".into(),
            session_id: None,
            score: Some(1.0),
        };
        assert!(should_inject_for_session(&core, Some("sess-a")));
        assert!(should_inject_for_session(&core, None));
        let foreign_art = MemoryEntry {
            id: "art".into(),
            key: "dag_art:sess-b:node1".into(),
            content: "old xray findings".into(),
            category: MemoryCategory::Core,
            timestamp: "now".into(),
            session_id: Some("sess-b".into()),
            score: Some(1.0),
        };
        assert!(!should_inject_for_session(&foreign_art, Some("sess-a")));
        let own_art = MemoryEntry {
            id: "art2".into(),
            key: "dag_art:sess-a:node1".into(),
            content: "this graph".into(),
            category: MemoryCategory::Custom("dag".into()),
            timestamp: "now".into(),
            session_id: Some("sess-a".into()),
            score: Some(1.0),
        };
        assert!(should_inject_for_session(&own_art, Some("sess-a")));
    }

    #[test]
    fn should_inject_for_session_excludes_other_and_legacy_conversation() {
        let same = MemoryEntry {
            id: "2".into(),
            key: "user_msg_1".into(),
            content: "echo hello".into(),
            category: MemoryCategory::Conversation,
            timestamp: "now".into(),
            session_id: Some("sess-a".into()),
            score: Some(1.0),
        };
        let other = MemoryEntry {
            id: "3".into(),
            key: "user_msg_2".into(),
            content: "echo hello".into(),
            category: MemoryCategory::Conversation,
            timestamp: "now".into(),
            session_id: Some("sess-b".into()),
            score: Some(1.0),
        };
        let legacy = MemoryEntry {
            id: "4".into(),
            key: "user_msg_3".into(),
            content: "echo hello".into(),
            category: MemoryCategory::Conversation,
            timestamp: "now".into(),
            session_id: None,
            score: Some(1.0),
        };
        assert!(should_inject_for_session(&same, Some("sess-a")));
        assert!(!should_inject_for_session(&other, Some("sess-a")));
        assert!(!should_inject_for_session(&legacy, Some("sess-a")));
        assert!(should_inject_for_session(&legacy, None));
    }

    #[test]
    fn consolidate_shrinks_conversation_and_keeps_core() {
        let entries = vec![
            MemoryEntry {
                id: "c".into(),
                key: "gate_default".into(),
                content: "landlock stays optional".into(),
                category: MemoryCategory::Core,
                timestamp: "2026-08-01T00:00:00Z".into(),
                session_id: None,
                score: None,
            },
            MemoryEntry {
                id: "1".into(),
                key: "user_msg_1".into(),
                content: "noise one".into(),
                category: MemoryCategory::Conversation,
                timestamp: "2026-08-01T01:00:00Z".into(),
                session_id: Some("sess-a".into()),
                score: None,
            },
            MemoryEntry {
                id: "2".into(),
                key: "user_msg_2".into(),
                content: "noise two".into(),
                category: MemoryCategory::Conversation,
                timestamp: "2026-08-01T02:00:00Z".into(),
                session_id: Some("sess-a".into()),
                score: None,
            },
            MemoryEntry {
                id: "3".into(),
                key: "user_msg_3".into(),
                content: "noise three".into(),
                category: MemoryCategory::Conversation,
                timestamp: "2026-08-01T03:00:00Z".into(),
                session_id: Some("sess-a".into()),
                score: None,
            },
            MemoryEntry {
                id: "4".into(),
                key: "user_msg_4".into(),
                content: "keep recent".into(),
                category: MemoryCategory::Conversation,
                timestamp: "2026-08-01T04:00:00Z".into(),
                session_id: Some("sess-a".into()),
                score: None,
            },
        ];
        let after = consolidate_entries(&entries, CONSOLIDATION_CONVERSATION_KEEP);
        assert!(after.len() < entries.len());
        assert!(after.iter().any(|e| e.key == "gate_default"));
        assert!(should_inject_for_session(
            after.iter().find(|e| e.key == "gate_default").unwrap(),
            Some("sess-later")
        ));
        assert!(after.iter().any(|e| e.key == "consolidated_conversation"));
        assert!(after.iter().any(|e| e.content.contains("keep recent")));
    }

    #[test]
    fn factory_markdown() {
        let tmp = TempDir::new().unwrap();
        let cfg = MemoryConfig {
            backend: "markdown".into(),
            ..MemoryConfig::default()
        };
        let mem = create_memory(&cfg, tmp.path(), None).unwrap();
        assert_eq!(mem.name(), "markdown");
    }

    #[test]
    fn factory_lucid() {
        let tmp = TempDir::new().unwrap();
        let cfg = MemoryConfig {
            backend: "lucid".into(),
            ..MemoryConfig::default()
        };
        let mem = create_memory(&cfg, tmp.path(), None).unwrap();
        assert_eq!(mem.name(), "lucid");
    }

    #[test]
    fn factory_none_uses_noop_memory() {
        let tmp = TempDir::new().unwrap();
        let cfg = MemoryConfig {
            backend: "none".into(),
            ..MemoryConfig::default()
        };
        let mem = create_memory(&cfg, tmp.path(), None).unwrap();
        assert_eq!(mem.name(), "none");
    }

    #[test]
    fn factory_unknown_falls_back_to_markdown() {
        let tmp = TempDir::new().unwrap();
        let cfg = MemoryConfig {
            backend: "redis".into(),
            ..MemoryConfig::default()
        };
        let mem = create_memory(&cfg, tmp.path(), None).unwrap();
        assert_eq!(mem.name(), "markdown");
    }

    #[test]
    fn migration_factory_lucid() {
        let tmp = TempDir::new().unwrap();
        let mem = create_memory_for_migration("lucid", tmp.path()).unwrap();
        assert_eq!(mem.name(), "lucid");
    }

    #[test]
    fn migration_factory_none_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let error = create_memory_for_migration("none", tmp.path())
            .err()
            .expect("backend=none should be rejected for migration");
        assert!(error.to_string().contains("disables persistence"));
    }

    #[test]
    fn effective_backend_name_prefers_storage_override() {
        let storage = StorageProviderConfig {
            provider: "postgres".into(),
            ..StorageProviderConfig::default()
        };

        assert_eq!(
            effective_memory_backend_name("sqlite", Some(&storage)),
            "postgres"
        );
    }

    #[test]
    fn factory_postgres_without_db_url_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let cfg = MemoryConfig {
            backend: "postgres".into(),
            ..MemoryConfig::default()
        };

        let storage = StorageProviderConfig {
            provider: "postgres".into(),
            db_url: None,
            ..StorageProviderConfig::default()
        };

        let error = create_memory_with_storage(&cfg, Some(&storage), tmp.path(), None)
            .err()
            .expect("postgres without db_url should be rejected");
        if cfg!(feature = "memory-postgres") {
            assert!(error.to_string().contains("db_url"));
        } else {
            assert!(error.to_string().contains("memory-postgres"));
        }
    }

    #[test]
    fn resolve_embedding_config_uses_base_config_when_model_is_not_hint() {
        let cfg = MemoryConfig {
            embedding_provider: "openai".into(),
            embedding_model: "text-embedding-3-small".into(),
            embedding_dimensions: 1536,
            ..MemoryConfig::default()
        };

        let resolved = resolve_embedding_config(&cfg, &[], Some("base-key"));
        assert_eq!(
            resolved,
            ResolvedEmbeddingConfig {
                provider: "openai".into(),
                model: "text-embedding-3-small".into(),
                dimensions: 1536,
                api_key: Some("base-key".into()),
            }
        );
    }

    #[test]
    fn resolve_embedding_config_uses_matching_route_with_api_key_override() {
        let cfg = MemoryConfig {
            embedding_provider: "none".into(),
            embedding_model: "hint:semantic".into(),
            embedding_dimensions: 1536,
            ..MemoryConfig::default()
        };
        let routes = vec![EmbeddingRouteConfig {
            hint: "semantic".into(),
            provider: "custom:https://api.example.com/v1".into(),
            model: "custom-embed-v2".into(),
            dimensions: Some(1024),
            api_key: Some("route-key".into()),
        }];

        let resolved = resolve_embedding_config(&cfg, &routes, Some("base-key"));
        assert_eq!(
            resolved,
            ResolvedEmbeddingConfig {
                provider: "custom:https://api.example.com/v1".into(),
                model: "custom-embed-v2".into(),
                dimensions: 1024,
                api_key: Some("route-key".into()),
            }
        );
    }

    #[test]
    fn resolve_embedding_config_falls_back_when_hint_is_missing() {
        let cfg = MemoryConfig {
            embedding_provider: "openai".into(),
            embedding_model: "hint:semantic".into(),
            embedding_dimensions: 1536,
            ..MemoryConfig::default()
        };

        let resolved = resolve_embedding_config(&cfg, &[], Some("base-key"));
        assert_eq!(
            resolved,
            ResolvedEmbeddingConfig {
                provider: "openai".into(),
                model: "hint:semantic".into(),
                dimensions: 1536,
                api_key: Some("base-key".into()),
            }
        );
    }

    #[test]
    fn resolve_embedding_config_falls_back_when_route_is_invalid() {
        let cfg = MemoryConfig {
            embedding_provider: "openai".into(),
            embedding_model: "hint:semantic".into(),
            embedding_dimensions: 1536,
            ..MemoryConfig::default()
        };
        let routes = vec![EmbeddingRouteConfig {
            hint: "semantic".into(),
            provider: String::new(),
            model: "text-embedding-3-small".into(),
            dimensions: Some(0),
            api_key: None,
        }];

        let resolved = resolve_embedding_config(&cfg, &routes, Some("base-key"));
        assert_eq!(
            resolved,
            ResolvedEmbeddingConfig {
                provider: "openai".into(),
                model: "hint:semantic".into(),
                dimensions: 1536,
                api_key: Some("base-key".into()),
            }
        );
    }
}
