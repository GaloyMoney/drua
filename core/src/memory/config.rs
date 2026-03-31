/// Configuration for the memory service.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct MemoryConfig {
    #[serde(default = "default_decay_half_life_days")]
    pub decay_half_life_days: f64,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            decay_half_life_days: default_decay_half_life_days(),
        }
    }
}

fn default_decay_half_life_days() -> f64 {
    14.0
}
