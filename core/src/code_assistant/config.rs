/// Configuration for embedding code assistant in another server.
#[derive(Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeAssistantConfig {
    #[serde(default = "default_code_assistant_db_path")]
    pub db_path: String,
}

impl Default for CodeAssistantConfig {
    fn default() -> Self {
        Self {
            db_path: default_code_assistant_db_path(),
        }
    }
}

fn default_code_assistant_db_path() -> String {
    "/data/code-assistant/code-assistant.db".to_string()
}
