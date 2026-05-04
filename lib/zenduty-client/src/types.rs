use serde::{Deserialize, Serialize};

/// Zenduty incident statuses are integer-coded on the wire.
/// See <https://apidocs.zenduty.com/>.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "u8", try_from = "u8")]
pub enum IncidentStatus {
    Triggered,
    Acknowledged,
    Resolved,
}

impl IncidentStatus {
    pub fn as_u8(self) -> u8 {
        match self {
            Self::Triggered => 1,
            Self::Acknowledged => 2,
            Self::Resolved => 3,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Triggered => "triggered",
            Self::Acknowledged => "acknowledged",
            Self::Resolved => "resolved",
        }
    }
}

impl From<IncidentStatus> for u8 {
    fn from(value: IncidentStatus) -> Self {
        value.as_u8()
    }
}

impl TryFrom<u8> for IncidentStatus {
    type Error = String;
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Triggered),
            2 => Ok(Self::Acknowledged),
            3 => Ok(Self::Resolved),
            other => Err(format!("unknown incident status code: {other}")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Incident {
    #[serde(default)]
    pub id: Option<u64>,
    /// Zenduty's stable string id used in URL paths (e.g. notes endpoint).
    #[serde(default)]
    pub unique_id: Option<String>,
    #[serde(default)]
    pub incident_number: Option<u64>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub status: Option<u8>,
    #[serde(default)]
    pub urgency: Option<u8>,
    #[serde(default)]
    pub creation_date: Option<String>,
    #[serde(default)]
    pub acknowledged_date: Option<String>,
    #[serde(default)]
    pub resolved_date: Option<String>,
    #[serde(default)]
    pub service: Option<serde_json::Value>,
    #[serde(default)]
    pub service_object: Option<serde_json::Value>,
    #[serde(default)]
    pub assigned_to: Option<serde_json::Value>,
    #[serde(default)]
    pub team: Option<serde_json::Value>,
    #[serde(default)]
    pub escalation_policy: Option<serde_json::Value>,
    #[serde(default)]
    pub escalation_policy_object: Option<serde_json::Value>,
    #[serde(default)]
    pub tags: Vec<serde_json::Value>,
    #[serde(default)]
    pub merged_with: Option<serde_json::Value>,
    /// Catch-all for fields the schema evolves to add (alert_key, sla,
    /// integration metadata) without forcing a client release.
    #[serde(flatten, default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncidentNote {
    #[serde(default)]
    pub unique_id: Option<String>,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub user_name: Option<String>,
    #[serde(default)]
    pub incident: Option<u64>,
    #[serde(default)]
    pub creation_date: Option<String>,
    #[serde(flatten, default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NewIncidentNote<'a> {
    pub note: &'a str,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct IncidentStatusUpdate {
    /// Integer status code: 1=triggered, 2=acknowledged, 3=resolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u8>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ListIncidentsParams<'a> {
    pub status: Option<&'a [u8]>,
    pub team_id: Option<&'a str>,
    pub service_id: Option<&'a str>,
    pub page: Option<u32>,
    pub page_size: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schedule {
    #[serde(default)]
    pub unique_id: Option<String>,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub time_zone: Option<String>,
    #[serde(default)]
    pub team: Option<serde_json::Value>,
    #[serde(default)]
    pub layers: Vec<serde_json::Value>,
    #[serde(default)]
    pub overrides: Vec<serde_json::Value>,
    #[serde(default)]
    pub on_call_now: Vec<serde_json::Value>,
    #[serde(flatten, default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Zenduty list endpoints return either a bare array or a DRF-paginated
/// envelope `{count, next, previous, results: [...]}`. This handles both.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Page<T> {
    Paginated {
        #[serde(default)]
        count: Option<u64>,
        #[serde(default)]
        next: Option<String>,
        #[serde(default)]
        previous: Option<String>,
        results: Vec<T>,
    },
    Bare(Vec<T>),
}

impl<T> Page<T> {
    pub fn into_results(self) -> Vec<T> {
        match self {
            Page::Paginated { results, .. } => results,
            Page::Bare(v) => v,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_deserializes_bare_array() {
        let raw = r#"[{"name":"a"},{"name":"b"}]"#;
        let page: Page<Schedule> = serde_json::from_str(raw).unwrap();
        let results = page.into_results();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].name, "a");
    }

    #[test]
    fn page_deserializes_drf_envelope() {
        let raw = r#"{"count":1,"next":null,"previous":null,"results":[{"name":"x"}]}"#;
        let page: Page<Schedule> = serde_json::from_str(raw).unwrap();
        let results = page.into_results();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "x");
    }

    #[test]
    fn incident_status_round_trips() {
        for s in [
            IncidentStatus::Triggered,
            IncidentStatus::Acknowledged,
            IncidentStatus::Resolved,
        ] {
            let n = s.as_u8();
            assert_eq!(IncidentStatus::try_from(n).unwrap(), s);
        }
    }
}
