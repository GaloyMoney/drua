//! `decide` — provider-agnostic decision-model tool (TypeSafe Jev and its
//! `/v1/systemone`-compatible clones, routed through OpenRouter by
//! default). Takes text or JSON state plus closed-set questions and
//! returns calibrated probabilities in one forward pass: no text
//! generation, no tool calls. See
//! `handoff-decide-tool-and-step-2026-10-05.md` for the full design.

use std::collections::BTreeMap;
use std::sync::Arc;

use decision_client::{Answer, DecisionClient, DecisionRequest, Question};
use rmcp::model::{CallToolResult, Content, JsonObject};
use serde::Deserialize;

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;
use super::{parse_params, schema_for, OutputSchema};
use crate::audit::Audit;
use crate::auth::AuthSubject;

#[derive(Deserialize, schemars::JsonSchema)]
struct DecideParams {
    /// What the model reads: free-form text or a JSON object/array.
    /// Reference its fields from question `instructions` in backticks
    /// (`` `doc.title` ``) — the model is shown the whole thing.
    #[schemars(schema_with = "crate::toolset::any_json_schema")]
    state: serde_json::Value,
    /// 1..=`max_questions` closed-set questions, keyed by an identifier
    /// matching `^[A-Za-z_][A-Za-z0-9_]*$` (so it's a valid
    /// `steps.<step>.outputs.answers.<key>` CEL reference).
    questions: BTreeMap<String, Question>,
    /// Overrides the configured default model id for this call.
    #[serde(default)]
    model: Option<String>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct DecideOutput {
    model: String,
    answers: BTreeMap<String, Answer>,
    usage: decision_client::DecisionUsage,
}

pub struct DecideTool {
    client: Arc<DecisionClient>,
    default_model: String,
    input: serde_json::Value,
    output: OutputSchema<DecideOutput>,
}

impl DecideTool {
    pub fn new(client: Arc<DecisionClient>, default_model: String) -> Self {
        Self {
            client,
            default_model,
            input: schema_for::<DecideParams>(),
            output: OutputSchema::new(),
        }
    }
}

#[async_trait::async_trait]
impl TopLevelTool for DecideTool {
    fn name(&self) -> &str {
        "decide"
    }

    fn description(&self) -> &str {
        "Ask a decision model closed-set questions about a piece of state and get calibrated \
         probabilities back in one call — no text generation, no reasoning, ~0.2 s, ~$0.00002 \
         per call. Question types: `noul` (P(true)), `choice` (pick one of up to 255 named \
         options, returns the full distribution and a confidence), `score` (position on 2-10 \
         ordered levels). Use it for triage, routing, labelling, duplicate checks and \
         self-verification of a draft; do the arithmetic, date maths and counting yourself \
         first — the model reads numbers and dates as text. Options are judged by their \
         descriptions, so describe them; put the thing to judge in `state` and reference its \
         fields in backticks (`doc.title`)."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &self.input
    }

    fn inner_output_schema(&self) -> Option<&serde_json::Value> {
        Some(self.output.schema())
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        !matches!(subject, AuthSubject::Anonymous)
    }

    async fn call(
        &self,
        _subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        Audit::record_action("decide");
        let params: DecideParams = parse_params(arguments)?;
        let question_count = params.questions.len();

        let request = DecisionRequest {
            model: params.model.unwrap_or_else(|| self.default_model.clone()),
            state: params.state,
            questions: params.questions,
        };

        decision_client::validate(&request, self.client.limits())
            .map_err(|e| ToolSetsError::InvalidArgument(e.to_string()))?;

        let response = match self.client.decide(&request).await {
            Ok(response) => response,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "decide failed: {e}"
                ))]))
            }
        };

        Audit::merge_metadata(serde_json::json!({
            "model": response.model,
            "question_count": question_count,
            "input_tokens": response.usage.input_tokens,
            "cost_usd": response.usage.cost_usd,
            "decision_id": response.id,
        }));

        let out = DecideOutput {
            model: response.model,
            answers: response.answers,
            usage: response.usage,
        };
        let structured = serde_json::to_value(&out).expect("DecideOutput serialization");
        let text = serde_json::to_string_pretty(&structured).unwrap_or_else(|_| "{}".to_string());
        Ok(self.output.success(text, &out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::primitives::{AgentId, ProjectId, UserId, WorkflowDefinitionId, WorkflowRunId};
    use crate::toolset::ToolSets;
    use std::time::Duration;

    fn unreachable_client() -> Arc<DecisionClient> {
        // Port 9 ("discard") never accepts connections — any real HTTP
        // attempt surfaces as a transport error distinct from a validation
        // rejection, so `decide_rejects_bad_questions_before_any_http` can
        // tell the two apart.
        Arc::new(DecisionClient::new(
            "unused-key",
            "http://127.0.0.1:9/decisions",
            Duration::from_millis(200),
            decision_client::DecisionLimits::default(),
        ))
    }

    fn tool() -> DecideTool {
        DecideTool::new(unreachable_client(), "typesafe/jev-1.13".to_string())
    }

    #[test]
    fn decide_satisfies_the_tool_step_contract() {
        let toolsets = ToolSets::empty_for_test();
        toolsets.register_top_level(tool());
        assert!(toolsets.find_for_workflow("decide").is_ok());
    }

    #[test]
    fn decide_hidden_from_anonymous_visible_to_everyone_else() {
        let tool = tool();
        let project_id = ProjectId::new();
        let subjects = [
            AuthSubject::User(UserId::new()),
            AuthSubject::ExportedAgent(UserId::new(), crate::primitives::McpCredsId::new(), vec![]),
            AuthSubject::Agent(project_id, AgentId::new(), vec![]),
            AuthSubject::workflow_script(
                project_id,
                WorkflowDefinitionId::new(),
                WorkflowRunId::new(),
            ),
            AuthSubject::workflow_executor(
                project_id,
                WorkflowDefinitionId::new(),
                WorkflowRunId::new(),
            ),
        ];
        for subject in subjects {
            assert!(tool.is_visible(&subject), "{subject:?} should be visible");
        }
        assert!(!tool.is_visible(&AuthSubject::Anonymous));
    }

    #[tokio::test]
    async fn decide_rejects_bad_questions_before_any_http() {
        let tool = tool();
        let mut criteria = BTreeMap::new();
        criteria.insert("only".to_string(), "the only option".to_string());
        let arguments = serde_json::json!({
            "state": "some state",
            "questions": {
                "destination": {
                    "type": "choice",
                    "instructions": "pick one",
                    "criteria": criteria,
                }
            }
        })
        .as_object()
        .unwrap()
        .clone();
        let err = tool
            .call(&AuthSubject::User(UserId::new()), Some(arguments))
            .await
            .expect_err("a choice with one option must be rejected before any HTTP call");
        assert!(matches!(err, ToolSetsError::InvalidArgument(_)));
    }

    #[test]
    fn decide_output_schema_is_an_object_with_result_wrapper() {
        let tool = tool();
        let schema = tool
            .output_schema()
            .expect("decide declares an output schema");
        let result = schema
            .get("properties")
            .and_then(|p| p.get("result"))
            .expect("wrapped output schema has properties.result");
        assert!(
            result
                .get("properties")
                .and_then(|p| p.get("answers"))
                .is_some(),
            "properties.result.properties.answers must be present, got {result}"
        );
    }
}
