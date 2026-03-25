/// Configuration for embedding style-agent in another server.
#[derive(Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StyleAgentConfig {
    #[serde(default = "default_style_agent_db_path")]
    pub db_path: String,
}

impl Default for StyleAgentConfig {
    fn default() -> Self {
        Self {
            db_path: default_style_agent_db_path(),
        }
    }
}

fn default_style_agent_db_path() -> String {
    "/data/style-agent/style-agent.db".to_string()
}
