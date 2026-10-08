use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One closed-set question posed to a decision model. `type` is the wire
/// discriminator (`noul` / `choice` / `score`); `instructions` and
/// `criteria` are `${{ … }}`-substituted by the workflow step before
/// dispatch, text fields only — the discriminator and question keys are
/// never templated.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Question {
    /// P(true) — a single yes/no probability.
    Noul {
        instructions: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        criteria: Option<NoulCriteria>,
    },
    /// Pick one of 2..=255 named options; the model returns the full
    /// probability distribution plus a confidence.
    Choice {
        instructions: String,
        criteria: BTreeMap<String, String>,
    },
    /// Position on 2..=10 ordered levels.
    Score {
        instructions: String,
        criteria: Vec<String>,
    },
}

/// Optional descriptions of the `true` / `false` outcomes for a `noul`
/// question — renamed on the wire to the bare booleans the model expects.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NoulCriteria {
    #[serde(rename = "true")]
    pub yes: String,
    #[serde(rename = "false")]
    pub no: String,
}

/// Outbound request body — same shape across OpenRouter, TypeSafe direct,
/// and a self-hosted Clef endpoint.
#[derive(Debug, Clone, Serialize)]
pub struct DecisionRequest {
    pub model: String,
    pub state: serde_json::Value,
    pub questions: BTreeMap<String, Question>,
}

/// One answer, shaped by its question's `type`.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Answer {
    Noul {
        noul: f64,
    },
    Choice {
        choice: String,
        confidence: f64,
        probabilities: BTreeMap<String, f64>,
    },
    Score {
        score: f64,
        confidence: f64,
        legend: BTreeMap<String, String>,
        probabilities: BTreeMap<String, f64>,
    },
}

impl Answer {
    /// Confidence on a 0.0..=1.0 scale, treating `noul` as a two-way
    /// choice: `choice` / `score` report `confidence` directly, `noul`
    /// takes `max(noul, 1 - noul)` so a probability near either extreme
    /// counts as confident. Used by the `decide` workflow step to gate
    /// downstream `condition:` expressions (§2.2 of the handoff).
    pub fn confidence_floor(&self) -> f64 {
        match self {
            Answer::Noul { noul } => noul.max(1.0 - noul),
            Answer::Choice { confidence, .. } | Answer::Score { confidence, .. } => *confidence,
        }
    }
}

/// Billed usage for one `decide` call. Deserialization is lenient —
/// `cost` is OpenRouter's field name, TypeSafe direct does not send one.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DecisionUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    /// Billed USD; OpenRouter sets `cost`, TypeSafe direct does not.
    #[serde(default, alias = "cost", skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

/// Inbound response body. Unknown fields are ignored (serde default),
/// `usage` is optional — the endpoint is alpha and both providers'
/// response shapes are still moving.
#[derive(Debug, Clone, Deserialize)]
pub struct DecisionResponse {
    #[serde(default)]
    pub id: Option<String>,
    pub model: String,
    pub answers: BTreeMap<String, Answer>,
    #[serde(default)]
    pub usage: DecisionUsage,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noul_question_round_trips_through_serde() {
        let q = Question::Noul {
            instructions: "Is this a duplicate?".into(),
            criteria: Some(NoulCriteria {
                yes: "same effort".into(),
                no: "distinct effort".into(),
            }),
        };
        let value = serde_json::to_value(&q).unwrap();
        assert_eq!(value["type"], "noul");
        assert_eq!(value["criteria"]["true"], "same effort");
        assert_eq!(value["criteria"]["false"], "distinct effort");
        let back: Question = serde_json::from_value(value).unwrap();
        assert!(matches!(back, Question::Noul { .. }));
    }

    #[test]
    fn choice_question_round_trips_through_serde() {
        let mut criteria = BTreeMap::new();
        criteria.insert("ec_recalc".to_string(), "Recalculate-balances work".into());
        criteria.insert("none".to_string(), "None of the above".into());
        let q = Question::Choice {
            instructions: "Which effort does this belong to?".into(),
            criteria,
        };
        let value = serde_json::to_value(&q).unwrap();
        assert_eq!(value["type"], "choice");
        let back: Question = serde_json::from_value(value).unwrap();
        let Question::Choice { criteria, .. } = back else {
            panic!("expected Choice");
        };
        assert_eq!(criteria.len(), 2);
    }

    #[test]
    fn score_question_round_trips_through_serde() {
        let q = Question::Score {
            instructions: "How urgent is this?".into(),
            criteria: vec!["can wait".into(), "this week".into(), "blocking".into()],
        };
        let value = serde_json::to_value(&q).unwrap();
        assert_eq!(value["type"], "score");
        let back: Question = serde_json::from_value(value).unwrap();
        let Question::Score { criteria, .. } = back else {
            panic!("expected Score");
        };
        assert_eq!(criteria.len(), 3);
    }

    /// Verbatim OpenRouter reference payload from the handoff's §2.1
    /// example response.
    #[test]
    fn deserializes_openrouter_reference_response() {
        let body = serde_json::json!({
            "model": "typesafe/jev-1.13",
            "answers": {
                "duplicate": { "type": "noul", "noul": 0.96 },
                "destination": {
                    "type": "choice", "choice": "ec_recalc", "confidence": 0.84,
                    "probabilities": { "ec_recalc": 0.84, "none": 0.16 }
                },
                "urgency": {
                    "type": "score", "score": 1.99, "confidence": 0.99,
                    "legend": { "0": "can wait", "1": "this week", "2": "blocking" },
                    "probabilities": { "0": 0, "1": 0.01, "2": 0.99 }
                }
            },
            "usage": { "input_tokens": 476, "output_tokens": 70, "cost_usd": 0.000019992 }
        });
        let resp: DecisionResponse = serde_json::from_value(body).unwrap();
        assert_eq!(resp.model, "typesafe/jev-1.13");
        assert_eq!(resp.usage.cost_usd, Some(0.000019992));

        let duplicate = &resp.answers["duplicate"];
        assert!((duplicate.confidence_floor() - 0.96).abs() < 1e-9);

        let destination = &resp.answers["destination"];
        assert!((destination.confidence_floor() - 0.84).abs() < 1e-9);

        let urgency = &resp.answers["urgency"];
        assert!((urgency.confidence_floor() - 0.99).abs() < 1e-9);
    }

    #[test]
    fn noul_confidence_floor_is_distance_from_the_uncertain_midpoint() {
        assert!((Answer::Noul { noul: 0.1 }.confidence_floor() - 0.9).abs() < 1e-9);
        assert!((Answer::Noul { noul: 0.9 }.confidence_floor() - 0.9).abs() < 1e-9);
        assert!((Answer::Noul { noul: 0.5 }.confidence_floor() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn deserializes_typesafe_direct_response_without_usage() {
        // TypeSafe direct doesn't send a `usage` block at all.
        let body = serde_json::json!({
            "model": "jev-1.13.0",
            "answers": {
                "ok": { "type": "noul", "noul": 0.5 }
            }
        });
        let resp: DecisionResponse = serde_json::from_value(body).unwrap();
        assert_eq!(resp.usage.input_tokens, 0);
        assert_eq!(resp.usage.cost_usd, None);
    }
}
