//! AI Flow types - plan/apply/destroy semantics like Terraform

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiFlowPlan {
    pub name: String,
    pub steps: Vec<AiStep>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiStep {
    pub id: String,
    pub task: AiTaskRef,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub retry: RetryPolicy,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RetryPolicy {
    pub count: u32,
    pub backoff_secs: u32,
    pub verify: Option<VerifyPolicy>,
}

impl RetryPolicy {
    pub fn new(count: u32) -> Self {
        Self { count, backoff_secs: 5, verify: None }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyPolicy {
    pub prompt: String,
    pub max_attempts: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AiTaskRef {
    Inline {
        model: String,
        prompt: String,
    },
    Reference(String),
}

impl AiTaskRef {
    pub fn model(&self) -> &str {
        match self {
            Self::Inline { model, .. } => model,
            Self::Reference(id) => id,
        }
    }

    pub fn kind(&self) -> &str {
        match self {
            Self::Inline { .. } => "inline",
            Self::Reference { .. } => "reference",
        }
    }
}

impl AiStep {
    pub fn model(&self) -> &str {
        self.task.model()
    }
}

impl AiFlowPlan {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            steps: vec![],
            env: HashMap::new(),
        }
    }

    pub fn add_step(&mut self, id: impl Into<String>, task: AiTaskRef) -> &mut AiStep {
        let id = id.into();
        self.steps.push(AiStep {
            id: id.clone(),
            task,
            depends_on: vec![],
            retry: RetryPolicy::default(),
        });
        self.steps.last_mut().unwrap()
    }

    pub fn with_deps(&mut self, id: &str, deps: Vec<String>) {
        if let Some(step) = self.steps.iter_mut().find(|s| s.id == id) {
            step.depends_on = deps;
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiGroup {
    pub name: String,
    pub flows: Vec<AiFlowPlan>,
    #[serde(default)]
    pub schedule: Option<String>,
    #[serde(default)]
    pub parallel: bool,
}

impl AiGroup {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            flows: vec![],
            schedule: None,
            parallel: false,
        }
    }
}