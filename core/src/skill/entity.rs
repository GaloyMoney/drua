use derive_builder::Builder;
use drua_library::{GitFileHash, SearchableFields, WriteOp};
use serde::{Deserialize, Serialize};

use es_entity::*;

use crate::primitives::*;
use crate::skill::file::render_skill_markdown;
use crate::skill::SKILL_DOC_TYPE;

#[derive(EsEvent, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[es_event(id = "SkillId")]
pub enum SkillEvent {
    Initialized {
        id: SkillId,
        project_id: Option<ProjectId>,
        project_name: Option<String>,
        /// `Some(s)` for space-scoped skills; mutually exclusive with
        /// `project_id` (enforced by the `skills_owner_at_most_one`
        /// CHECK constraint).
        #[serde(default)]
        space_id: Option<SpaceId>,
        /// Denormalised space slug — mirrors the `project_name` denorm.
        #[serde(default)]
        space_slug: Option<String>,
        name: String,
        description: String,
        body: String,
        /// Repo-relative on-disk path. Sacred — never mutated by the
        /// importer.
        path: String,
    },
    Updated {
        name: Option<String>,
        description: Option<String>,
        body: Option<String>,
    },
    /// Records a file move detected by reverse-sync — the importer
    /// re-imports a previously-known frontmatter id at a new on-disk
    /// path. Distinct from `Updated` because file content didn't
    /// change, only its location; not a content event so no library
    /// write-back is fired (the move happened in git already).
    PathChanged { path: String },
}

#[derive(EsEntity, Builder)]
#[builder(pattern = "owned", build_fn(error = "EntityHydrationError"))]
pub struct Skill {
    pub id: SkillId,
    #[builder(default)]
    pub project_id: Option<ProjectId>,
    #[builder(default)]
    pub project_name: Option<String>,
    /// `Some(s)` when this skill belongs to a space rather than a project.
    /// Mutually exclusive with `project_id` (DB CHECK constraint).
    #[builder(default)]
    pub space_id: Option<SpaceId>,
    /// Denormalised space slug — `Some` exactly when `space_id` is `Some`.
    #[builder(default)]
    pub space_slug: Option<String>,
    pub name: String,
    pub description: String,
    pub body: String,
    /// Repo-relative on-disk path. The importer never mutates this —
    /// whatever the user wrote is the path of record.
    pub path: String,
    events: EntityEvents<SkillEvent>,
}

impl Skill {
    pub fn created_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.events
            .entity_first_persisted_at()
            .expect("entity_first_persisted_at not found")
    }

    /// Canonical on-disk form (markdown w/ frontmatter).
    pub(crate) fn rendered(&self) -> String {
        render_skill_markdown(
            self.id.into(),
            &self.name,
            &self.description,
            &self.body,
            &self.created_at().to_rfc3339(),
            &self.updated_at_rfc3339(),
        )
    }

    fn updated_at_rfc3339(&self) -> String {
        self.events
            .entity_last_modified_at()
            .or_else(|| self.events.entity_first_persisted_at())
            .unwrap_or_else(chrono::Utc::now)
            .to_rfc3339()
    }

    /// Hash of the canonical runtime form, used for reverse-sync
    /// idempotency comparisons.
    pub(crate) fn file_hash(&self) -> GitFileHash {
        GitFileHash::new(self.rendered())
    }

    pub fn update(
        &mut self,
        name: Option<String>,
        description: Option<String>,
        body: Option<String>,
        incoming_file_hash: GitFileHash,
    ) -> Idempotent<()> {
        if self.file_hash() == incoming_file_hash {
            return Idempotent::AlreadyApplied;
        }
        if let Some(ref n) = name {
            self.name = n.clone();
        }
        if let Some(ref d) = description {
            self.description = d.clone();
        }
        if let Some(ref b) = body {
            self.body = b.clone();
        }
        self.events.push(SkillEvent::Updated {
            name,
            description,
            body,
        });
        Idempotent::Executed(())
    }

    /// Records a path change on the entity. Idempotent — same path is
    /// a no-op. Only invoked by reverse-sync when an existing skill is
    /// re-imported at a new on-disk location (e.g. user moved the
    /// markdown via `spaces edit op=move`). Pure metadata: doesn't
    /// affect rendered content or fire a library write-back.
    pub fn change_path(&mut self, new_path: String) -> Idempotent<()> {
        if self.path == new_path {
            return Idempotent::AlreadyApplied;
        }
        self.path = new_path.clone();
        self.events.push(SkillEvent::PathChanged { path: new_path });
        Idempotent::Executed(())
    }

    /// User-driven path (no file_hash compare; that's [`Self::update`]).
    pub fn update_content(
        &mut self,
        name: Option<String>,
        description: Option<String>,
        body: Option<String>,
    ) -> Idempotent<()> {
        let name_changes = name.as_ref().is_some_and(|n| n != &self.name);
        let desc_changes = description.as_ref().is_some_and(|d| d != &self.description);
        let body_changes = body.as_ref().is_some_and(|b| b != &self.body);
        if !name_changes && !desc_changes && !body_changes {
            return Idempotent::AlreadyApplied;
        }
        if name_changes {
            self.name = name.clone().unwrap();
        }
        if desc_changes {
            self.description = description.clone().unwrap();
        }
        if body_changes {
            self.body = body.clone().unwrap();
        }
        self.events.push(SkillEvent::Updated {
            name: name_changes.then(|| self.name.clone()),
            description: desc_changes.then(|| self.description.clone()),
            body: body_changes.then(|| self.body.clone()),
        });
        Idempotent::Executed(())
    }
}

impl core::fmt::Display for Skill {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Skill: {}, name: {}", self.id, self.name)
    }
}

impl drua_library::LibrarySynced for Skill {
    type Event = SkillEvent;

    fn is_content_event(ev: &SkillEvent) -> bool {
        // `PathChanged` is metadata-only — the file move happened in
        // git already; no write-back to fire.
        matches!(
            ev,
            SkillEvent::Initialized { .. } | SkillEvent::Updated { .. }
        )
    }

    fn searchable_fields(&self) -> SearchableFields {
        // Space-scoped skills surface under their space; project- and
        // global-scoped under the project (or unscoped).
        let (scope_id, scope_slug) = match (self.space_id, self.project_id) {
            (Some(s), _) => (Some(uuid::Uuid::from(s)), self.space_slug.clone()),
            (None, Some(p)) => (Some(uuid::Uuid::from(p)), self.project_name.clone()),
            (None, None) => (None, None),
        };
        SearchableFields {
            doc_id: self.id.into(),
            doc_type: SKILL_DOC_TYPE,
            scope_id,
            scope_slug,
            name: self.name.clone(),
            path: Some(self.path.clone()),
            content: self.description.clone(),
        }
    }

    fn write_op(&self) -> WriteOp {
        let content = self.rendered().into_bytes();
        let id_uuid: uuid::Uuid = self.id.into();
        let message = format!("skill: {}-{}", &self.name, &id_uuid.to_string()[..8]);
        WriteOp::WriteFile {
            path: self.path.clone(),
            content,
            message,
        }
    }
}

pub struct SkillBody(String);

impl SkillBody {
    pub(crate) fn new(body: String) -> Self {
        Self(body)
    }

    /// `$ARGUMENTS` → full argument string; `$0`, `$1`, … → positional
    /// (shell-split, quotes respected). If neither placeholder matches and
    /// args are non-empty, appends `ARGUMENTS: <value>`.
    pub fn interpolate(self, arguments: Option<&str>) -> String {
        let args = arguments.unwrap_or_default();
        let positional = shell_split(args);

        let mut result = self.0;
        let mut had_substitution = false;

        if result.contains("$ARGUMENTS") {
            result = result.replace("$ARGUMENTS", args);
            had_substitution = true;
        }

        // Highest index first so $10 isn't eaten by $1.
        for i in (0..positional.len()).rev() {
            let placeholder = format!("${i}");
            if result.contains(&placeholder) {
                result = result.replace(&placeholder, &positional[i]);
                had_substitution = true;
            }
        }

        if !had_substitution && !args.is_empty() {
            format!("{result}\n\nARGUMENTS: {args}")
        } else {
            result
        }
    }
}

impl From<SkillBody> for String {
    fn from(sb: SkillBody) -> Self {
        sb.0
    }
}

/// Double quotes: preserved literally, backslash escapes next char.
/// Single quotes: preserved literally, no escaping.
/// Unquoted whitespace delimits arguments.
fn shell_split(input: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '"' => loop {
                match chars.next() {
                    Some('\\') => {
                        if let Some(next) = chars.next() {
                            current.push(next);
                        }
                    }
                    Some('"') | None => break,
                    Some(c) => current.push(c),
                }
            },
            '\'' => loop {
                match chars.next() {
                    Some('\'') | None => break,
                    Some(c) => current.push(c),
                }
            },
            c if c.is_whitespace() => {
                if !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }

    if !current.is_empty() {
        args.push(current);
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_split_simple() {
        assert_eq!(shell_split("staging prod"), vec!["staging", "prod"]);
    }

    #[test]
    fn shell_split_double_quotes() {
        assert_eq!(
            shell_split(r#""hello world" foo"#),
            vec!["hello world", "foo"]
        );
    }

    #[test]
    fn shell_split_single_quotes() {
        assert_eq!(shell_split("'hello world' foo"), vec!["hello world", "foo"]);
    }

    #[test]
    fn shell_split_escaped_quote() {
        assert_eq!(
            shell_split(r#""say \"hi\"" bar"#),
            vec![r#"say "hi""#, "bar"]
        );
    }

    #[test]
    fn shell_split_empty() {
        assert!(shell_split("").is_empty());
        assert!(shell_split("   ").is_empty());
    }

    #[test]
    fn shell_split_mixed_quotes() {
        assert_eq!(
            shell_split(r#"plain "double quoted" 'single quoted'"#),
            vec!["plain", "double quoted", "single quoted"]
        );
    }

    #[test]
    fn interpolate_replaces_arguments_placeholder() {
        let body = SkillBody::new("Deploy $ARGUMENTS to production.".into());
        assert_eq!(
            body.interpolate(Some("staging")),
            "Deploy staging to production."
        );
    }

    #[test]
    fn interpolate_appends_when_no_placeholder() {
        let body = SkillBody::new("Run the deploy process.".into());
        assert_eq!(
            body.interpolate(Some("staging")),
            "Run the deploy process.\n\nARGUMENTS: staging"
        );
    }

    #[test]
    fn interpolate_noop_when_no_args_and_no_placeholder() {
        let body = SkillBody::new("Run the deploy process.".into());
        assert_eq!(body.interpolate(None), "Run the deploy process.");
    }

    #[test]
    fn interpolate_replaces_multiple_arguments_occurrences() {
        let body = SkillBody::new("First: $ARGUMENTS, second: $ARGUMENTS".into());
        assert_eq!(body.interpolate(Some("val")), "First: val, second: val");
    }

    #[test]
    fn interpolate_positional_params() {
        let body = SkillBody::new("Deploy $0 to $1.".into());
        assert_eq!(
            body.interpolate(Some("myapp production")),
            "Deploy myapp to production."
        );
    }

    #[test]
    fn interpolate_positional_with_quotes() {
        let body = SkillBody::new("Say $0 to $1.".into());
        assert_eq!(
            body.interpolate(Some(r#""hello world" bob"#)),
            "Say hello world to bob."
        );
    }

    #[test]
    fn interpolate_mixed_arguments_and_positional() {
        let body = SkillBody::new("All: $ARGUMENTS, first: $0, second: $1.".into());
        assert_eq!(
            body.interpolate(Some("foo bar")),
            "All: foo bar, first: foo, second: bar."
        );
    }

    #[test]
    fn interpolate_unmatched_positional_left_as_is() {
        let body = SkillBody::new("$0 and $1 and $2.".into());
        assert_eq!(
            body.interpolate(Some("only-one")),
            "only-one and $1 and $2."
        );
    }

    #[test]
    fn interpolate_high_positional_index() {
        let args = "a b c d e f g h i j k";
        let result = SkillBody::new("$0 $1 $10".into()).interpolate(Some(args));
        assert_eq!(result, "a b k");
    }
}

impl TryFromEvents<SkillEvent> for Skill {
    fn try_from_events(events: EntityEvents<SkillEvent>) -> Result<Self, EntityHydrationError> {
        let mut builder = SkillBuilder::default();

        for event in events.iter_all() {
            match event {
                SkillEvent::Initialized {
                    id,
                    project_id,
                    project_name,
                    space_id,
                    space_slug,
                    name,
                    description,
                    body,
                    path,
                } => {
                    builder = builder
                        .id(*id)
                        .project_id(*project_id)
                        .project_name(project_name.clone())
                        .space_id(*space_id)
                        .space_slug(space_slug.clone())
                        .name(name.clone())
                        .description(description.clone())
                        .body(body.clone())
                        .path(path.clone());
                }

                SkillEvent::Updated {
                    name,
                    description,
                    body,
                    ..
                } => {
                    if let Some(name) = name {
                        builder = builder.name(name.clone());
                    }
                    if let Some(description) = description {
                        builder = builder.description(description.clone());
                    }
                    if let Some(body) = body {
                        builder = builder.body(body.clone());
                    }
                }
                SkillEvent::PathChanged { path } => {
                    builder = builder.path(path.clone());
                }
            }
        }

        builder.events(events).build()
    }
}

#[derive(Debug, Builder)]
#[builder(pattern = "owned", build_fn(name = "build_inner"))]
pub struct NewSkill {
    #[builder(setter(into))]
    pub(super) id: SkillId,
    #[builder(default, setter(into, strip_option))]
    pub(super) project_id: Option<ProjectId>,
    #[builder(default, setter(into, strip_option))]
    pub(super) project_name: Option<String>,
    #[builder(default, setter(into, strip_option))]
    pub(super) space_id: Option<SpaceId>,
    #[builder(default, setter(into, strip_option))]
    pub(super) space_slug: Option<String>,
    #[builder(setter(into))]
    pub(super) name: String,
    #[builder(setter(into))]
    pub(super) description: String,
    #[builder(setter(into))]
    pub(super) body: String,
    /// Repo-relative on-disk path. Filled lazily on `build()` via
    /// [`crate::skill::file::default_skill_path`] when the caller didn't
    /// set one explicitly — the reverse-sync importer always sets it
    /// (`parsed.path`); DB-driven create surfaces (`Skills::create` /
    /// future `create_in_space`) skip it and let the builder derive
    /// from `(name, project_name, space_slug)`.
    #[builder(default, setter(into))]
    pub(super) path: String,
}

impl NewSkill {
    pub fn builder() -> NewSkillBuilder {
        NewSkillBuilder::default().id(SkillId::new())
    }
}

impl NewSkillBuilder {
    /// Lazy default for `path`: derived from `(name, project_name,
    /// space_slug)` when the caller didn't set it explicitly. Wraps
    /// the auto-generated `build_inner` so callers see the same
    /// `build()` API regardless of whether they passed a path.
    pub fn build(self) -> Result<NewSkill, NewSkillBuilderError> {
        let mut me = self;
        if me.path.as_deref().map(str::is_empty).unwrap_or(true) {
            // Builder fields are `Option<T>`; for `Option<String>`
            // setters they're `Option<Option<String>>`. Flatten before
            // borrowing.
            let name = me.name.as_deref().unwrap_or("");
            let project_name = me.project_name.as_ref().and_then(|p| p.as_deref());
            let space_slug = me.space_slug.as_ref().and_then(|s| s.as_deref());
            me.path = Some(crate::skill::file::default_skill_path(
                name,
                project_name,
                space_slug,
            ));
        }
        me.build_inner()
    }
}

impl IntoEvents<SkillEvent> for NewSkill {
    fn into_events(self) -> EntityEvents<SkillEvent> {
        EntityEvents::init(
            self.id,
            [SkillEvent::Initialized {
                id: self.id,
                project_id: self.project_id,
                project_name: self.project_name.clone(),
                space_id: self.space_id,
                space_slug: self.space_slug,
                name: self.name,
                description: self.description,
                body: self.body,
                path: self.path,
            }],
        )
    }
}
