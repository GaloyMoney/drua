/// Configuration for the memory service.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryConfig {
    #[serde(default = "default_db_path")]
    pub db_path: String,
    #[serde(default = "default_decay_half_life_days")]
    pub decay_half_life_days: f64,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            db_path: default_db_path(),
            decay_half_life_days: default_decay_half_life_days(),
        }
    }
}

fn default_db_path() -> String {
    "/data/memory/memory.db".to_string()
}

fn default_decay_half_life_days() -> f64 {
    14.0
}
