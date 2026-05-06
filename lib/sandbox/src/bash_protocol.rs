//! Typed wire shape for the `bash` tool, shared between the sandbox
//! tool server (`images/sandbox/server`) and the in-tree HTTP client
//! (`InstanceClient::execute_bash`). Same JSON the server has always
//! accepted — the types just make the contract explicit and turn
//! caller-side typos into deserialization errors.
//!
//! These types intentionally don't pull in any of the K8s/HTTP deps
//! the rest of `lib/sandbox` carries; they are pure serde shapes.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Typed input for the `bash` tool. `deny_unknown_fields` turns
/// caller-side typos like `timeoutMs` into a deserialization error
/// rather than a silently-ignored field.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", default, deny_unknown_fields)]
pub struct BashCommandInput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub restart: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

impl BashCommandInput {
    pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;
    pub const MAX_TIMEOUT_MS: u64 = 10_800_000;

    pub fn validated_timeout_ms(&self) -> Result<u64, BashInputError> {
        match self.timeout_ms {
            None => Ok(Self::DEFAULT_TIMEOUT_MS),
            Some(0) => Err(BashInputError::TimeoutZero),
            Some(t) if t > Self::MAX_TIMEOUT_MS => Err(BashInputError::TimeoutTooLarge(t)),
            Some(t) => Ok(t),
        }
    }
}

/// Typed response for the `bash` tool. Same wire shape as the generic
/// `/execute` envelope — this is a renamed alias on the wire, not a
/// new format.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct BashCommandOutput {
    pub output: String,
    pub is_error: bool,
}

#[derive(Debug, Error)]
pub enum BashInputError {
    #[error("'timeout_ms' must be greater than 0")]
    TimeoutZero,
    #[error("'timeout_ms' must be at most {max} milliseconds (got {0})", max = BashCommandInput::MAX_TIMEOUT_MS)]
    TimeoutTooLarge(u64),
    #[error("'command' field is required (or set 'restart': true)")]
    MissingCommand,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_minimal_input() {
        let input = BashCommandInput {
            command: Some("ls".to_string()),
            ..Default::default()
        };
        let json = serde_json::to_value(&input).unwrap();
        assert_eq!(json, serde_json::json!({"command": "ls"}));
        let back: BashCommandInput = serde_json::from_value(json).unwrap();
        assert_eq!(back, input);
    }

    #[test]
    fn roundtrip_full_input() {
        let input = BashCommandInput {
            command: Some("make build".to_string()),
            restart: false,
            timeout_ms: Some(60_000),
        };
        let json = serde_json::to_value(&input).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"command": "make build", "timeout_ms": 60_000})
        );
        let back: BashCommandInput = serde_json::from_value(json).unwrap();
        assert_eq!(back, input);
    }

    #[test]
    fn roundtrip_restart_input() {
        let input = BashCommandInput {
            restart: true,
            ..Default::default()
        };
        let json = serde_json::to_value(&input).unwrap();
        assert_eq!(json, serde_json::json!({"restart": true}));
    }

    #[test]
    fn empty_object_deserializes_to_default() {
        let input: BashCommandInput = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(input, BashCommandInput::default());
    }

    #[test]
    fn unknown_field_is_rejected() {
        let err = serde_json::from_value::<BashCommandInput>(
            serde_json::json!({"command": "ls", "timeoutMs": 1000}),
        )
        .expect_err("camelCase typo should be rejected");
        assert!(
            err.to_string().contains("timeoutMs") || err.to_string().contains("unknown field"),
            "expected unknown-field error, got: {err}"
        );
    }

    #[test]
    fn timeout_validation() {
        let none = BashCommandInput::default();
        assert_eq!(
            none.validated_timeout_ms().unwrap(),
            BashCommandInput::DEFAULT_TIMEOUT_MS
        );

        let ok = BashCommandInput {
            timeout_ms: Some(2500),
            ..Default::default()
        };
        assert_eq!(ok.validated_timeout_ms().unwrap(), 2500);

        let zero = BashCommandInput {
            timeout_ms: Some(0),
            ..Default::default()
        };
        assert!(matches!(
            zero.validated_timeout_ms(),
            Err(BashInputError::TimeoutZero)
        ));

        let too_large = BashCommandInput {
            timeout_ms: Some(BashCommandInput::MAX_TIMEOUT_MS + 1),
            ..Default::default()
        };
        assert!(matches!(
            too_large.validated_timeout_ms(),
            Err(BashInputError::TimeoutTooLarge(_))
        ));
    }

    #[test]
    fn output_roundtrip() {
        for case in [
            BashCommandOutput {
                output: "hello\n".to_string(),
                is_error: false,
            },
            BashCommandOutput {
                output: "Exit code 1\noops".to_string(),
                is_error: true,
            },
        ] {
            let json = serde_json::to_value(&case).unwrap();
            let back: BashCommandOutput = serde_json::from_value(json).unwrap();
            assert_eq!(back, case);
        }
    }

    #[test]
    fn input_rejects_string_timeout_ms() {
        let err = serde_json::from_value::<BashCommandInput>(
            serde_json::json!({"command": "ls", "timeout_ms": "1000"}),
        )
        .expect_err("string timeout_ms should be rejected at deser");
        assert!(
            err.to_string().contains("integer") || err.to_string().contains("u64"),
            "expected type error, got: {err}"
        );
    }
}
