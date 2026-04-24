use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use es_entity::*;

use crate::primitives::*;

#[derive(EsEvent, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[es_event(id = "SkillId")]
pub enum SkillEvent {
    Initialized {
        id: SkillId,
        workspace_id: Option<WorkspaceId>,
        workspace_name: Option<String>,
        name: String,
        description: String,
        body: String,
    },
    Updated {
        name: Option<String>,
        description: Option<String>,
        body: Option<String>,
    },
}

#[derive(EsEntity, Builder)]
#[builder(pattern = "owned", build_fn(error = "EntityHydrationError"))]
pub struct Skill {
    pub id: SkillId,
    #[builder(default)]
    pub workspace_id: Option<WorkspaceId>,
    #[builder(default)]
    pub workspace_name: Option<String>,
    pub name: String,
    pub description: String,
    pub body: String,
    events: EntityEvents<SkillEvent>,
}

impl Skill {
    pub fn created_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.events
            .entity_first_persisted_at()
            .expect("entity_first_persisted_at not found")
    }

    pub(crate) fn as_runtime_file(&self) -> crate::library::RuntimeFile {
        let created_at = self
            .events
            .entity_first_persisted_at()
            .map(|t| t.to_rfc3339())
            .unwrap_or_default();
        let updated_at = self
            .events
            .entity_last_modified_at()
            .map(|t| t.to_rfc3339())
            .unwrap_or_default();

        crate::library::RuntimeFile::for_skill(
            self.id,
            self.workspace_id,
            self.workspace_name.as_deref(),
            &self.name,
            &self.description,
            &self.body,
            &created_at,
            &updated_at,
        )
    }

    pub fn update(
        &mut self,
        name: Option<String>,
        description: Option<String>,
        body: Option<String>,
    ) -> Idempotent<()> {
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
}

impl core::fmt::Display for Skill {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Skill: {}, name: {}", self.id, self.name)
    }
}

/// A resolved skill body ready for argument interpolation.
pub struct SkillBody(String);

impl SkillBody {
    pub(crate) fn new(body: String) -> Self {
        Self(body)
    }

    /// Substitute placeholders in the skill body with the provided arguments.
    ///
    /// - `$ARGUMENTS` — replaced with the full argument string.
    /// - `$0`, `$1`, `$2`, … — replaced with positional arguments parsed
    ///   via shell-style splitting (double/single quotes respected).
    ///
    /// If the body contains neither `$ARGUMENTS` nor any matching `$N`
    /// placeholder and arguments are non-empty, appends
    /// `ARGUMENTS: <value>` to the end.
    pub fn interpolate(self, arguments: Option<&str>) -> String {
        let args = arguments.unwrap_or_default();
        let positional = shell_split(args);

        let mut result = self.0;
        let mut had_substitution = false;

        if result.contains("$ARGUMENTS") {
            result = result.replace("$ARGUMENTS", args);
            had_substitution = true;
        }

        // Replace $N from highest index down so $10 isn't eaten by $1.
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

/// Shell-style argument splitting with double and single quote support.
///
/// - Double quotes: content preserved literally, backslash escapes the
///   next character (`\"` → `"`).
/// - Single quotes: content preserved literally, no escaping.
/// - Unquoted whitespace delimits arguments.
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

    // -- shell_split ---------------------------------------------------------

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

    // -- SkillBody::interpolate ----------------------------------------------

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
                    workspace_id,
                    workspace_name,
                    name,
                    description,
                    body,
                } => {
                    builder = builder
                        .id(*id)
                        .workspace_id(*workspace_id)
                        .workspace_name(workspace_name.clone())
                        .name(name.clone())
                        .description(description.clone())
                        .body(body.clone());
                }

                SkillEvent::Updated {
                    name,
                    description,
                    body,
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
            }
        }

        builder.events(events).build()
    }
}

#[derive(Debug, Builder)]
#[builder(pattern = "owned")]
pub struct NewSkill {
    #[builder(setter(into))]
    pub(super) id: SkillId,
    #[builder(default, setter(into, strip_option))]
    pub(super) workspace_id: Option<WorkspaceId>,
    #[builder(default, setter(into, strip_option))]
    pub(super) workspace_name: Option<String>,
    #[builder(setter(into))]
    pub(super) name: String,
    #[builder(setter(into))]
    pub(super) description: String,
    #[builder(setter(into))]
    pub(super) body: String,
}

impl NewSkill {
    pub fn builder() -> NewSkillBuilder {
        NewSkillBuilder::default().id(SkillId::new())
    }
}

impl IntoEvents<SkillEvent> for NewSkill {
    fn into_events(self) -> EntityEvents<SkillEvent> {
        EntityEvents::init(
            self.id,
            [SkillEvent::Initialized {
                id: self.id,
                workspace_id: self.workspace_id,
                workspace_name: self.workspace_name.clone(),
                name: self.name,
                description: self.description,
                body: self.body,
            }],
        )
    }
}
