//! AI-assisted deployment diagnostics

use crate::config::Config;
use crate::deploy::strategy::{DiagnosticStrategy, InvestigationStep};
use crate::providers::traits::Provider;
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub struct DiagnosticOrchestrator {
    llm: Box<dyn Provider>,
    model: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DiagnosticContext {
    pub server_info: ServerInfo,
    pub user_description: String,
    pub category_hint: Option<String>,
    pub conversation_history: Vec<Message>,
    pub current_hypothesis: Option<DiagnosticHypothesis>,
    pub evidence_collected: Vec<Evidence>,
    pub refuted_hypotheses: Vec<DiagnosticHypothesis>,
    pub started_at: DateTime<Utc>,
    pub investigation_steps_executed: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
    pub id: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub mode: DeploymentModeInfo,
    pub environment: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DeploymentModeInfo {
    Direct,
    Docker,
    Systemd,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticHypothesis {
    pub hypothesis_id: String,
    pub description: String,
    pub confidence: f64,
    pub reasoning: String,
    pub expected_evidence: Vec<String>,
    pub contradictory_evidence: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evidence {
    pub evidence_id: String,
    pub category: String,
    pub source: String,
    pub description: String,
    pub data: serde_json::Value,
    pub timestamp: DateTime<Utc>,
    pub step_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum NextAction {
    Investigate {
        updated_hypothesis: DiagnosticHypothesis,
        reasoning: String,
        next_investigation_step: InvestigationStep,
    },
    ConfirmRootCause {
        hypothesis: DiagnosticHypothesis,
        evidence: Vec<Evidence>,
        reasoning: String,
        confidence: f64,
    },
    RequestMoreInfo {
        reasoning: String,
        questions: Vec<String>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AIStructuredAnalysis {
    pub outcome: StepOutcome,
    pub reasoning: String,
    pub confidence: f64,
    pub should_continue: bool,
    pub alternative_action: Option<AlternativeAction>,
    pub next_verification: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum StepOutcome {
    Success,
    PartialSuccess {
        continuation: String,
    },
    Failure {
        alternative_action: Option<AlternativeAction>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AlternativeAction {
    pub description: String,
    pub commands: Vec<String>,
    pub rationale: String,
    pub risk_assessment: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DiagnosticData {
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct NetworkDiagnostics;

#[derive(Debug, Serialize, Deserialize)]
pub struct SystemDiagnostics;

#[derive(Debug, Serialize, Deserialize)]
pub struct ServiceDiagnostics;

#[derive(Debug, Serialize, Deserialize)]
pub struct ConfigDiagnostics;

#[derive(Debug, Serialize, Deserialize)]
pub struct LogAnalysis;

#[derive(Debug, Serialize, Deserialize)]
pub struct SecurityDiagnostics;

impl DiagnosticOrchestrator {
    pub fn new(
        config: &Config,
        server: ServerInfo,
        problem: String,
        category_hint: Option<String>,
    ) -> Result<Self> {
        let llm = config.create_provider()?;
        let model = config
            .default_model
            .clone()
            .unwrap_or_else(|| "anthropic/claude-sonnet-4-20250514".to_string());

        Ok(Self { llm, model })
    }

    pub async fn plan_diagnosis(&mut self) -> Result<DiagnosticStrategy> {
        let prompt = "Create diagnostic strategy";
        let response = self.llm.simple_chat(prompt, &self.model, 0.7).await?;
        let strategy: DiagnosticStrategy = serde_json::from_str(&response)?;
        Ok(strategy)
    }

    pub async fn execute_step(&mut self, _step: &InvestigationStep) -> Result<Evidence> {
        Ok(Evidence {
            evidence_id: "stub".to_string(),
            category: "general".to_string(),
            source: "stub".to_string(),
            description: "Stub evidence".to_string(),
            data: serde_json::Value::Null,
            timestamp: Utc::now(),
            step_id: None,
        })
    }

    pub async fn update_strategy(&mut self, _evidence: &Evidence) -> Result<NextAction> {
        Ok(NextAction::RequestMoreInfo {
            reasoning: "Stub implementation".to_string(),
            questions: vec![],
        })
    }
}
