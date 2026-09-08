//! File-backed chat session store for Web Chat Phase 2 (VL-UI-003).
//! Web Chat 第二阶段基于文件的会话存储（VL-UI-003）。

use super::types::ChatMessageInput;
use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// After this many user turns, request a one-shot LLM title refresh.
pub const TITLE_REFINE_AFTER_USER_TURNS: usize = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatSessionSummary {
    pub id: String,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    pub message_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatSession {
    pub id: String,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub model_id: Option<String>,
    #[serde(default)]
    pub messages: Vec<ChatMessageInput>,
    /// When true, skip further auto title refresh (explicit create or already refined).
    #[serde(default)]
    pub title_refined: bool,
}

/// Result of appending messages — caller may run a cheap LLM title pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppendMessagesResult {
    pub user_turns: usize,
    pub needs_title_refine: bool,
}

#[derive(Debug, Clone)]
pub struct ChatSessionStore {
    root: PathBuf,
}

impl ChatSessionStore {
    pub fn new(workspace_dir: &Path) -> Self {
        Self {
            root: crate::agent::context_contract::chat_sessions_dir(workspace_dir),
        }
    }

    fn session_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.json"))
    }

    async fn ensure_root(&self) -> Result<()> {
        tokio::fs::create_dir_all(&self.root)
            .await
            .with_context(|| format!("create session dir {}", self.root.display()))
    }

    pub async fn list(&self) -> Result<Vec<ChatSessionSummary>> {
        self.ensure_root().await?;
        let mut entries = tokio::fs::read_dir(&self.root).await?;
        let mut summaries = Vec::new();

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Ok(session) = Self::read_session_file(&path).await {
                summaries.push(session.summary());
            }
        }

        summaries.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(summaries)
    }

    pub async fn get(&self, id: &str) -> Result<Option<ChatSession>> {
        if !is_valid_session_id(id) {
            return Ok(None);
        }
        let path = self.session_path(id);
        if !path.is_file() {
            return Ok(None);
        }
        Ok(Some(Self::read_session_file(&path).await?))
    }

    pub async fn create(
        &self,
        title: Option<String>,
        model_id: Option<String>,
    ) -> Result<ChatSession> {
        self.ensure_root().await?;
        let now = Utc::now().to_rfc3339();
        let explicit = title
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty());
        let title_refined = explicit.is_some();
        let session = ChatSession {
            id: Uuid::new_v4().to_string(),
            title: explicit.unwrap_or_else(|| "New chat".to_string()),
            created_at: now.clone(),
            updated_at: now,
            model_id: model_id.filter(|m| !m.trim().is_empty()),
            messages: Vec::new(),
            title_refined,
        };
        self.write_session(&session).await?;
        Ok(session)
    }

    pub async fn delete(&self, id: &str) -> Result<bool> {
        if !is_valid_session_id(id) {
            return Ok(false);
        }
        let path = self.session_path(id);
        if !path.is_file() {
            return Ok(false);
        }
        tokio::fs::remove_file(path).await?;
        Ok(true)
    }

    pub async fn append_messages(
        &self,
        id: &str,
        new_messages: &[ChatMessageInput],
        model_id: Option<&str>,
    ) -> Result<AppendMessagesResult> {
        let Some(mut session) = self.get(id).await? else {
            anyhow::bail!("session not found: {id}");
        };

        session.messages.extend(new_messages.iter().cloned());
        if let Some(model) = model_id.filter(|m| !m.trim().is_empty()) {
            session.model_id = Some(model.to_string());
        }

        let user_turns = user_turn_count(&session.messages);

        // Provisional title from the first user message until LLM refine.
        if !session.title_refined && session.title == "New chat" {
            if let Some(first_user) = session
                .messages
                .iter()
                .find(|m| m.role == "user" && !m.content.trim().is_empty())
            {
                session.title = truncate_title(&first_user.content);
            }
        }

        let needs_title_refine =
            !session.title_refined && user_turns >= TITLE_REFINE_AFTER_USER_TURNS;

        session.updated_at = Utc::now().to_rfc3339();
        self.write_session(&session).await?;
        Ok(AppendMessagesResult {
            user_turns,
            needs_title_refine,
        })
    }

    /// Persist an LLM-generated title and mark the session as refined.
    pub async fn set_refined_title(&self, id: &str, title: &str) -> Result<()> {
        let Some(mut session) = self.get(id).await? else {
            anyhow::bail!("session not found: {id}");
        };
        let cleaned = sanitize_generated_title(title);
        if !cleaned.is_empty() {
            session.title = cleaned;
        }
        session.title_refined = true;
        session.updated_at = Utc::now().to_rfc3339();
        self.write_session(&session).await
    }

    /// Mark refined without changing the title (LLM failed — keep provisional).
    pub async fn mark_title_refined(&self, id: &str) -> Result<()> {
        let Some(mut session) = self.get(id).await? else {
            anyhow::bail!("session not found: {id}");
        };
        session.title_refined = true;
        session.updated_at = Utc::now().to_rfc3339();
        self.write_session(&session).await
    }

    async fn write_session(&self, session: &ChatSession) -> Result<()> {
        self.ensure_root().await?;
        let path = self.session_path(&session.id);
        let data = serde_json::to_vec_pretty(session).context("serialize session")?;
        tokio::fs::write(path, data).await?;
        Ok(())
    }

    async fn read_session_file(path: &Path) -> Result<ChatSession> {
        let data = tokio::fs::read(path).await?;
        let session: ChatSession = serde_json::from_slice(&data).context("parse session json")?;
        Ok(session)
    }
}

impl ChatSession {
    fn summary(&self) -> ChatSessionSummary {
        ChatSessionSummary {
            id: self.id.clone(),
            title: self.title.clone(),
            created_at: self.created_at.clone(),
            updated_at: self.updated_at.clone(),
            model_id: self.model_id.clone(),
            message_count: self.messages.len(),
        }
    }
}

fn is_valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub fn user_turn_count(messages: &[ChatMessageInput]) -> usize {
    messages
        .iter()
        .filter(|m| m.role == "user" && !m.content.trim().is_empty())
        .count()
}

fn truncate_title(text: &str) -> String {
    let trimmed = text.trim().replace('\n', " ");
    const MAX: usize = 48;
    if trimmed.chars().count() <= MAX {
        trimmed
    } else {
        let end = trimmed
            .char_indices()
            .nth(MAX)
            .map(|(i, _)| i)
            .unwrap_or(trimmed.len());
        format!("{}…", &trimmed[..end])
    }
}

/// Strip quotes/whitespace and cap length for sidebar display.
pub fn sanitize_generated_title(raw: &str) -> String {
    let mut t = raw
        .trim()
        .trim_matches(|c| c == '"' || c == '\'' || c == '「' || c == '」');
    t = t.trim().trim_matches('`');
    // Models sometimes prefix "Title:" — drop a single label line.
    for prefix in ["Title:", "title:", "标题：", "标题:"] {
        if let Some(rest) = t.strip_prefix(prefix) {
            t = rest.trim();
            break;
        }
    }
    if let Some(first) = t.lines().next() {
        t = first.trim();
    }
    truncate_title(t)
}

/// Reject one-word filler titles common from small models (`yes`, `ok`, …).
pub fn is_acceptable_generated_title(title: &str) -> bool {
    let t = title.trim();
    if t.chars().count() < 4 {
        return false;
    }
    const WEAK: &[&str] = &[
        "yes", "no", "ok", "okay", "sure", "done", "thanks", "hello", "hi", "好的", "是的", "好",
    ];
    let lower = t.to_ascii_lowercase();
    !WEAK.contains(&lower.as_str())
}

/// Compact transcript of the first N user turns (plus replies) for a title prompt.
pub fn title_refine_transcript(messages: &[ChatMessageInput], max_user_turns: usize) -> String {
    let mut out = String::new();
    let mut users = 0usize;
    for m in messages {
        if m.role != "user" && m.role != "assistant" {
            continue;
        }
        if m.role == "user" {
            users = users.saturating_add(1);
        }
        if users > max_user_turns {
            break;
        }
        let body = truncate_body(m.content.trim(), 400);
        if body.is_empty() {
            continue;
        }
        out.push_str(m.role.as_str());
        out.push_str(": ");
        out.push_str(&body);
        out.push('\n');
        if users == max_user_turns && m.role == "assistant" {
            break;
        }
    }
    out
}

fn truncate_body(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let end = text
        .char_indices()
        .nth(max_chars)
        .map(|(i, _)| i)
        .unwrap_or(text.len());
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(content: &str) -> ChatMessageInput {
        ChatMessageInput {
            role: "user".into(),
            content: content.into(),
        }
    }

    fn assistant(content: &str) -> ChatMessageInput {
        ChatMessageInput {
            role: "assistant".into(),
            content: content.into(),
        }
    }

    #[test]
    fn valid_session_ids() {
        assert!(is_valid_session_id("abc-123_def"));
        assert!(!is_valid_session_id("../etc/passwd"));
        assert!(!is_valid_session_id(""));
    }

    #[test]
    fn truncate_title_limits_length() {
        let long = "a".repeat(80);
        let title = truncate_title(&long);
        assert!(title.chars().count() <= 49);
        assert!(title.ends_with('…'));
    }

    #[test]
    fn sanitize_generated_title_strips_wrapping() {
        assert_eq!(sanitize_generated_title("  \"Topic A\"  "), "Topic A");
        assert_eq!(sanitize_generated_title("Title: topic-b"), "topic-b");
    }

    #[test]
    fn is_acceptable_generated_title_rejects_filler() {
        assert!(!is_acceptable_generated_title("yes"));
        assert!(!is_acceptable_generated_title("ok"));
        assert!(is_acceptable_generated_title("Piubt Xray health"));
    }

    #[test]
    fn title_refine_transcript_stops_after_three_turns() {
        let msgs = vec![
            user("u1"),
            assistant("a1"),
            user("u2"),
            assistant("a2"),
            user("u3"),
            assistant("a3"),
            user("u4"),
            assistant("a4"),
        ];
        let t = title_refine_transcript(&msgs, 3);
        assert!(t.contains("u3"));
        assert!(t.contains("a3"));
        assert!(!t.contains("u4"));
    }

    #[tokio::test]
    async fn create_list_get_delete_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = ChatSessionStore::new(dir.path());

        let created = store
            .create(None, Some("provider/model".into()))
            .await
            .unwrap();
        assert_eq!(created.title, "New chat");
        assert!(!created.title_refined);

        let listed = store.list().await.unwrap();
        assert_eq!(listed.len(), 1);

        let fetched = store.get(&created.id).await.unwrap().unwrap();
        assert_eq!(fetched.id, created.id);

        let result = store
            .append_messages(&created.id, &[user("hello")], Some("provider/model"))
            .await
            .unwrap();
        assert!(result.needs_title_refine);

        let updated = store.get(&created.id).await.unwrap().unwrap();
        assert_eq!(updated.messages.len(), 1);
        assert_eq!(updated.title, "hello");

        assert!(store.delete(&created.id).await.unwrap());
        assert!(store.get(&created.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn signals_refine_after_first_user_turn() {
        let dir = tempfile::tempdir().unwrap();
        let store = ChatSessionStore::new(dir.path());
        let created = store.create(None, None).await.unwrap();

        let r1 = store
            .append_messages(&created.id, &[user("turn-1"), assistant("ok")], None)
            .await
            .unwrap();
        assert!(r1.needs_title_refine);
        assert_eq!(r1.user_turns, 1);

        store
            .set_refined_title(&created.id, "refined-title")
            .await
            .unwrap();
        let after = store.get(&created.id).await.unwrap().unwrap();
        assert!(after.title_refined);
        assert_eq!(after.title, "refined-title");

        let r4 = store
            .append_messages(&created.id, &[user("turn-4"), assistant("ok")], None)
            .await
            .unwrap();
        assert!(!r4.needs_title_refine);
    }

    #[tokio::test]
    async fn explicit_create_title_is_not_flagged_for_refine() {
        let dir = tempfile::tempdir().unwrap();
        let store = ChatSessionStore::new(dir.path());
        let created = store
            .create(Some("pinned-title".into()), None)
            .await
            .unwrap();
        assert!(created.title_refined);

        for i in 0..3 {
            let r = store
                .append_messages(
                    &created.id,
                    &[user(&format!("turn-{i}")), assistant("ok")],
                    None,
                )
                .await
                .unwrap();
            assert!(!r.needs_title_refine);
        }
        let after = store.get(&created.id).await.unwrap().unwrap();
        assert_eq!(after.title, "pinned-title");
    }
}
