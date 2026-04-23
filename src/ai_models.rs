//! Free Tier Model Registry - fallback chain for OpenCode Zen
//!
//! Priority-ordered list of free models. If one fails or is unavailable,
//! the executor automatically tries the next one.

#[derive(Debug, Clone)]
pub struct ModelEntry {
    pub id: String,
    pub name: String,
    pub priority: u8,
    pub free: bool,
}

pub struct ModelRegistry {
    models: Vec<ModelEntry>,
}

impl Default for ModelRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelRegistry {
    pub fn new() -> Self {
        Self {
            models: vec![
                ModelEntry {
                    id: "big-pickle".to_string(),
                    name: "Big Pickle".to_string(),
                    priority: 1,
                    free: true,
                },
                ModelEntry {
                    id: "nemotron-3-super-free".to_string(),
                    name: "Nemotron 3 Super".to_string(),
                    priority: 2,
                    free: true,
                },
                ModelEntry {
                    id: "glm-4.7-free".to_string(),
                    name: "GLM 4.7".to_string(),
                    priority: 3,
                    free: true,
                },
                ModelEntry {
                    id: "minimax-m2.5-free".to_string(),
                    name: "MiniMax M2.5".to_string(),
                    priority: 4,
                    free: true,
                },
                ModelEntry {
                    id: "kimi-k2.5-free".to_string(),
                    name: "Kimi K2.5".to_string(),
                    priority: 5,
                    free: true,
                },
                ModelEntry {
                    id: "ling-2.6-flash-free".to_string(),
                    name: "Ling 2.6 Flash".to_string(),
                    priority: 6,
                    free: true,
                },
                ModelEntry {
                    id: "trinity-large-preview-free".to_string(),
                    name: "Trinity Large".to_string(),
                    priority: 7,
                    free: true,
                },
            ],
        }
    }

    pub fn free_models(&self) -> Vec<&ModelEntry> {
        self.models.iter().filter(|m| m.free).collect()
    }

    pub fn primary(&self) -> &ModelEntry {
        self.models.first().expect("at least one model")
    }

    pub fn fallback_for(&self, model_id: &str) -> Option<&ModelEntry> {
        let idx = self.models.iter().position(|m| m.id == model_id)?;
        self.models.get(idx + 1).filter(|m| m.free)
    }

    pub fn all(&self) -> &[ModelEntry] {
        &self.models
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_primary_is_big_pickle() {
        let registry = ModelRegistry::new();
        assert_eq!(registry.primary().id, "big-pickle");
    }

    #[test]
    fn test_fallback_chain() {
        let registry = ModelRegistry::new();
        
        let first = registry.primary();
        assert_eq!(first.id, "big-pickle");
        
        let second = registry.fallback_for("big-pickle").unwrap();
        assert_eq!(second.id, "nemotron-3-super-free");
        
        let third = registry.fallback_for("nemotron-3-super-free").unwrap();
        assert_eq!(third.id, "glm-4.7-free");
    }

    #[test]
    fn test_free_models_count() {
        let registry = ModelRegistry::new();
        assert_eq!(registry.free_models().len(), 7);
    }
}
