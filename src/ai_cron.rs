//! AI Flow Daemon - FSM-based long-running AI orchestration
//!
//! Implements reconciliation loop for AI flows like Kubernetes controllers.
//! States: Pending → Running → Verifying → Success | Failed

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepState {
    Pending,
    Running,
    Verifying,
    Success,
    Failed,
}

impl Default for StepState {
    fn default() -> Self {
        Self::Pending
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowState {
    pub flow_name: String,
    pub step_states: HashMap<String, StepState>,
    pub last_run: Option<DateTime<Utc>>,
    pub run_count: u64,
    pub consecutive_failures: u32,
}

impl FlowState {
    pub fn new(flow_name: impl Into<String>) -> Self {
        Self {
            flow_name: flow_name.into(),
            step_states: HashMap::new(),
            last_run: None,
            run_count: 0,
            consecutive_failures: 0,
        }
    }

    pub fn is_converged(&self) -> bool {
        self.step_states.values().all(|s| *s == StepState::Success)
    }

    pub fn is_failed(&self) -> bool {
        self.step_states.values().any(|s| *s == StepState::Failed)
    }

    pub fn reset(&mut self) {
        for state in self.step_states.values_mut() {
            *state = StepState::Pending;
        }
    }

    pub fn mark_running(&mut self, step_id: &str) {
        self.step_states
            .insert(step_id.to_string(), StepState::Running);
    }

    pub fn mark_success(&mut self, step_id: &str) {
        self.step_states
            .insert(step_id.to_string(), StepState::Success);
    }

    pub fn mark_failed(&mut self, step_id: &str) {
        self.step_states
            .insert(step_id.to_string(), StepState::Failed);
    }
}

pub struct AiFlowDaemon {
    states: Arc<RwLock<HashMap<String, FlowState>>>,
}

impl AiFlowDaemon {
    pub fn new() -> Self {
        Self {
            states: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn register(&self, flow_name: &str, step_ids: Vec<String>) {
        let mut states = self.states.write().await;
        let mut flow_state = FlowState::new(flow_name);
        for step_id in step_ids {
            flow_state.step_states.insert(step_id, StepState::Pending);
        }
        states.insert(flow_name.to_string(), flow_state);
    }

    pub async fn tick(&self, flow_name: &str) -> Option<FlowState> {
        let states = self.states.read().await;
        states.get(flow_name).cloned()
    }

    pub async fn update_state(&self, flow_name: &str, step_id: &str, state: StepState) {
        let mut states = self.states.write().await;
        if let Some(flow) = states.get_mut(flow_name) {
            flow.step_states.insert(step_id.to_string(), state);
            flow.last_run = Some(chrono::Utc::now());
            flow.run_count += 1;
        }
    }

    pub async fn is_converged(&self, flow_name: &str) -> bool {
        let states = self.states.read().await;
        states
            .get(flow_name)
            .map(|f| f.is_converged())
            .unwrap_or(false)
    }

    pub async fn get_pending_steps(&self, flow_name: &str) -> Vec<String> {
        let states = self.states.read().await;
        states
            .get(flow_name)
            .map(|f| {
                f.step_states
                    .iter()
                    .filter(|(_, s)| **s == StepState::Pending)
                    .map(|(id, _)| id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub async fn list_flows(&self) -> Vec<String> {
        let states = self.states.read().await;
        states.keys().cloned().collect()
    }

    pub async fn get_step_state(&self, flow_name: &str, step_id: &str) -> Option<StepState> {
        let states = self.states.read().await;
        states.get(flow_name)?.step_states.get(step_id).copied()
    }
}

impl Default for AiFlowDaemon {
    fn default() -> Self {
        Self::new()
    }
}
