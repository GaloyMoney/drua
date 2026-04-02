use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use es_entity::*;

use crate::primitives::*;

#[derive(EsEvent, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[es_event(id = "ReportId")]
pub enum ReportEvent {
    Initialized {
        id: ReportId,
        title: String,
        content: String,
        tags: Vec<String>,
    },
    Pinned {},
    Unpinned {},
}

#[derive(EsEntity, Builder)]
#[builder(pattern = "owned", build_fn(error = "EntityHydrationError"))]
pub struct Report {
    pub id: ReportId,
    pub title: String,
    pub content: String,
    pub tags: Vec<String>,
    #[builder(default)]
    pub pinned: bool,
    events: EntityEvents<ReportEvent>,
}

impl Report {
    pub fn created_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.events
            .entity_first_persisted_at()
            .expect("entity_first_persisted_at not found")
    }
}

impl core::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Report: {}, title: {}", self.id, self.title)
    }
}

impl TryFromEvents<ReportEvent> for Report {
    fn try_from_events(events: EntityEvents<ReportEvent>) -> Result<Self, EntityHydrationError> {
        let mut builder = ReportBuilder::default();

        for event in events.iter_all() {
            match event {
                ReportEvent::Initialized {
                    id,
                    title,
                    content,
                    tags,
                } => {
                    builder = builder
                        .id(*id)
                        .title(title.clone())
                        .content(content.clone())
                        .tags(tags.clone());
                }
                ReportEvent::Pinned {} => {
                    builder = builder.pinned(true);
                }
                ReportEvent::Unpinned {} => {
                    builder = builder.pinned(false);
                }
            }
        }

        builder.events(events).build()
    }
}

#[derive(Debug, Builder)]
pub struct NewReport {
    #[builder(setter(into))]
    pub(super) id: ReportId,
    #[builder(setter(into))]
    pub(super) title: String,
    #[builder(setter(into))]
    pub(super) content: String,
    pub(super) tags: Vec<String>,
}

impl NewReport {
    pub fn builder() -> NewReportBuilder {
        let mut builder = NewReportBuilder::default();
        builder.id(ReportId::new());
        builder
    }
}

impl IntoEvents<ReportEvent> for NewReport {
    fn into_events(self) -> EntityEvents<ReportEvent> {
        EntityEvents::init(
            self.id,
            [ReportEvent::Initialized {
                id: self.id,
                title: self.title,
                content: self.content,
                tags: self.tags,
            }],
        )
    }
}

/// A search result with relevance scoring.
#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    pub id: ReportId,
    pub title: String,
    pub content: String,
    pub tags: Vec<String>,
    pub score: f64,
    pub pinned: bool,
}

#[cfg(test)]
mod tests {
    use es_entity::{IntoEvents as _, TryFromEvents as _};

    use crate::primitives::ReportId;

    use super::{NewReport, Report};

    fn new_report() -> Report {
        let new = NewReport::builder()
            .id(ReportId::new())
            .title("Test finding")
            .content("Some important content")
            .tags(vec!["test".to_string()])
            .build()
            .unwrap();

        Report::try_from_events(new.into_events()).unwrap()
    }

    #[test]
    fn report_hydration() {
        let report = new_report();
        assert_eq!(report.title, "Test finding");
        assert_eq!(report.content, "Some important content");
        assert_eq!(report.tags, vec!["test"]);
        assert!(!report.pinned);
    }

    #[test]
    fn report_hydration_minimal() {
        let new = NewReport::builder()
            .id(ReportId::new())
            .title("Minimal report")
            .content("content")
            .tags(vec![])
            .build()
            .unwrap();

        let report = Report::try_from_events(new.into_events()).unwrap();
        assert_eq!(report.title, "Minimal report");
        assert!(!report.pinned);
    }
}
