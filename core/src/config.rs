use serde::Deserialize;

use crate::agent::AgentConfig;
use crate::toolset::ToolSetsConfig;

#[derive(Clone, Debug, Default, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub agents: AgentConfig,
    #[serde(default)]
    pub toolsets: ToolSetsConfig,
}
