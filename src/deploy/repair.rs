use crate::deploy::diagnostic::{AIStructuredAnalysis, AlternativeAction};
use crate::providers::traits::Provider;
use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub struct RepairPlanner {
    llm: Box<dyn Provider>,
    dangerous_mode: bool,
    model: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RepairPlan {
    pub plan_id: String,
    pub root_cause: String,
    pub repair_steps: Vec<RepairStep>,
    pub estimated_duration_minutes: u32,
    pub risk_score: f64,
    pub success_probability: f64,
    pub rollback_plan: RollbackPlan,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RepairStep {
    pub step_id: String,
    pub description: String,
    pub command: String,
    pub expected_outcome: String,
    pub verification_method: String,
    pub risk_rating: f64,
    pub must_approve: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RollbackPlan {
    pub triggers: Vec<String>,
    pub rollback_steps: Vec<String>,
    pub estimated_rollback_duration_minutes: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StepVerification {
    pub success: bool,
    pub reasoning: String,
    pub alternative: Option<AlternativeAction>,
}

pub struct AdaptiveRepairExecutor {
    llm: Box<dyn Provider>,
    model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpertiseLevel {
    pub level: String,
}

impl std::fmt::Display for ExpertiseLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.level)
    }
}

impl RepairPlanner {
    pub fn new(llm: Box<dyn Provider>, dangerous_mode: bool, model: String) -> Self {
        Self {
            llm,
            dangerous_mode,
            model,
        }
    }

    pub async fn create_repair_plan(
        &self,
        hypothesis: String,
        evidence: String,
    ) -> Result<RepairPlan> {
        let prompt = format!(
            "Create repair plan for hypothesis: {}, evidence: {}",
            hypothesis, evidence
        );
        let response = self.llm.simple_chat(&prompt, &self.model, 0.7).await?;
        let plan: RepairPlan = serde_json::from_str(&response)?;
        Ok(plan)
    }

    fn build_repair_prompt(&self, hypothesis: &str, evidence: &str) -> Result<String> {
        Ok(format!(
            "Create repair for: {} with evidence: {}",
            hypothesis, evidence
        ))
    }

    fn ask_approval(&self, step: &RepairStep, risk_score: f64) -> bool {
        if self.dangerous_mode {
            return true;
        }
        risk_score < 50.0
    }
}

impl AdaptiveRepairExecutor {
    pub fn new(llm: Box<dyn Provider>, model: String) -> Self {
        Self { llm, model }
    }

    pub async fn execute_step(&self, step: &RepairStep) -> Result<()> {
        Ok(())
    }

    pub async fn verify_step(
        &mut self,
        step: &RepairStep,
        command_output: &str,
    ) -> Result<StepVerification> {
        let prompt = format!(
            "Verify step: {}, output: {}",
            step.description, command_output
        );
        let response = self.llm.simple_chat(&prompt, &self.model, 0.7).await?;
        let analysis: AIStructuredAnalysis = serde_json::from_str(&response)?;
        Ok(StepVerification {
            success: matches!(
                analysis.outcome,
                crate::deploy::diagnostic::StepOutcome::Success
            ),
            reasoning: analysis.reasoning,
            alternative: analysis.alternative_action,
        })
    }
}
