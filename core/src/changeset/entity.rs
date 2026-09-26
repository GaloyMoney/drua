use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use es_entity::*;

use super::error::ChangesetError;
use crate::primitives::*;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "VARCHAR", rename_all = "snake_case")]
pub enum ChangesetStatus {
    Open,
    Submitted,
    Merged,
    Applied,
    Discarded,
    Abandoned,
}

impl ChangesetStatus {
    /// `Merged | Applied` in prose from the handoff — the tree reached
    /// `main` one way or the other.
    pub fn is_landed(self) -> bool {
        matches!(self, ChangesetStatus::Merged | ChangesetStatus::Applied)
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            ChangesetStatus::Merged
                | ChangesetStatus::Applied
                | ChangesetStatus::Discarded
                | ChangesetStatus::Abandoned
        )
    }
}

/// Who opened (or acted on) a changeset. Used both for `Opened.opened_by`
/// and `Applied.applied_by`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChangesetActor {
    Agent { agent_id: AgentId },
    WorkflowRun { run_id: WorkflowRunId },
    User { user_id: UserId },
}

impl ChangesetActor {
    pub fn agent_id(&self) -> Option<AgentId> {
        match self {
            ChangesetActor::Agent { agent_id } => Some(*agent_id),
            _ => None,
        }
    }

    pub fn workflow_run_id(&self) -> Option<WorkflowRunId> {
        match self {
            ChangesetActor::WorkflowRun { run_id } => Some(*run_id),
            _ => None,
        }
    }
}

/// rev2 D4: the `opened_by_actor` repo column encoding — also the
/// `draft_for` lookup key, so a schema change here needs the partial
/// unique index (`changesets_opened_by_actor_key`) renamed to match.
impl core::fmt::Display for ChangesetActor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChangesetActor::Agent { agent_id } => write!(f, "agent:{agent_id}"),
            ChangesetActor::WorkflowRun { run_id } => write!(f, "run:{run_id}"),
            ChangesetActor::User { user_id } => write!(f, "user:{user_id}"),
        }
    }
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("invalid changeset actor encoding: {0:?}")]
pub struct ParseChangesetActorError(String);

impl std::str::FromStr for ChangesetActor {
    type Err = ParseChangesetActorError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = || ParseChangesetActorError(s.to_string());
        let (kind, id) = s.split_once(':').ok_or_else(invalid)?;
        match kind {
            "agent" => Ok(ChangesetActor::Agent {
                agent_id: id.parse().map_err(|_| invalid())?,
            }),
            "run" => Ok(ChangesetActor::WorkflowRun {
                run_id: id.parse().map_err(|_| invalid())?,
            }),
            "user" => Ok(ChangesetActor::User {
                user_id: id.parse().map_err(|_| invalid())?,
            }),
            _ => Err(invalid()),
        }
    }
}

#[derive(EsEvent, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[es_event(id = "ChangesetId")]
pub enum ChangesetEvent {
    Opened {
        id: ChangesetId,
        /// `None` for a project-less draft — a bare-`Admin` subject
        /// has no project context (rev3 D16).
        project_id: Option<ProjectId>,
        title: String,
        description: Option<String>,
        base_oid: String,
        opened_by: ChangesetActor,
    },
    /// One per landed git op. `head_oid` is the branch tip after the op.
    CommitRecorded {
        head_oid: String,
        action: String,
        path: String,
    },
    Rebased {
        base_oid: String,
        head_oid: String,
    },
    Submitted {
        head_oid: String,
        pr_number: u64,
        pr_url: String,
    },
    Applied {
        merge_oid: String,
        applied_by: ChangesetActor,
    },
    Merged {
        merge_oid: String,
    },
    Abandoned,
    Discarded {
        reason: Option<String>,
    },
}

#[derive(EsEntity, Builder)]
#[builder(pattern = "owned", build_fn(error = "EntityHydrationError"))]
pub struct Changeset {
    pub id: ChangesetId,
    /// `None` for a project-less draft (rev3 D16 — a bare-`Admin`
    /// subject's draft; ownership and lookup are by actor, never by
    /// project).
    pub project_id: Option<ProjectId>,
    pub title: String,
    #[builder(setter(strip_option), default)]
    pub description: Option<String>,
    pub base_oid: String,
    pub head_oid: String,
    pub status: ChangesetStatus,
    pub opened_by: ChangesetActor,
    #[builder(default)]
    pub pr_number: Option<u64>,
    #[builder(default)]
    pub pr_url: Option<String>,
    events: EntityEvents<ChangesetEvent>,
}

impl Changeset {
    /// Branch name for `id`, without needing a hydrated entity — lets
    /// `SpaceFs` name a changeset's ref from just the id it parsed out
    /// of a `space:<slug>@<id>/<rel>` path.
    pub fn branch_for(id: ChangesetId) -> String {
        format!("drua/{id}")
    }

    pub fn git_ref_for(id: ChangesetId) -> String {
        format!("refs/heads/{}", Self::branch_for(id))
    }

    pub fn branch(&self) -> String {
        Self::branch_for(self.id)
    }

    pub fn git_ref(&self) -> String {
        Self::git_ref_for(self.id)
    }

    pub fn is_open(&self) -> bool {
        self.status == ChangesetStatus::Open
    }

    pub fn created_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.events
            .entity_first_persisted_at()
            .expect("entity_first_persisted_at not found")
    }

    /// Ops recorded since the last `Opened`/`Rebased` — i.e. against the
    /// changeset's *current* base, not its whole history.
    pub fn commit_count(&self) -> usize {
        let mut count = 0;
        for event in self.events.iter_all().rev() {
            match event {
                ChangesetEvent::CommitRecorded { .. } => count += 1,
                ChangesetEvent::Opened { .. } | ChangesetEvent::Rebased { .. } => break,
                _ => {}
            }
        }
        count
    }

    /// Whether there's anything to land. Unlike `commit_count`, this
    /// survives a rebase: a clean rebase resets `commit_count` to 0
    /// (it only counts ops since the last `Opened`/`Rebased`) even
    /// though `head_oid` still differs from `base_oid` for a draft
    /// with real prior content (bugbot 2026-09-26).
    pub fn has_commits(&self) -> bool {
        self.head_oid != self.base_oid
    }

    fn invalid_transition(&self, op: &'static str) -> ChangesetError {
        ChangesetError::InvalidTransition {
            from: self.status,
            op,
        }
    }

    /// No-op (`AlreadyApplied`) if `head_oid` already matches — replaying
    /// the same commit oid (e.g. a retried write) shouldn't grow the
    /// event log. `Err(InvalidTransition)` unless `Open`.
    pub fn record_commit(
        &mut self,
        head_oid: String,
        action: impl Into<String>,
        path: impl Into<String>,
    ) -> Result<Idempotent<()>, ChangesetError> {
        if !self.is_open() {
            return Err(self.invalid_transition("record_commit"));
        }
        if head_oid == self.head_oid {
            return Ok(Idempotent::AlreadyApplied);
        }
        self.head_oid = head_oid.clone();
        self.events.push(ChangesetEvent::CommitRecorded {
            head_oid,
            action: action.into(),
            path: path.into(),
        });
        Ok(Idempotent::Executed(()))
    }

    /// `Open` only — moves the changeset's fork point without losing its
    /// branch identity. No-op if `base_oid`/`head_oid` are unchanged.
    pub fn rebase(
        &mut self,
        base_oid: String,
        head_oid: String,
    ) -> Result<Idempotent<()>, ChangesetError> {
        if !self.is_open() {
            return Err(self.invalid_transition("rebase"));
        }
        if base_oid == self.base_oid && head_oid == self.head_oid {
            return Ok(Idempotent::AlreadyApplied);
        }
        self.base_oid = base_oid.clone();
        self.head_oid = head_oid.clone();
        self.events
            .push(ChangesetEvent::Rebased { base_oid, head_oid });
        Ok(Idempotent::Executed(()))
    }

    pub fn submit(
        &mut self,
        head_oid: String,
        pr_number: u64,
        pr_url: String,
    ) -> Result<Idempotent<()>, ChangesetError> {
        idempotency_guard!(
            self.events.iter_all().rev(),
            already_applied: ChangesetEvent::Submitted { .. },
        );
        if !self.is_open() {
            return Err(self.invalid_transition("submit"));
        }
        self.status = ChangesetStatus::Submitted;
        self.head_oid = head_oid.clone();
        self.pr_number = Some(pr_number);
        self.pr_url = Some(pr_url.clone());
        self.events.push(ChangesetEvent::Submitted {
            head_oid,
            pr_number,
            pr_url,
        });
        Ok(Idempotent::Executed(()))
    }

    /// `Open` or `Submitted` — drua merges the branch into `main` itself.
    pub fn apply(
        &mut self,
        merge_oid: String,
        applied_by: ChangesetActor,
    ) -> Result<Idempotent<()>, ChangesetError> {
        idempotency_guard!(
            self.events.iter_all().rev(),
            already_applied: ChangesetEvent::Applied { .. },
        );
        if !matches!(
            self.status,
            ChangesetStatus::Open | ChangesetStatus::Submitted
        ) {
            return Err(self.invalid_transition("apply"));
        }
        self.status = ChangesetStatus::Applied;
        self.events.push(ChangesetEvent::Applied {
            merge_oid,
            applied_by,
        });
        Ok(Idempotent::Executed(()))
    }

    /// The sync job observed the branch merged on GitHub. Valid from
    /// `Submitted`, and from `Open` too — a human may merge a branch
    /// that was never submitted through drua.
    pub fn mark_merged(&mut self, merge_oid: String) -> Result<Idempotent<()>, ChangesetError> {
        idempotency_guard!(
            self.events.iter_all().rev(),
            already_applied: ChangesetEvent::Merged { .. },
        );
        if !matches!(
            self.status,
            ChangesetStatus::Open | ChangesetStatus::Submitted
        ) {
            return Err(self.invalid_transition("mark_merged"));
        }
        self.status = ChangesetStatus::Merged;
        self.events.push(ChangesetEvent::Merged { merge_oid });
        Ok(Idempotent::Executed(()))
    }

    /// The sync job observed the branch gone from origin without a merge
    /// (PR closed, branch deleted). `Submitted` only.
    pub fn mark_abandoned(&mut self) -> Result<Idempotent<()>, ChangesetError> {
        idempotency_guard!(
            self.events.iter_all().rev(),
            already_applied: ChangesetEvent::Abandoned,
        );
        if self.status != ChangesetStatus::Submitted {
            return Err(self.invalid_transition("mark_abandoned"));
        }
        self.status = ChangesetStatus::Abandoned;
        self.events.push(ChangesetEvent::Abandoned);
        Ok(Idempotent::Executed(()))
    }

    /// `Open` or `Submitted`.
    pub fn discard(&mut self, reason: Option<String>) -> Result<Idempotent<()>, ChangesetError> {
        idempotency_guard!(
            self.events.iter_all().rev(),
            already_applied: ChangesetEvent::Discarded { .. },
        );
        if !matches!(
            self.status,
            ChangesetStatus::Open | ChangesetStatus::Submitted
        ) {
            return Err(self.invalid_transition("discard"));
        }
        self.status = ChangesetStatus::Discarded;
        self.events.push(ChangesetEvent::Discarded {
            reason: reason.clone(),
        });
        Ok(Idempotent::Executed(()))
    }
}

impl core::fmt::Display for Changeset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Changeset: {}, title: {}, status: {:?}",
            self.id, self.title, self.status
        )
    }
}

impl TryFromEvents<ChangesetEvent> for Changeset {
    fn try_from_events(events: EntityEvents<ChangesetEvent>) -> Result<Self, EntityHydrationError> {
        let mut builder = ChangesetBuilder::default();
        let mut status = ChangesetStatus::Open;

        for event in events.iter_all() {
            match event {
                ChangesetEvent::Opened {
                    id,
                    project_id,
                    title,
                    description,
                    base_oid,
                    opened_by,
                } => {
                    builder = builder
                        .id(*id)
                        .project_id(*project_id)
                        .title(title.clone())
                        .base_oid(base_oid.clone())
                        .head_oid(base_oid.clone())
                        .opened_by(*opened_by);
                    if let Some(desc) = description {
                        builder = builder.description(desc.clone());
                    }
                }
                ChangesetEvent::CommitRecorded { head_oid, .. } => {
                    builder = builder.head_oid(head_oid.clone());
                }
                ChangesetEvent::Rebased { base_oid, head_oid } => {
                    builder = builder
                        .base_oid(base_oid.clone())
                        .head_oid(head_oid.clone());
                    status = ChangesetStatus::Open;
                }
                ChangesetEvent::Submitted {
                    head_oid,
                    pr_number,
                    pr_url,
                } => {
                    builder = builder
                        .head_oid(head_oid.clone())
                        .pr_number(Some(*pr_number))
                        .pr_url(Some(pr_url.clone()));
                    status = ChangesetStatus::Submitted;
                }
                ChangesetEvent::Applied { .. } => {
                    status = ChangesetStatus::Applied;
                }
                ChangesetEvent::Merged { .. } => {
                    status = ChangesetStatus::Merged;
                }
                ChangesetEvent::Abandoned => {
                    status = ChangesetStatus::Abandoned;
                }
                ChangesetEvent::Discarded { .. } => {
                    status = ChangesetStatus::Discarded;
                }
            }
        }

        builder = builder.status(status);
        builder.events(events).build()
    }
}

#[derive(Debug, Builder)]
#[builder(pattern = "owned")]
pub struct NewChangeset {
    #[builder(setter(into))]
    pub(super) id: ChangesetId,
    /// `None` for a project-less draft (rev3 D16).
    #[builder(default)]
    pub(super) project_id: Option<ProjectId>,
    #[builder(setter(into))]
    pub(super) title: String,
    #[builder(setter(into, strip_option), default)]
    pub(super) description: Option<String>,
    #[builder(setter(into))]
    pub(super) base_oid: String,
    pub(super) opened_by: ChangesetActor,
}

impl NewChangeset {
    pub fn builder() -> NewChangesetBuilder {
        NewChangesetBuilder::default().id(ChangesetId::new())
    }

    /// `EsRepo`'s `create(accessor = "initial_status()")` for the
    /// `status` column — every changeset starts `Open`.
    pub(crate) fn initial_status(&self) -> ChangesetStatus {
        ChangesetStatus::Open
    }
}

impl IntoEvents<ChangesetEvent> for NewChangeset {
    fn into_events(self) -> EntityEvents<ChangesetEvent> {
        EntityEvents::init(
            self.id,
            [ChangesetEvent::Opened {
                id: self.id,
                project_id: self.project_id,
                title: self.title,
                description: self.description,
                base_oid: self.base_oid,
                opened_by: self.opened_by,
            }],
        )
    }
}

#[cfg(test)]
mod tests {
    use es_entity::{IntoEvents as _, TryFromEvents as _};

    use super::*;

    fn agent_actor() -> ChangesetActor {
        ChangesetActor::Agent {
            agent_id: AgentId::new(),
        }
    }

    fn open_changeset() -> Changeset {
        let new = NewChangeset::builder()
            .project_id(Some(ProjectId::new()))
            .title("curate: relink notes")
            .base_oid("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            .opened_by(agent_actor())
            .build()
            .unwrap();
        Changeset::try_from_events(new.into_events()).unwrap()
    }

    #[test]
    fn hydration_sets_base_and_head_to_base_oid() {
        let cs = open_changeset();
        assert_eq!(cs.status, ChangesetStatus::Open);
        assert_eq!(cs.base_oid, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert_eq!(cs.head_oid, cs.base_oid);
        assert_eq!(cs.commit_count(), 0);
        assert!(cs.pr_number.is_none());
        assert!(cs.pr_url.is_none());
    }

    #[test]
    fn hydration_round_trip_preserves_state() {
        let mut cs = open_changeset();
        cs.record_commit("bbb...".to_string(), "edit", "spaces/s/a.md")
            .unwrap()
            .did_execute();
        cs.record_commit("ccc...".to_string(), "edit", "spaces/s/b.md")
            .unwrap()
            .did_execute();

        let events = cs.events;
        let rehydrated = Changeset::try_from_events(events).unwrap();
        assert_eq!(rehydrated.head_oid, "ccc...");
        assert_eq!(rehydrated.commit_count(), 2);
        assert_eq!(rehydrated.status, ChangesetStatus::Open);
    }

    #[test]
    fn record_commit_rejected_when_not_open() {
        let mut cs = open_changeset();
        cs.discard(None).unwrap().did_execute();
        let outcome = cs.record_commit("bbb...".to_string(), "edit", "a.md");
        assert!(matches!(
            outcome,
            Err(ChangesetError::InvalidTransition {
                from: ChangesetStatus::Discarded,
                op: "record_commit"
            })
        ));
    }

    #[test]
    fn record_commit_is_noop_on_same_oid() {
        let mut cs = open_changeset();
        let base = cs.head_oid.clone();
        let outcome = cs.record_commit(base, "edit", "a.md").unwrap();
        assert!(matches!(outcome, Idempotent::AlreadyApplied));
    }

    #[test]
    fn commit_count_resets_after_rebase() {
        let mut cs = open_changeset();
        cs.record_commit("bbb...".to_string(), "edit", "a.md")
            .unwrap()
            .did_execute();
        cs.record_commit("ccc...".to_string(), "edit", "b.md")
            .unwrap()
            .did_execute();
        assert_eq!(cs.commit_count(), 2);

        cs.rebase("new-base...".to_string(), "new-base...".to_string())
            .unwrap()
            .did_execute();
        assert_eq!(cs.commit_count(), 0, "rebase resets the op count");

        cs.record_commit("ddd...".to_string(), "edit", "c.md")
            .unwrap()
            .did_execute();
        assert_eq!(cs.commit_count(), 1);
    }

    #[test]
    fn has_commits_survives_a_clean_rebase() {
        let mut cs = open_changeset();
        cs.record_commit("bbb...".to_string(), "edit", "a.md")
            .unwrap()
            .did_execute();
        assert!(cs.has_commits());

        cs.rebase("new-main...".to_string(), "squashed...".to_string())
            .unwrap()
            .did_execute();
        assert_eq!(cs.commit_count(), 0, "rebase resets the op count");
        assert!(
            cs.has_commits(),
            "base and head still differ after a rebase that squashed real content"
        );
    }

    #[test]
    fn rebase_rejected_when_not_open() {
        let mut cs = open_changeset();
        cs.discard(None).unwrap().did_execute();
        let outcome = cs.rebase("x".to_string(), "y".to_string());
        assert!(matches!(
            outcome,
            Err(ChangesetError::InvalidTransition { op: "rebase", .. })
        ));
    }

    #[test]
    fn submit_transitions_open_to_submitted() {
        let mut cs = open_changeset();
        cs.record_commit("bbb...".to_string(), "edit", "a.md")
            .unwrap()
            .did_execute();
        assert!(cs
            .submit("bbb...".into(), 42, "https://github.com/x/y/pull/42".into())
            .unwrap()
            .did_execute());
        assert_eq!(cs.status, ChangesetStatus::Submitted);
        assert_eq!(cs.pr_number, Some(42));
        assert_eq!(cs.pr_url.as_deref(), Some("https://github.com/x/y/pull/42"));
    }

    #[test]
    fn submit_is_idempotent_against_retry() {
        let mut cs = open_changeset();
        cs.submit("h".into(), 1, "u".into()).unwrap().did_execute();
        let outcome = cs.submit("h2".into(), 2, "u2".into()).unwrap();
        assert!(matches!(outcome, Idempotent::AlreadyApplied));
        // The first submit's values stick — a retry doesn't clobber them.
        assert_eq!(cs.pr_number, Some(1));
    }

    #[test]
    fn submit_rejected_when_not_open() {
        let mut cs = open_changeset();
        cs.discard(None).unwrap().did_execute();
        let outcome = cs.submit("h".into(), 1, "u".into());
        assert!(matches!(
            outcome,
            Err(ChangesetError::InvalidTransition { op: "submit", .. })
        ));
    }

    #[test]
    fn apply_transitions_open_to_applied() {
        let mut cs = open_changeset();
        assert!(cs
            .apply("merge-oid".into(), agent_actor())
            .unwrap()
            .did_execute());
        assert_eq!(cs.status, ChangesetStatus::Applied);
    }

    #[test]
    fn apply_transitions_submitted_to_applied() {
        let mut cs = open_changeset();
        cs.submit("h".into(), 1, "u".into()).unwrap().did_execute();
        assert!(cs
            .apply("merge-oid".into(), agent_actor())
            .unwrap()
            .did_execute());
        assert_eq!(cs.status, ChangesetStatus::Applied);
    }

    #[test]
    fn apply_rejected_from_terminal_state() {
        let mut cs = open_changeset();
        cs.discard(None).unwrap().did_execute();
        let outcome = cs.apply("merge-oid".into(), agent_actor());
        assert!(matches!(
            outcome,
            Err(ChangesetError::InvalidTransition { op: "apply", .. })
        ));
    }

    #[test]
    fn apply_is_idempotent() {
        let mut cs = open_changeset();
        cs.apply("merge-oid".into(), agent_actor())
            .unwrap()
            .did_execute();
        let outcome = cs.apply("other-oid".into(), agent_actor()).unwrap();
        assert!(matches!(outcome, Idempotent::AlreadyApplied));
    }

    #[test]
    fn mark_merged_from_submitted() {
        let mut cs = open_changeset();
        cs.submit("h".into(), 1, "u".into()).unwrap().did_execute();
        assert!(cs.mark_merged("merge-oid".into()).unwrap().did_execute());
        assert_eq!(cs.status, ChangesetStatus::Merged);
    }

    #[test]
    fn mark_merged_from_open_for_a_never_submitted_human_merge() {
        let mut cs = open_changeset();
        assert!(cs.mark_merged("merge-oid".into()).unwrap().did_execute());
        assert_eq!(cs.status, ChangesetStatus::Merged);
    }

    #[test]
    fn mark_merged_rejected_from_terminal_state() {
        let mut cs = open_changeset();
        cs.discard(None).unwrap().did_execute();
        let outcome = cs.mark_merged("merge-oid".into());
        assert!(matches!(
            outcome,
            Err(ChangesetError::InvalidTransition {
                op: "mark_merged",
                ..
            })
        ));
    }

    #[test]
    fn mark_abandoned_from_submitted() {
        let mut cs = open_changeset();
        cs.submit("h".into(), 1, "u".into()).unwrap().did_execute();
        assert!(cs.mark_abandoned().unwrap().did_execute());
        assert_eq!(cs.status, ChangesetStatus::Abandoned);
    }

    #[test]
    fn mark_abandoned_rejected_when_open() {
        let mut cs = open_changeset();
        let outcome = cs.mark_abandoned();
        assert!(matches!(
            outcome,
            Err(ChangesetError::InvalidTransition {
                from: ChangesetStatus::Open,
                op: "mark_abandoned"
            })
        ));
    }

    #[test]
    fn discard_from_open() {
        let mut cs = open_changeset();
        assert!(cs.discard(Some("stale".into())).unwrap().did_execute());
        assert_eq!(cs.status, ChangesetStatus::Discarded);
    }

    #[test]
    fn discard_from_submitted() {
        let mut cs = open_changeset();
        cs.submit("h".into(), 1, "u".into()).unwrap().did_execute();
        assert!(cs.discard(None).unwrap().did_execute());
        assert_eq!(cs.status, ChangesetStatus::Discarded);
    }

    #[test]
    fn discard_is_idempotent() {
        let mut cs = open_changeset();
        cs.discard(None).unwrap().did_execute();
        let outcome = cs.discard(Some("again".into())).unwrap();
        assert!(matches!(outcome, Idempotent::AlreadyApplied));
    }

    #[test]
    fn discard_rejected_from_terminal_state() {
        let mut cs = open_changeset();
        cs.mark_merged("merge-oid".into()).unwrap().did_execute();
        let outcome = cs.discard(None);
        assert!(matches!(
            outcome,
            Err(ChangesetError::InvalidTransition { op: "discard", .. })
        ));
    }

    #[test]
    fn rebase_hydrates_correctly() {
        let mut cs = open_changeset();
        cs.record_commit("bbb...".to_string(), "edit", "a.md")
            .unwrap()
            .did_execute();
        cs.rebase("new-base...".to_string(), "new-head...".to_string())
            .unwrap()
            .did_execute();

        let events = cs.events;
        let rehydrated = Changeset::try_from_events(events).unwrap();
        assert_eq!(rehydrated.base_oid, "new-base...");
        assert_eq!(rehydrated.head_oid, "new-head...");
        assert_eq!(rehydrated.status, ChangesetStatus::Open);
    }

    #[test]
    fn git_ref_and_branch_are_id_scoped() {
        let cs = open_changeset();
        assert_eq!(cs.branch(), format!("drua/{}", cs.id));
        assert_eq!(cs.git_ref(), format!("refs/heads/drua/{}", cs.id));
    }

    #[test]
    fn changeset_actor_id_accessors() {
        let agent_id = AgentId::new();
        let a = ChangesetActor::Agent { agent_id };
        assert_eq!(a.agent_id(), Some(agent_id));
        assert_eq!(a.workflow_run_id(), None);

        let run_id = WorkflowRunId::new();
        let w = ChangesetActor::WorkflowRun { run_id };
        assert_eq!(w.workflow_run_id(), Some(run_id));
        assert_eq!(w.agent_id(), None);

        let u = ChangesetActor::User {
            user_id: UserId::new(),
        };
        assert_eq!(u.agent_id(), None);
        assert_eq!(u.workflow_run_id(), None);
    }

    #[test]
    fn status_is_landed_and_terminal_classification() {
        assert!(ChangesetStatus::Merged.is_landed());
        assert!(ChangesetStatus::Applied.is_landed());
        assert!(!ChangesetStatus::Open.is_landed());
        assert!(!ChangesetStatus::Submitted.is_landed());

        assert!(ChangesetStatus::Discarded.is_terminal());
        assert!(ChangesetStatus::Abandoned.is_terminal());
        assert!(!ChangesetStatus::Open.is_terminal());
        assert!(!ChangesetStatus::Submitted.is_terminal());
    }

    #[test]
    fn changeset_actor_display_round_trips_through_from_str() {
        use std::str::FromStr;

        let agent = ChangesetActor::Agent {
            agent_id: AgentId::new(),
        };
        assert_eq!(ChangesetActor::from_str(&agent.to_string()).unwrap(), agent);

        let run = ChangesetActor::WorkflowRun {
            run_id: WorkflowRunId::new(),
        };
        assert_eq!(ChangesetActor::from_str(&run.to_string()).unwrap(), run);

        let user = ChangesetActor::User {
            user_id: UserId::new(),
        };
        assert_eq!(ChangesetActor::from_str(&user.to_string()).unwrap(), user);
    }

    #[test]
    fn changeset_actor_display_uses_the_documented_prefixes() {
        let agent_id = AgentId::new();
        assert_eq!(
            ChangesetActor::Agent { agent_id }.to_string(),
            format!("agent:{agent_id}")
        );
        let run_id = WorkflowRunId::new();
        assert_eq!(
            ChangesetActor::WorkflowRun { run_id }.to_string(),
            format!("run:{run_id}")
        );
        let user_id = UserId::new();
        assert_eq!(
            ChangesetActor::User { user_id }.to_string(),
            format!("user:{user_id}")
        );
    }

    #[test]
    fn changeset_actor_from_str_rejects_garbage() {
        use std::str::FromStr;

        assert!(ChangesetActor::from_str("nonsense").is_err());
        assert!(ChangesetActor::from_str("agent:not-a-uuid").is_err());
        assert!(ChangesetActor::from_str("robot:00000000-0000-0000-0000-000000000000").is_err());
    }
}
