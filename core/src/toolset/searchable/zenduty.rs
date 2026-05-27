use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject, Tool};
use serde::Deserialize;
use zenduty_client::{IncidentStatus, ListIncidentsParams, ZendutyClient};

use crate::auth::AuthSubject;

use super::super::{SearchableToolSet, ToolSetEntry, ToolSetsError};

fn parse_params<T: serde::de::DeserializeOwned>(
    arguments: Option<JsonObject>,
) -> Result<T, ToolSetsError> {
    let value = serde_json::Value::Object(arguments.unwrap_or_default());
    serde_json::from_value(value).map_err(|e| ToolSetsError::InvalidArgument(e.to_string()))
}

fn schema_for<T: schemars::JsonSchema>() -> serde_json::Value {
    let settings = schemars::gen::SchemaSettings::draft07().with(|s| {
        s.inline_subschemas = true;
        s.meta_schema = None;
    });
    let generator = settings.into_generator();
    let schema = generator.into_root_schema_for::<T>();
    let mut value = serde_json::to_value(schema).expect("schema serialization");
    if let Some(obj) = value.as_object_mut() {
        obj.remove("title");
        obj.insert(
            "additionalProperties".into(),
            serde_json::Value::Bool(false),
        );
    }
    value
}

/// Incident status accepted on input. Names (canonical) or Zenduty's
/// internal integer codes (1=triggered, 2=acknowledged, 3=resolved) both
/// work — `list_incidents` results sometimes leak the int form and
/// agents copy it back unchanged. Schema surfaces the enum names.
#[derive(schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
enum IncidentStatusName {
    Triggered,
    Acknowledged,
    Resolved,
}

impl IncidentStatusName {
    fn into_incident_status(self) -> IncidentStatus {
        match self {
            Self::Triggered => IncidentStatus::Triggered,
            Self::Acknowledged => IncidentStatus::Acknowledged,
            Self::Resolved => IncidentStatus::Resolved,
        }
    }
}

impl<'de> serde::Deserialize<'de> for IncidentStatusName {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        match s.to_ascii_lowercase().as_str() {
            "triggered" => Ok(Self::Triggered),
            "acknowledged" | "ack" => Ok(Self::Acknowledged),
            "resolved" | "resolve" => Ok(Self::Resolved),
            other => Err(serde::de::Error::custom(format!(
                "unknown status '{other}' (expected triggered|acknowledged|ack|resolved|resolve)"
            ))),
        }
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
enum StatusInput {
    Name(IncidentStatusName),
    Code(u8),
}

impl StatusInput {
    fn into_incident_status(self) -> Result<IncidentStatus, ToolSetsError> {
        match self {
            Self::Name(n) => Ok(n.into_incident_status()),
            Self::Code(c) => IncidentStatus::try_from(c).map_err(|_| {
                ToolSetsError::InvalidArgument(format!(
                    "unknown status code '{c}' (expected 1=triggered, 2=acknowledged, 3=resolved)"
                ))
            }),
        }
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ListIncidentsArgs {
    /// Filter by status. Accepts canonical names ("triggered",
    /// "acknowledged", "resolved") or Zenduty's int codes (1/2/3).
    /// Defaults to triggered + acknowledged (ongoing) when omitted.
    #[serde(default)]
    statuses: Option<Vec<StatusInput>>,
    #[serde(default)]
    team_id: Option<String>,
    #[serde(default)]
    service_id: Option<String>,
    #[serde(default)]
    page: Option<u32>,
    #[serde(default)]
    page_size: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct IncidentIdArgs {
    /// Zenduty incident `unique_id` — the alphanumeric string returned by
    /// `list_incidents` (e.g. `"HjZEH2pEMC44c89PWMpsEA"`). Not the
    /// `incident_number` (small int). Legacy callers may still pass
    /// `incident_id` for backward compatibility.
    #[serde(alias = "incident_id")]
    unique_id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct AddNoteArgs {
    /// Zenduty incident `unique_id` (alphanumeric, from `list_incidents`).
    #[serde(alias = "incident_id")]
    unique_id: String,
    /// Note body (markdown supported by Zenduty).
    note: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct DeleteNoteArgs {
    /// Zenduty incident `unique_id` (alphanumeric, from `list_incidents`).
    #[serde(alias = "incident_id")]
    unique_id: String,
    /// Note `unique_id` (returned by `add_incident_note` / `list_incident_notes`).
    note_id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct UpdateStatusArgs {
    /// Zenduty incident `unique_id` (alphanumeric, from `list_incidents`).
    #[serde(alias = "incident_id")]
    unique_id: String,
    /// One of "acknowledged" or "resolved". "triggered" is rarely useful.
    status: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ListSchedulesArgs {
    /// Defaults to `toolsets.zenduty.default_team` when omitted.
    #[serde(default)]
    team_id: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct GetScheduleArgs {
    #[serde(default)]
    team_id: Option<String>,
    schedule_id: String,
}

static LIST_INCIDENTS_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<ListIncidentsArgs>);
static INCIDENT_ID_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<IncidentIdArgs>);
static ADD_NOTE_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<AddNoteArgs>);
static DELETE_NOTE_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<DeleteNoteArgs>);
static UPDATE_STATUS_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<UpdateStatusArgs>);
static LIST_SCHEDULES_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<ListSchedulesArgs>);
static GET_SCHEDULE_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<GetScheduleArgs>);

#[derive(serde::Serialize, schemars::JsonSchema)]
struct IncidentSummaryOutput {
    unique_id: Option<String>,
    incident_number: Option<u64>,
    title: String,
    status: Option<String>,
    urgency: Option<u8>,
    creation_date: Option<String>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct IncidentListOutput {
    incidents: Vec<IncidentSummaryOutput>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct IncidentDetailOutput {
    unique_id: Option<String>,
    incident_number: Option<u64>,
    title: String,
    summary: Option<String>,
    status: Option<String>,
    urgency: Option<u8>,
    creation_date: Option<String>,
    acknowledged_date: Option<String>,
    resolved_date: Option<String>,
    #[schemars(schema_with = "crate::toolset::any_json_schema")]
    service: Option<serde_json::Value>,
    #[schemars(schema_with = "crate::toolset::any_json_schema")]
    assigned_to: Option<serde_json::Value>,
    #[schemars(schema_with = "crate::toolset::any_json_schema")]
    team: Option<serde_json::Value>,
    #[schemars(schema_with = "crate::toolset::any_json_schema")]
    escalation_policy: Option<serde_json::Value>,
    #[schemars(schema_with = "crate::toolset::array_of_any_schema")]
    tags: Vec<serde_json::Value>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct NoteOutput {
    unique_id: Option<String>,
    note: String,
    user_name: Option<String>,
    creation_date: Option<String>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct ScheduleSummaryOutput {
    unique_id: Option<String>,
    name: String,
    summary: Option<String>,
    time_zone: Option<String>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct ScheduleListOutput {
    schedules: Vec<ScheduleSummaryOutput>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct ScheduleDetailOutput {
    unique_id: Option<String>,
    name: String,
    summary: Option<String>,
    time_zone: Option<String>,
    #[schemars(schema_with = "crate::toolset::array_of_any_schema")]
    layers: Vec<serde_json::Value>,
    #[schemars(schema_with = "crate::toolset::array_of_any_schema")]
    overrides: Vec<serde_json::Value>,
    #[schemars(schema_with = "crate::toolset::array_of_any_schema")]
    on_call_now: Vec<serde_json::Value>,
}

static OUT_LIST_INCIDENTS: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<IncidentListOutput>);
static OUT_INCIDENT_DETAIL: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<IncidentDetailOutput>);
static OUT_NOTE: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<NoteOutput>);
static OUT_DELETED: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "deleted": { "type": "boolean" },
            "unique_id": { "type": "string" },
            "note_id": { "type": "string" },
        },
        "additionalProperties": false,
    })
});
static OUT_LIST_SCHEDULES: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<ScheduleListOutput>);
static OUT_SCHEDULE_DETAIL: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<ScheduleDetailOutput>);

fn status_name(code: Option<u8>) -> Option<String> {
    code.and_then(|c| IncidentStatus::try_from(c).ok())
        .map(|s| s.as_str().to_string())
}

fn parse_status_name(s: &str) -> Result<IncidentStatus, ToolSetsError> {
    match s.to_ascii_lowercase().as_str() {
        "triggered" => Ok(IncidentStatus::Triggered),
        "acknowledged" | "ack" => Ok(IncidentStatus::Acknowledged),
        "resolved" | "resolve" => Ok(IncidentStatus::Resolved),
        other => Err(ToolSetsError::InvalidArgument(format!(
            "unknown status '{other}' (expected triggered|acknowledged|resolved)"
        ))),
    }
}

pub struct ZendutyToolSet {
    client: Arc<ZendutyClient>,
    default_team: Option<String>,
    tools: Vec<ToolSetEntry>,
}

impl ZendutyToolSet {
    pub fn new(client: ZendutyClient, default_team: Option<String>) -> Self {
        let tools = vec![
            tool_entry(
                "list_incidents",
                "List Zenduty incidents. Defaults to ongoing (triggered + acknowledged). Optionally filter by team_id, service_id, or status names.",
                (*LIST_INCIDENTS_SCHEMA).clone(),
                (*OUT_LIST_INCIDENTS).clone(),
            ),
            tool_entry(
                "get_incident",
                "Get full detail for a Zenduty incident by unique_id, including status, urgency, assignees, service, and team.",
                (*INCIDENT_ID_SCHEMA).clone(),
                (*OUT_INCIDENT_DETAIL).clone(),
            ),
            tool_entry(
                "add_incident_note",
                "Attach a note (comment) to a Zenduty incident. Use this to record context like a CI build URL on an ongoing incident.",
                (*ADD_NOTE_SCHEMA).clone(),
                (*OUT_NOTE).clone(),
            ),
            tool_entry(
                "list_incident_notes",
                "List notes attached to a Zenduty incident.",
                (*INCIDENT_ID_SCHEMA).clone(),
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "notes": {
                            "type": "array",
                            "items": (*OUT_NOTE).clone(),
                        }
                    },
                    "additionalProperties": false,
                }),
            ),
            tool_entry(
                "delete_incident_note",
                "Delete a note from a Zenduty incident by its unique_id.",
                (*DELETE_NOTE_SCHEMA).clone(),
                (*OUT_DELETED).clone(),
            ),
            tool_entry(
                "update_incident_status",
                "Update an incident's status (acknowledged or resolved).",
                (*UPDATE_STATUS_SCHEMA).clone(),
                (*OUT_INCIDENT_DETAIL).clone(),
            ),
            tool_entry(
                "list_schedules",
                "List on-call schedules for a Zenduty team. Falls back to the configured default team when team_id is omitted.",
                (*LIST_SCHEDULES_SCHEMA).clone(),
                (*OUT_LIST_SCHEDULES).clone(),
            ),
            tool_entry(
                "get_schedule",
                "Get a Zenduty schedule's detail including layers, overrides, and the current on-call list.",
                (*GET_SCHEDULE_SCHEMA).clone(),
                (*OUT_SCHEDULE_DETAIL).clone(),
            ),
        ];

        Self {
            client: Arc::new(client),
            default_team,
            tools,
        }
    }

    fn resolve_team(&self, team_id: Option<String>) -> Result<String, ToolSetsError> {
        match team_id.or_else(|| self.default_team.clone()) {
            Some(t) if !t.is_empty() => Ok(t),
            _ => Err(ToolSetsError::MissingArgument(
                "team_id (no default_team configured)".to_string(),
            )),
        }
    }
}

#[async_trait::async_trait]
impl SearchableToolSet for ZendutyToolSet {
    fn name(&self) -> &str {
        "zenduty"
    }

    fn category(&self) -> &str {
        "incident-response"
    }

    fn category_description(&self) -> &str {
        "Zenduty incidents, notes, and on-call schedules"
    }

    fn tools(&self) -> &[ToolSetEntry] {
        &self.tools
    }

    async fn call(
        &self,
        _subject: &AuthSubject,
        tool_name: &str,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        match tool_name {
            "list_incidents" => {
                let args: ListIncidentsArgs = parse_params(arguments)?;
                let status_codes: Vec<u8> = match args.statuses {
                    Some(inputs) => inputs
                        .into_iter()
                        .map(|s| s.into_incident_status().map(|st| st.as_u8()))
                        .collect::<Result<Vec<_>, _>>()?,
                    None => vec![
                        IncidentStatus::Triggered.as_u8(),
                        IncidentStatus::Acknowledged.as_u8(),
                    ],
                };
                let params = ListIncidentsParams {
                    status: Some(&status_codes),
                    team_id: args.team_id.as_deref(),
                    service_id: args.service_id.as_deref(),
                    page: args.page,
                    page_size: args.page_size,
                };
                let incidents = self.client.list_incidents(&params).await?;
                let out = IncidentListOutput {
                    incidents: incidents
                        .iter()
                        .map(|i| IncidentSummaryOutput {
                            unique_id: i.unique_id.clone(),
                            incident_number: i.incident_number,
                            title: i.title.clone(),
                            status: status_name(i.status),
                            urgency: i.urgency,
                            creation_date: i.creation_date.clone(),
                        })
                        .collect(),
                };
                Ok(typed_result(&out))
            }
            "get_incident" => {
                let args: IncidentIdArgs = parse_params(arguments)?;
                let i = self.client.get_incident(&args.unique_id).await?;
                let out = IncidentDetailOutput {
                    unique_id: i.unique_id,
                    incident_number: i.incident_number,
                    title: i.title,
                    summary: i.summary,
                    status: status_name(i.status),
                    urgency: i.urgency,
                    creation_date: i.creation_date,
                    acknowledged_date: i.acknowledged_date,
                    resolved_date: i.resolved_date,
                    service: i.service_object.or(i.service),
                    assigned_to: i.assigned_to,
                    team: i.team,
                    escalation_policy: i.escalation_policy_object.or(i.escalation_policy),
                    tags: i.tags,
                };
                Ok(typed_result(&out))
            }
            "add_incident_note" => {
                let args: AddNoteArgs = parse_params(arguments)?;
                let note = self
                    .client
                    .add_incident_note(&args.unique_id, &args.note)
                    .await?;
                let out = NoteOutput {
                    unique_id: note.unique_id,
                    note: note.note,
                    user_name: note.user_name,
                    creation_date: note.creation_date,
                };
                Ok(typed_result(&out))
            }
            "list_incident_notes" => {
                let args: IncidentIdArgs = parse_params(arguments)?;
                let notes = self.client.list_incident_notes(&args.unique_id).await?;
                let mapped: Vec<NoteOutput> = notes
                    .into_iter()
                    .map(|n| NoteOutput {
                        unique_id: n.unique_id,
                        note: n.note,
                        user_name: n.user_name,
                        creation_date: n.creation_date,
                    })
                    .collect();
                let out = serde_json::json!({ "notes": mapped });
                let text = serde_json::to_string_pretty(&out).unwrap_or_default();
                let mut result = CallToolResult::success(vec![Content::text(text)]);
                result.structured_content = Some(out);
                Ok(result)
            }
            "delete_incident_note" => {
                let args: DeleteNoteArgs = parse_params(arguments)?;
                self.client
                    .delete_incident_note(&args.unique_id, &args.note_id)
                    .await?;
                let out = serde_json::json!({
                    "deleted": true,
                    "unique_id": args.unique_id,
                    "note_id": args.note_id,
                });
                let text = serde_json::to_string_pretty(&out).unwrap_or_default();
                let mut result = CallToolResult::success(vec![Content::text(text)]);
                result.structured_content = Some(out);
                Ok(result)
            }
            "update_incident_status" => {
                let args: UpdateStatusArgs = parse_params(arguments)?;
                let status = parse_status_name(&args.status)?;
                let i = self
                    .client
                    .update_incident_status(&args.unique_id, status)
                    .await?;
                let out = IncidentDetailOutput {
                    unique_id: i.unique_id,
                    incident_number: i.incident_number,
                    title: i.title,
                    summary: i.summary,
                    status: status_name(i.status),
                    urgency: i.urgency,
                    creation_date: i.creation_date,
                    acknowledged_date: i.acknowledged_date,
                    resolved_date: i.resolved_date,
                    service: i.service_object.or(i.service),
                    assigned_to: i.assigned_to,
                    team: i.team,
                    escalation_policy: i.escalation_policy_object.or(i.escalation_policy),
                    tags: i.tags,
                };
                Ok(typed_result(&out))
            }
            "list_schedules" => {
                let args: ListSchedulesArgs = parse_params(arguments)?;
                let team = self.resolve_team(args.team_id)?;
                let schedules = self.client.list_schedules(&team).await?;
                let out = ScheduleListOutput {
                    schedules: schedules
                        .iter()
                        .map(|s| ScheduleSummaryOutput {
                            unique_id: s.unique_id.clone(),
                            name: s.name.clone(),
                            summary: s.summary.clone(),
                            time_zone: s.time_zone.clone(),
                        })
                        .collect(),
                };
                Ok(typed_result(&out))
            }
            "get_schedule" => {
                let args: GetScheduleArgs = parse_params(arguments)?;
                let team = self.resolve_team(args.team_id)?;
                let s = self.client.get_schedule(&team, &args.schedule_id).await?;
                let out = ScheduleDetailOutput {
                    unique_id: s.unique_id,
                    name: s.name,
                    summary: s.summary,
                    time_zone: s.time_zone,
                    layers: s.layers,
                    overrides: s.overrides,
                    on_call_now: s.on_call_now,
                };
                Ok(typed_result(&out))
            }
            _ => Err(ToolSetsError::ToolNotFound(tool_name.to_string())),
        }
    }
}

fn tool_entry(
    name: &str,
    description: &str,
    schema: serde_json::Value,
    output_schema: serde_json::Value,
) -> ToolSetEntry {
    let input_schema: JsonObject = match schema {
        serde_json::Value::Object(m) => m,
        _ => Default::default(),
    };
    let out_schema = match output_schema {
        serde_json::Value::Object(m) => Some(Arc::new(m)),
        _ => None,
    };
    let mut tool = Tool::default();
    tool.name = name.to_string().into();
    tool.description = Some(description.to_string().into());
    tool.input_schema = Arc::new(input_schema);
    tool.output_schema = out_schema;
    ToolSetEntry {
        name: name.to_string(),
        description: tool,
    }
}

fn typed_result<T: serde::Serialize>(value: &T) -> CallToolResult {
    let structured = serde_json::to_value(value).expect("serialization");
    let text = serde_json::to_string_pretty(&structured).unwrap_or_default();
    let mut result = CallToolResult::success(vec![Content::text(text)]);
    result.structured_content = Some(structured);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_status_accepts_canonical_names() {
        assert_eq!(
            parse_status_name("triggered").unwrap(),
            IncidentStatus::Triggered
        );
        assert_eq!(
            parse_status_name("ACKNOWLEDGED").unwrap(),
            IncidentStatus::Acknowledged
        );
        assert_eq!(
            parse_status_name("Resolved").unwrap(),
            IncidentStatus::Resolved
        );
    }

    #[test]
    fn parse_status_rejects_unknown() {
        assert!(parse_status_name("closed").is_err());
    }

    #[test]
    fn status_name_round_trips() {
        assert_eq!(status_name(Some(1)).as_deref(), Some("triggered"));
        assert_eq!(status_name(Some(2)).as_deref(), Some("acknowledged"));
        assert_eq!(status_name(Some(3)).as_deref(), Some("resolved"));
        assert_eq!(status_name(Some(99)), None);
        assert_eq!(status_name(None), None);
    }

    #[test]
    fn incident_id_args_accepts_unique_id_field() {
        let args: IncidentIdArgs =
            serde_json::from_value(serde_json::json!({ "unique_id": "HjZEH2pEMC44c89PWMpsEA" }))
                .expect("unique_id form deserializes");
        assert_eq!(args.unique_id, "HjZEH2pEMC44c89PWMpsEA");
    }

    #[test]
    fn incident_id_args_still_accepts_legacy_incident_id_alias() {
        let args: IncidentIdArgs =
            serde_json::from_value(serde_json::json!({ "incident_id": "HjZEH2pEMC44c89PWMpsEA" }))
                .expect("legacy incident_id alias deserializes");
        assert_eq!(args.unique_id, "HjZEH2pEMC44c89PWMpsEA");
    }

    #[test]
    fn list_incidents_args_accepts_name_strings() {
        let args: ListIncidentsArgs = serde_json::from_value(serde_json::json!({
            "statuses": ["triggered", "acknowledged"],
        }))
        .expect("name form deserializes");
        let codes: Vec<u8> = args
            .statuses
            .unwrap()
            .into_iter()
            .map(|s| s.into_incident_status().unwrap().as_u8())
            .collect();
        assert_eq!(codes, vec![1, 2]);
    }

    #[test]
    fn list_incidents_args_accepts_int_codes() {
        let args: ListIncidentsArgs = serde_json::from_value(serde_json::json!({
            "statuses": [1, 2, 3],
        }))
        .expect("int form deserializes");
        let codes: Vec<u8> = args
            .statuses
            .unwrap()
            .into_iter()
            .map(|s| s.into_incident_status().unwrap().as_u8())
            .collect();
        assert_eq!(codes, vec![1, 2, 3]);
    }

    #[test]
    fn list_incidents_args_accepts_mixed_names_and_ints() {
        let args: ListIncidentsArgs = serde_json::from_value(serde_json::json!({
            "statuses": ["triggered", 2, "resolved"],
        }))
        .expect("mixed form deserializes");
        let codes: Vec<u8> = args
            .statuses
            .unwrap()
            .into_iter()
            .map(|s| s.into_incident_status().unwrap().as_u8())
            .collect();
        assert_eq!(codes, vec![1, 2, 3]);
    }

    #[test]
    fn status_input_rejects_unknown_int_code() {
        let args: ListIncidentsArgs = serde_json::from_value(serde_json::json!({
            "statuses": [9],
        }))
        .unwrap();
        let err = args
            .statuses
            .unwrap()
            .into_iter()
            .map(|s| s.into_incident_status())
            .collect::<Result<Vec<_>, _>>()
            .unwrap_err();
        assert!(
            err.to_string().contains("unknown status code '9'"),
            "got: {err}"
        );
    }

    #[test]
    fn list_incidents_schema_exposes_canonical_status_names() {
        let schema = serde_json::to_value(&*LIST_INCIDENTS_SCHEMA).unwrap();
        // Walk to any `enum` keyword whose array matches the canonical names.
        // Searching recursively avoids brittleness over schemars's exact layout
        // for the untagged `StatusInput` (it may emit `anyOf` of variants).
        fn find_canonical_enum(v: &serde_json::Value) -> bool {
            if let Some(arr) = v.get("enum").and_then(|e| e.as_array()) {
                let names: Vec<&str> = arr.iter().filter_map(|x| x.as_str()).collect();
                if names.contains(&"triggered")
                    && names.contains(&"acknowledged")
                    && names.contains(&"resolved")
                {
                    return true;
                }
            }
            match v {
                serde_json::Value::Object(map) => map.values().any(find_canonical_enum),
                serde_json::Value::Array(arr) => arr.iter().any(find_canonical_enum),
                _ => false,
            }
        }
        assert!(
            find_canonical_enum(&schema),
            "schema does not surface enum constraint for statuses: {schema}"
        );
    }

    #[test]
    fn status_input_is_case_insensitive_and_accepts_aliases() {
        for (input, expected_code) in [
            ("TRIGGERED", 1u8),
            ("Acknowledged", 2),
            ("ack", 2),
            ("RESOLVED", 3),
            ("resolve", 3),
        ] {
            let args: ListIncidentsArgs = serde_json::from_value(serde_json::json!({
                "statuses": [input],
            }))
            .unwrap_or_else(|e| panic!("'{input}' should deserialize: {e}"));
            let code = args
                .statuses
                .unwrap()
                .into_iter()
                .next()
                .unwrap()
                .into_incident_status()
                .unwrap_or_else(|e| panic!("'{input}' should convert: {e}"))
                .as_u8();
            assert_eq!(code, expected_code, "input: {input}");
        }
    }
}
