use crate::providers::traits::Provider;
use anyhow::Result;
use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};

pub struct PredictiveDiagnostics {
    llm: Box<dyn Provider>,
    model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerState {
    pub server_id: String,
    pub last_checked: DateTime<Utc>,
    pub cpu_usage_percent: f64,
    pub memory_usage_percent: f64,
    pub disk_usage_percent: f64,
    pub active_connections: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PredictionResult {
    pub prediction_id: String,
    pub server_id: String,
    pub predicted_issues: Vec<PredictedIssue>,
    pub confidence: f64,
    pub generated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PredictedIssue {
    pub issue_type: String,
    pub severity: String,
    pub description: String,
    pub likelihood_percent: f64,
    pub estimated_time_to_issue: TimeDelta,
    pub recommended_actions: Vec<String>,
}

pub enum PredictionHorizon {
    OneHour,
    OneDay,
    OneWeek,
}

impl PredictiveDiagnostics {
    pub fn new(llm: Box<dyn Provider>, model: String) -> Self {
        Self { llm, model }
    }

    pub async fn predict_issues(
        &self,
        state: &ServerState,
        horizon: PredictionHorizon,
    ) -> Result<PredictionResult> {
        let prompt = format!(
            "Predict issues for state: {} at horizon: {:?}",
            state.server_id, horizon
        );
        let response = self.llm.simple_chat(&prompt, &self.model, 0.7).await?;
        let result: PredictionResult = serde_json::from_str(&response)?;
        Ok(result)
    }
}
