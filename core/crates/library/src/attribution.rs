//! Commit attribution attached to every library/repo write.
//!
//! Plain serializable data so it survives the persisted-job round trip
//! (see [`crate::WriteOp`]). Rendered into a `git2::Signature` and a
//! trailer block by [`crate::git::GitEngine::commit_one`].
//!
//! Constructors live both here (library defaults: [`Self::library_default`],
//! [`Self::system`]) and in `drua-core::audit::Audit::commit_attribution`,
//! which builds a richer value from the request's `AuditContextData` plus a
//! JIT user lookup for the `Co-Authored-By:` line.

use es_entity::context::EventContext;
use serde::{Deserialize, Serialize};

const ATTRIBUTION_CONTEXT_KEY: &str = "commit_attribution";

/// Domain used for synthetic agent / bot identities. Real humans appear
/// in `Co-Authored-By:` via `<github_id>+<username>@users.noreply.github.com`.
pub const AGENT_DOMAIN: &str = "agent.galoy.io";

pub const LIBRARY_BOT_NAME: &str = "drua-library[bot]";
pub const LIBRARY_BOT_EMAIL: &str = "library@agent.galoy.io";
pub const SYSTEM_BOT_NAME: &str = "drua-system[bot]";
pub const SYSTEM_BOT_EMAIL: &str = "system@agent.galoy.io";
pub const ADMIN_BOT_NAME: &str = "drua-admin[bot]";
pub const ADMIN_BOT_EMAIL: &str = "admin@agent.galoy.io";
pub const AGENT_BOT_NAME: &str = "drua-agent[bot]";
pub const AGENT_BOT_EMAIL: &str = "agent@agent.galoy.io";
pub const WORKFLOW_BOT_NAME: &str = "drua-workflow[bot]";
pub const WORKFLOW_BOT_EMAIL: &str = "workflow@agent.galoy.io";

/// Stable subject-type tag rendered into the `Drua-Subject-Type:` trailer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommitSubjectKind {
    /// Importer canonical rewrites and other unattributed system writes.
    System,
    /// Library default — used as the fallback when no attribution was
    /// captured at the call site (e.g. tests, internal tooling).
    Library,
    /// Direct user mutation (e.g. admin via MCP).
    User,
    /// Bearer-token-driven mutation by a credential issued to a user.
    ExportedAgent,
    /// Autonomous agent (cron / webhook / manual workflow run).
    WorkflowAgent,
    /// Agent acting on behalf of a human user (chat session).
    AgentOnBehalfOfUser,
}

impl CommitSubjectKind {
    pub fn as_trailer_value(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Library => "library",
            Self::User => "user",
            Self::ExportedAgent => "exported_agent",
            Self::WorkflowAgent => "workflow_agent",
            Self::AgentOnBehalfOfUser => "agent_on_behalf_of_user",
        }
    }
}

/// Plain serializable attribution data attached to every library commit.
///
/// `trailers` are rendered alphabetically after any `Co-Authored-By:`
/// lines, separated from the user-provided commit message by a blank
/// line so trailers form a valid Git trailer block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitAttribution {
    pub author_name: String,
    pub author_email: String,
    pub committer_name: String,
    pub committer_email: String,
    pub kind: CommitSubjectKind,
    #[serde(default)]
    pub trailers: Vec<(String, String)>,
    /// `(display_name, email)` rendered as `Co-Authored-By:` lines.
    #[serde(default)]
    pub co_authored_by: Vec<(String, String)>,
}

impl Default for CommitAttribution {
    fn default() -> Self {
        Self::library_default()
    }
}

impl CommitAttribution {
    /// Pre-MVP fallback signature; matches `drua-library@local`.
    /// Used when no audit context is available (tests, bare library
    /// crate consumers, post_persist_hook with no captured context).
    pub fn library_default() -> Self {
        Self {
            author_name: LIBRARY_BOT_NAME.to_owned(),
            author_email: LIBRARY_BOT_EMAIL.to_owned(),
            committer_name: LIBRARY_BOT_NAME.to_owned(),
            committer_email: LIBRARY_BOT_EMAIL.to_owned(),
            kind: CommitSubjectKind::Library,
            trailers: vec![("Drua-Subject-Type".to_owned(), "library".to_owned())],
            co_authored_by: Vec::new(),
        }
    }

    /// Importer canonical-rewrite path and other unattributed system writes.
    pub fn system() -> Self {
        Self {
            author_name: SYSTEM_BOT_NAME.to_owned(),
            author_email: SYSTEM_BOT_EMAIL.to_owned(),
            committer_name: SYSTEM_BOT_NAME.to_owned(),
            committer_email: SYSTEM_BOT_EMAIL.to_owned(),
            kind: CommitSubjectKind::System,
            trailers: vec![("Drua-Subject-Type".to_owned(), "system".to_owned())],
            co_authored_by: Vec::new(),
        }
    }

    /// Append a `Drua-*` trailer (de-duplicates by exact key+value match).
    pub fn add_trailer(&mut self, key: impl Into<String>, value: impl Into<String>) {
        let entry = (key.into(), value.into());
        if !self.trailers.contains(&entry) {
            self.trailers.push(entry);
        }
    }

    pub fn add_co_authored_by(&mut self, name: impl Into<String>, email: impl Into<String>) {
        let entry = (name.into(), email.into());
        if !self.co_authored_by.contains(&entry) {
            self.co_authored_by.push(entry);
        }
    }

    /// Render the trailer block appended after the commit message body.
    /// Returns the empty string if there are no trailers or co-authors,
    /// otherwise a leading blank-line + key/value lines suitable for
    /// `git interpret-trailers --parse`.
    pub fn render_message_suffix(&self) -> String {
        if self.trailers.is_empty() && self.co_authored_by.is_empty() {
            return String::new();
        }

        let mut out = String::from("\n\n");

        for (name, email) in &self.co_authored_by {
            out.push_str("Co-Authored-By: ");
            out.push_str(name);
            out.push_str(" <");
            out.push_str(email);
            out.push_str(">\n");
        }

        let mut sorted = self.trailers.clone();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        for (key, value) in &sorted {
            out.push_str(key);
            out.push_str(": ");
            out.push_str(value);
            out.push('\n');
        }

        out.truncate(out.trim_end_matches('\n').len());
        out
    }

    /// Read the most-recently-stored value from the request's
    /// [`EventContext`]. Falls back to [`Self::library_default`] when
    /// no caller has populated it (tests, importer paths, etc.).
    pub fn from_event_context() -> Self {
        let ec = EventContext::current();
        ec.data()
            .lookup::<Self>(ATTRIBUTION_CONTEXT_KEY)
            .ok()
            .flatten()
            .unwrap_or_else(Self::library_default)
    }

    /// Stash this value in the request's [`EventContext`] so downstream
    /// post-persist hooks (e.g. `LibrarySynced` for Notes / Skills /
    /// Workflows) can pick it up without a `&Users` resolver in scope.
    pub fn put_in_event_context(&self) {
        let mut ec = EventContext::current();
        let _ = ec.insert(ATTRIBUTION_CONTEXT_KEY, self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_default_has_minimal_subject_type() {
        let a = CommitAttribution::library_default();
        assert_eq!(a.kind, CommitSubjectKind::Library);
        assert_eq!(
            a.trailers,
            vec![("Drua-Subject-Type".to_owned(), "library".to_owned())]
        );
        assert!(a.co_authored_by.is_empty());
    }

    #[test]
    fn system_uses_dedicated_bot_identity() {
        let a = CommitAttribution::system();
        assert_eq!(a.author_name, SYSTEM_BOT_NAME);
        assert_eq!(a.author_email, SYSTEM_BOT_EMAIL);
        assert_eq!(a.committer_name, SYSTEM_BOT_NAME);
        assert_eq!(a.committer_email, SYSTEM_BOT_EMAIL);
        assert_eq!(a.kind, CommitSubjectKind::System);
    }

    #[test]
    fn render_suffix_is_empty_when_no_trailers_or_co_authors() {
        let mut a = CommitAttribution::library_default();
        a.trailers.clear();
        assert_eq!(a.render_message_suffix(), "");
    }

    #[test]
    fn render_suffix_orders_co_authors_first_then_alpha_trailers() {
        let mut a = CommitAttribution::system();
        a.trailers.clear();
        a.add_trailer("Drua-Workflow-Run", "019df3a1");
        a.add_trailer("Drua-Subject-Type", "workflow_agent");
        a.add_trailer("Drua-Action", "space.write_file");
        a.add_co_authored_by("Alice", "12345+alice@users.noreply.github.com");

        let suffix = a.render_message_suffix();
        let expected = "\n\nCo-Authored-By: Alice <12345+alice@users.noreply.github.com>\n\
                       Drua-Action: space.write_file\n\
                       Drua-Subject-Type: workflow_agent\n\
                       Drua-Workflow-Run: 019df3a1";
        assert_eq!(suffix, expected);
    }

    #[test]
    fn add_trailer_dedupes_exact_matches() {
        let mut a = CommitAttribution::system();
        a.trailers.clear();
        a.add_trailer("Drua-Action", "space.write_file");
        a.add_trailer("Drua-Action", "space.write_file");
        a.add_trailer("Drua-Action", "space.delete_file");
        assert_eq!(a.trailers.len(), 2);
    }

    #[test]
    fn add_co_authored_by_dedupes_exact_matches() {
        let mut a = CommitAttribution::system();
        a.add_co_authored_by("Alice", "alice@example.com");
        a.add_co_authored_by("Alice", "alice@example.com");
        assert_eq!(a.co_authored_by.len(), 1);
    }

    #[test]
    fn render_human_on_behalf_of_workflow() {
        let mut a = CommitAttribution {
            author_name: "Alice (via drua)".to_owned(),
            author_email: "12345+alice@users.noreply.github.com".to_owned(),
            committer_name: AGENT_BOT_NAME.to_owned(),
            committer_email: AGENT_BOT_EMAIL.to_owned(),
            kind: CommitSubjectKind::AgentOnBehalfOfUser,
            trailers: Vec::new(),
            co_authored_by: Vec::new(),
        };
        a.add_trailer("Drua-Subject-Type", "agent_on_behalf_of_user");
        a.add_trailer("Drua-Acting-User", "5f1c8b3a");
        a.add_trailer("Drua-Project", "a1b2c3d4");
        a.add_trailer("Drua-Agent", "019df3aa");
        a.add_co_authored_by("Alice", "12345+alice@users.noreply.github.com");

        let suffix = a.render_message_suffix();
        assert!(
            suffix.starts_with("\n\nCo-Authored-By: Alice <12345+alice@users.noreply.github.com>")
        );
        assert!(suffix.contains("Drua-Acting-User: 5f1c8b3a"));
        assert!(suffix.contains("Drua-Subject-Type: agent_on_behalf_of_user"));
        let acting_pos = suffix.find("Drua-Acting-User").unwrap();
        let agent_pos = suffix.find("Drua-Agent").unwrap();
        let project_pos = suffix.find("Drua-Project").unwrap();
        let subject_pos = suffix.find("Drua-Subject-Type").unwrap();
        assert!(acting_pos < agent_pos);
        assert!(agent_pos < project_pos);
        assert!(project_pos < subject_pos);
    }

    #[test]
    fn subject_kind_serializes_snake_case() {
        let json = serde_json::to_string(&CommitSubjectKind::AgentOnBehalfOfUser).unwrap();
        assert_eq!(json, "\"agent_on_behalf_of_user\"");
    }

    #[test]
    fn round_trip_serde() {
        let mut a = CommitAttribution::system();
        a.add_trailer("Drua-Action", "space.write_file");
        a.add_co_authored_by("Bob", "67890+bob@users.noreply.github.com");

        let json = serde_json::to_string(&a).unwrap();
        let back: CommitAttribution = serde_json::from_str(&json).unwrap();
        assert_eq!(a, back);
    }
}
