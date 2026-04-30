use serde::{Deserialize, Serialize};

/// The type of secret, determining how the harness injects it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretType {
    /// Injected as an environment variable.
    EnvVar,
    /// Written to `/run/secrets/<name>`.
    File,
}

impl SecretType {
    pub fn as_str(&self) -> &'static str {
        match self {
            SecretType::EnvVar => "env_var",
            SecretType::File => "file",
        }
    }
}

impl std::fmt::Display for SecretType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for SecretType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "env_var" => Ok(SecretType::EnvVar),
            "file" => Ok(SecretType::File),
            other => Err(format!("invalid secret type: {other}")),
        }
    }
}
