use serde::Deserialize;

#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConcourseConfig {
    /// Enable Concourse CI integration.
    #[serde(default)]
    pub enabled: bool,

    /// Base URL of the Concourse instance (e.g. `https://ci.galoy.io`).
    #[serde(default)]
    pub url: String,

    /// Default Concourse team name.
    #[serde(default)]
    pub team: String,

    /// Credentials — populated from environment variables, not the config file.
    #[serde(skip)]
    pub username: String,
    #[serde(skip)]
    pub password: String,
}
