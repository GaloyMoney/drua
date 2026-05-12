use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub struct TunnelRuntimeConfig {
    #[serde(default)]
    pub self_pod_addr: Option<String>,
    #[serde(default = "default_tunnel_heartbeat_secs")]
    pub heartbeat_secs: u64,
    #[serde(default = "default_tunnel_expires_after_secs")]
    pub expires_after_secs: u64,
    #[serde(default = "default_tunnel_reaper_interval_secs")]
    pub reaper_interval_secs: u64,
    #[serde(default)]
    pub internal_secret: Option<String>,
}

impl Default for TunnelRuntimeConfig {
    fn default() -> Self {
        Self {
            self_pod_addr: None,
            heartbeat_secs: default_tunnel_heartbeat_secs(),
            expires_after_secs: default_tunnel_expires_after_secs(),
            reaper_interval_secs: default_tunnel_reaper_interval_secs(),
            internal_secret: None,
        }
    }
}

pub fn default_tunnel_heartbeat_secs() -> u64 {
    30
}

pub fn default_tunnel_expires_after_secs() -> u64 {
    90
}

pub fn default_tunnel_reaper_interval_secs() -> u64 {
    60
}

#[derive(Debug, thiserror::Error)]
pub enum TunnelConfigError {
    #[error("tunnel_runtime.heartbeat_secs must be > 0")]
    ZeroHeartbeat,
    #[error("tunnel_runtime.expires_after_secs ({expires_after_secs}) must be >= 2x heartbeat_secs ({heartbeat_secs})")]
    TtlTooLow {
        heartbeat_secs: u64,
        expires_after_secs: u64,
    },
    #[error("tunnel_runtime.reaper_interval_secs must be > 0")]
    ZeroReaperInterval,
}

impl TunnelRuntimeConfig {
    pub fn validate(&self) -> Result<(), TunnelConfigError> {
        if self.heartbeat_secs == 0 {
            return Err(TunnelConfigError::ZeroHeartbeat);
        }
        if self.expires_after_secs < self.heartbeat_secs * 2 {
            return Err(TunnelConfigError::TtlTooLow {
                heartbeat_secs: self.heartbeat_secs,
                expires_after_secs: self.expires_after_secs,
            });
        }
        if self.reaper_interval_secs == 0 {
            return Err(TunnelConfigError::ZeroReaperInterval);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rejects_zero_heartbeat() {
        let runtime = TunnelRuntimeConfig {
            heartbeat_secs: 0,
            ..Default::default()
        };
        assert!(matches!(
            runtime.validate(),
            Err(TunnelConfigError::ZeroHeartbeat)
        ));
    }

    #[test]
    fn validate_rejects_ttl_below_2x_heartbeat() {
        let runtime = TunnelRuntimeConfig {
            heartbeat_secs: 30,
            expires_after_secs: 45,
            ..Default::default()
        };
        assert!(matches!(
            runtime.validate(),
            Err(TunnelConfigError::TtlTooLow { .. })
        ));
    }

    #[test]
    fn validate_accepts_default() {
        TunnelRuntimeConfig::default().validate().unwrap();
    }

    #[test]
    fn validate_rejects_zero_reaper_interval() {
        let runtime = TunnelRuntimeConfig {
            reaper_interval_secs: 0,
            ..Default::default()
        };
        assert!(matches!(
            runtime.validate(),
            Err(TunnelConfigError::ZeroReaperInterval)
        ));
    }
}
