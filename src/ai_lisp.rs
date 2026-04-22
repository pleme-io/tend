//! AI Flow Parser - JSON/YAML format
//!
//! ```json
//! {
//!   "flows": [
//!     {
//!       "name": "code-review",
//!       "steps": [
//!         {"id": "review", "model": "big-pickle", "prompt": "Review code", "retry": 3},
//!         {"id": "verify", "model": "big-pickle", "prompt": "Verify", "depends_on": ["review"]}
//!       ]
//!     }
//!   ]
//! }
//! ```

use crate::ai_flow::{AiFlowPlan, AiGroup};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct AiFlowFile {
    #[serde(default)]
    pub flows: Vec<AiFlowPlan>,
    #[serde(default)]
    pub groups: Vec<AiGroup>,
}

pub fn parse_json(text: &str) -> Result<AiFlowFile, String> {
    serde_json::from_str(text).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_json_simple() {
        let src = r#"{
            "flows": [{
                "name": "test",
                "steps": [
                    {"id": "a", "model": "big-pickle", "prompt": "hello"}
                ]
            }]
        }"#;
        let file = parse_json(src).unwrap();
        assert_eq!(file.flows.len(), 1);
        assert_eq!(file.flows[0].steps.len(), 1);
    }
}
