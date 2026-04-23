//! AI Flow Executor - plan/apply/destroy semantics like Terraform
//! 
//! Uses reqwest for HTTP calls to opencode-zen API directly.

use anyhow::Result;
use crate::ai_flow::{AiFlowPlan, AiStep, AiTaskRef, RetryPolicy};
use crate::ai_models::ModelRegistry;
use crate::ai_planner::PlannedFlow;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutionPhase {
    Plan,
    Apply,
    Destroy,
}

impl ExecutionPhase {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Plan => "plan",
            Self::Apply => "apply",
            Self::Destroy => "destroy",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionReport {
    pub phase: ExecutionPhase,
    pub plan_output: Vec<String>,
    pub outputs: HashMap<String, String>,
    pub errors: Vec<String>,
}

impl ExecutionReport {
    pub fn new(phase: ExecutionPhase) -> Self {
        Self {
            phase,
            plan_output: vec![],
            outputs: HashMap::new(),
            errors: vec![],
        }
    }

    pub fn is_success(&self) -> bool {
        self.errors.is_empty()
    }

    pub fn summary(&self) -> String {
        if self.is_success() {
            format!("{}: {} steps executed", self.phase.as_str(), self.outputs.len())
        } else {
            format!("{}: {} errors", self.phase.as_str(), self.errors.len())
        }
    }
}

pub struct AiExecutor {
    api_key: String,
    client: reqwest::Client,
    results: Arc<RwLock<HashMap<String, String>>>,
    dry_run: bool,
    fallback_models: ModelRegistry,
}

impl AiExecutor {
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            client: reqwest::Client::new(),
            results: Arc::new(RwLock::new(HashMap::new())),
            dry_run: false,
            fallback_models: ModelRegistry::new(),
        }
    }

    pub fn with_dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = dry_run;
        self
    }

    pub fn models(&self) -> &ModelRegistry {
        &self.fallback_models
    }

    pub async fn run(&self, planned: &PlannedFlow) -> Result<ExecutionReport> {
        let mut report = ExecutionReport::new(ExecutionPhase::Apply);

        if self.dry_run {
            for &idx in &planned.execution_order {
                let step = &planned.plan.steps[idx];
                report.plan_output.push(format!(
                    "Step {}: {} would execute with model {}",
                    step.id, step.task.kind(), step.task.model()
                ));
            }
            return Ok(report);
        }

        for &idx in &planned.execution_order {
            let step = &planned.plan.steps[idx];
            match self.execute_step(step).await {
                Ok(result) => {
                    report.outputs.insert(step.id.clone(), result);
                }
                Err(e) => {
                    report.errors.push(format!("Step {} failed: {}", step.id, e));
                    break;
                }
            }
        }

        Ok(report)
    }

    pub async fn plan(&self, planned: &PlannedFlow) -> Result<ExecutionReport> {
        let mut report = ExecutionReport::new(ExecutionPhase::Plan);

        for &idx in &planned.execution_order {
            let step = &planned.plan.steps[idx];
            report.plan_output.push(format!(
                "+ {} [{}] model={} timeout={}s retries={}",
                step.id,
                step.task.kind(),
                step.task.model(),
                step.retry.count,
                step.retry.backoff_secs
            ));
            for dep in &step.depends_on {
                report.plan_output.push(format!("  depends on: {}", dep));
            }
        }

        Ok(report)
    }

    pub async fn destroy(&self, planned: &PlannedFlow) -> Result<ExecutionReport> {
        let mut report = ExecutionReport::new(ExecutionPhase::Destroy);

        self.clear_results().await;
        report.plan_output.push(format!(
            "Destroyed {} step results",
            planned.plan.steps.len()
        ));

        Ok(report)
    }

    pub async fn execute_step(&self, step: &AiStep) -> Result<String> {
        let mut current_model = step.task.model().to_string();
        let mut last_error = None;

        for attempt in 0..step.retry.count.max(1) {
            match self.execute_single(&current_model, step).await {
                Ok(result) => return Ok(result),
                Err(e) => {
                    last_error = Some(e);
                    if let Some(next) = self.fallback_models.fallback_for(&current_model) {
                        eprintln!("Model {} failed, trying: {}", current_model, next.id);
                        current_model = next.id.clone();
                    } else {
                        break;
                    }
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("all models failed")))
    }

    async fn execute_single(&self, model: &str, step: &AiStep) -> Result<String> {
        let prompt = match &step.task {
            AiTaskRef::Inline { prompt, .. } => prompt.clone(),
            AiTaskRef::Reference(_) => return Err(anyhow::anyhow!("Reference not supported")),
        };

        #[derive(Serialize)]
        struct ChatMessage {
            role: String,
            content: String,
        }

        #[derive(Serialize)]
        struct ChatRequest {
            model: String,
            messages: Vec<ChatMessage>,
            temperature: f64,
            max_tokens: i32,
        }

        let request = ChatRequest {
            model: model.to_string(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: prompt,
            }],
            temperature: 0.7,
            max_tokens: 4096,
        };

        let resp = self.client
            .post("https://opencode.ai/zen/v1/chat/completions")
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&request)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(anyhow::anyhow!("API error: {}", resp.status()));
        }

        #[derive(Deserialize)]
        struct ChatResponse {
            choices: Vec<Choice>,
        }

        #[derive(Deserialize)]
        struct Choice {
            message: Message,
        }

        #[derive(Deserialize)]
        struct Message {
            content: String,
        }

        let response: ChatResponse = resp.json().await?;
        
        Ok(response.choices.into_iter().next()
            .map(|c| c.message.content)
            .unwrap_or_default())
    }

    pub async fn get_result(&self, step_id: &str) -> Option<String> {
        let results = self.results.read().await;
        results.get(step_id).cloned()
    }

    pub async fn clear_results(&self) {
        let mut results = self.results.write().await;
        results.clear();
    }
}