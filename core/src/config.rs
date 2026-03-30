use serde::Deserialize;

use crate::toolset::ToolSetsConfig;

#[derive(Clone, Debug, Default, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub toolsets: ToolSetsConfig,
}
