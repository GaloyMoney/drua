use drua_library::GitFileHash;

use crate::primitives::SkillId;

/// Renders a skill as markdown with frontmatter — the canonical
/// on-disk form. Identical bytes round-trip via the library's git
/// hash short-circuit.
///
/// Frontmatter intentionally omits `name:` — the filename is the
/// canonical name (matches Claude Code, Hugo, GitHub Actions, etc.)
/// and writing it twice creates two sources of truth that drift on
/// rename. `_name` is taken to match the public signature for now.
pub fn render_skill_markdown(
    doc_id: uuid::Uuid,
    _name: &str,
    description: &str,
    body: &str,
    created_at: &str,
    updated_at: &str,
) -> String {
    format!(
        "---\nid: {}\ndescription: \"{}\"\ncreated: {}\nupdated: {}\n---\n\n{}\n",
        doc_id,
        description.replace('"', "\\\""),
        created_at,
        updated_at,
        body
    )
}

/// Parsed skill from on-disk content. `needs_rewrite` signals the
/// importer should re-render the file (e.g. to inject an `id:`
/// frontmatter that wasn't there before) — but always at the same
/// `path`; never as a rename. `project_name` and `space_slug` are
/// mutually exclusive — the path determines which (if any) is set.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ParsedSkill {
    pub skill_id: SkillId,
    pub project_name: Option<String>,
    pub space_slug: Option<String>,
    pub name: String,
    pub description: String,
    pub body: String,
    pub created_at: String,
    pub updated_at: String,
    pub path: String,
    pub needs_rewrite: bool,
}

impl ParsedSkill {
    /// Canonical on-disk form for the parsed skill — must match
    /// [`crate::skill::Skill::rendered`] byte-for-byte so reverse-sync's
    /// `GitFileHash` compare against an existing entity short-circuits.
    pub fn render(&self) -> String {
        render_skill_markdown(
            self.skill_id.into(),
            &self.name,
            &self.description,
            &self.body,
            &self.created_at,
            &self.updated_at,
        )
    }

    pub fn file_hash(&self) -> GitFileHash {
        GitFileHash::new(self.render())
    }
}

/// Handles three formats:
/// 1. Full frontmatter (canonical) — `needs_rewrite = false`.
/// 2. Frontmatter without `id:` — generates a new `SkillId`, `needs_rewrite = true`.
/// 3. No frontmatter (human-authored) — generates a new `SkillId`, `needs_rewrite = true`.
///
/// Returns `None` only if the content has no recognisable form.
pub fn parse_skill_markdown(content: &str, path: &str) -> Option<ParsedSkill> {
    let project_name = project_name_from_skill_path(path);
    let space_slug = space_slug_from_skill_path(path);
    let content = content.trim();
    if content.is_empty() {
        return None;
    }

    let mut parsed = if content.starts_with("---") {
        parse_skill_with_frontmatter(content, project_name, space_slug, path)?
    } else {
        parse_skill_without_frontmatter(content, project_name, space_slug, path)?
    };

    parsed.path = path.to_string();
    Some(parsed)
}

#[derive(serde::Deserialize, Default)]
struct SkillFrontmatter {
    #[serde(default)]
    id: Option<uuid::Uuid>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    created: Option<String>,
    #[serde(default)]
    updated: Option<String>,
}

/// Name = path slug (e.g. `data-validator.md` → `data-validator`,
/// `data-validator/SKILL.md` → `data-validator`; see
/// [`skill_name_from_path`]). Always — the skill's invocation handle
/// is the slug, not the heading or frontmatter `name:`. Description
/// falls back through:
/// 1. Frontmatter `description:`
/// 2. `# Heading` text (when no description in frontmatter)
/// 3. Empty
fn parse_skill_with_frontmatter(
    content: &str,
    project_name: Option<String>,
    space_slug: Option<String>,
    path: &str,
) -> Option<ParsedSkill> {
    let rest = content.strip_prefix("---")?;
    let (frontmatter_str, after_fm) = rest.split_once("\n---")?;

    let fm: SkillFrontmatter = serde_yaml::from_str(frontmatter_str.trim()).unwrap_or_default();

    let (skill_id, has_id) = match fm.id {
        Some(uuid) => (SkillId::from(uuid), true),
        None => (SkillId::new(), false),
    };

    let name = skill_name_from_path(path)?;
    let body = after_fm.trim().to_string();
    let description = fm
        .description
        .or_else(|| heading_text(&body))
        .unwrap_or_default();

    let created_at = fm.created.unwrap_or_default();
    let updated_at = fm.updated.unwrap_or_default();

    let needs_rewrite = !has_id;

    Some(ParsedSkill {
        skill_id,
        project_name,
        space_slug,
        name,
        description,
        body,
        created_at,
        updated_at,
        path: String::new(),
        needs_rewrite,
    })
}

fn parse_skill_without_frontmatter(
    content: &str,
    project_name: Option<String>,
    space_slug: Option<String>,
    path: &str,
) -> Option<ParsedSkill> {
    let name = skill_name_from_path(path)?;
    let body = content.trim().to_string();
    let description = heading_text(&body).unwrap_or_default();

    Some(ParsedSkill {
        skill_id: SkillId::new(),
        project_name,
        space_slug,
        name,
        description,
        body,
        created_at: String::new(),
        updated_at: String::new(),
        path: String::new(),
        needs_rewrite: true,
    })
}

/// First `# heading` line text, if present at the start of the body.
fn heading_text(body: &str) -> Option<String> {
    let line = body.lines().next()?;
    line.strip_prefix("# ").map(|s| s.trim().to_string())
}

/// `runtime/projects/{project}/skills/*.md` → `Some(project)`;
/// other paths (including `runtime/spaces/*` and `runtime/skills/*`) → `None`.
pub fn project_name_from_skill_path(relative_path: &str) -> Option<String> {
    let parts: Vec<&str> = relative_path.split('/').collect();
    if parts.len() >= 5 && parts[0] == "runtime" && parts[1] == "projects" && parts[3] == "skills" {
        Some(parts[2].to_string())
    } else {
        None
    }
}

/// `spaces/{slug}/skills/*.md` → `Some(slug)`; other paths → `None`.
/// Spaces use the flat `spaces/<slug>/...` layout (no `runtime/` prefix);
/// see `library/src/space/mod.rs::write_file` for the canonical layout.
pub fn space_slug_from_skill_path(relative_path: &str) -> Option<String> {
    let parts: Vec<&str> = relative_path.split('/').collect();
    if parts.len() >= 4 && parts[0] == "spaces" && parts[2] == "skills" {
        Some(parts[1].to_string())
    } else {
        None
    }
}

/// Anthropic Agent Skills layout: `.../skills/<name>/SKILL.md` (same
/// convention the sandbox scans from `.claude/skills/`).
pub const SKILL_DIR_FILE: &str = "SKILL.md";

pub fn is_skill_dir_file(path: &str) -> bool {
    path.rsplit('/').next() == Some(SKILL_DIR_FILE)
}

/// Skill name for either on-disk layout:
/// - flat, `.../skills/<name>.md` → slug of the file stem;
/// - directory, `.../skills/<name>/SKILL.md` → slug of the parent
///   directory, so sibling `references/` and `scripts/` can live
///   beside the skill without changing its invocation handle.
///
/// Examples:
/// - `spaces/mkt/skills/write-blog-post.md` → `Some("write-blog-post")`
/// - `spaces/mkt/skills/write-blog-post/SKILL.md` → `Some("write-blog-post")`
/// - `spaces/mkt/skills/SKILL.md` → `None` (no directory to name it)
pub fn skill_name_from_path(path: &str) -> Option<String> {
    if !is_skill_dir_file(path) {
        return name_from_filename(path);
    }
    let mut parts = path.rsplit('/');
    parts.next()?; // SKILL.md
    let dir = parts.next()?;
    if dir == "skills" {
        return None;
    }
    let slug = slugify(dir);
    if slug.is_empty() {
        None
    } else {
        Some(slug)
    }
}

/// Derive a kebab-case skill name from a file path. Used by the
/// importer when the file has no `name:` frontmatter — the filename
/// stem is the source of truth.
///
/// Examples:
/// - `runtime/skills/deploy.md` → `Some("deploy")`
/// - `spaces/team/skills/Hello World.md` → `Some("hello-world")`
/// - `spaces/team/skills/.md` → `None`
pub fn name_from_filename(path: &str) -> Option<String> {
    let filename = path.rsplit('/').next()?;
    let stem = filename
        .strip_suffix(".md")
        .or_else(|| filename.strip_suffix(".yml"))
        .unwrap_or(filename);
    if stem.is_empty() {
        return None;
    }
    let slug = slugify(stem);
    if slug.is_empty() {
        None
    } else {
        Some(slug)
    }
}

/// title/name → kebab-case slug for filename construction.
pub fn slugify(title: &str) -> String {
    title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// Default repo-relative path for a skill created through the
/// DB-driven service surface (`Skills::create` / `create_in_space`).
/// The importer does NOT use this — paths it sees are whatever the
/// author wrote.
pub fn default_skill_path(
    name: &str,
    project_name: Option<&str>,
    space_slug: Option<&str>,
) -> String {
    let slug = slugify(name);
    if let Some(space) = space_slug {
        format!("spaces/{space}/skills/{slug}.md")
    } else if let Some(project) = project_name {
        format!("runtime/projects/{project}/skills/{slug}.md")
    } else {
        format!("runtime/skills/{slug}.md")
    }
}

#[cfg(test)]
mod path_tests {
    use super::*;

    #[test]
    fn project_name_extraction() {
        assert_eq!(
            project_name_from_skill_path("runtime/projects/alpha/skills/foo.md"),
            Some("alpha".to_string())
        );
        assert_eq!(
            project_name_from_skill_path("runtime/spaces/team/skills/foo.md"),
            None,
            "space-rooted path must not be parsed as project"
        );
        assert_eq!(project_name_from_skill_path("runtime/skills/foo.md"), None);
    }

    #[test]
    fn space_slug_extraction() {
        assert_eq!(
            space_slug_from_skill_path("spaces/team/skills/foo.md"),
            Some("team".to_string())
        );
        assert_eq!(
            space_slug_from_skill_path("runtime/projects/alpha/skills/foo.md"),
            None,
            "project-rooted path must not be parsed as space"
        );
        assert_eq!(space_slug_from_skill_path("runtime/skills/foo.md"), None);
        assert_eq!(
            space_slug_from_skill_path("spaces/team/notes/foo.md"),
            None,
            "non-skill subtree must not match"
        );
    }

    #[test]
    fn skill_name_flat_layout_uses_file_stem() {
        assert_eq!(
            skill_name_from_path("spaces/mkt/skills/write-blog-post.md"),
            Some("write-blog-post".to_string())
        );
        assert_eq!(
            skill_name_from_path("runtime/skills/Deploy Prod.md"),
            Some("deploy-prod".to_string())
        );
    }

    #[test]
    fn skill_name_dir_layout_uses_parent_dir() {
        assert_eq!(
            skill_name_from_path("spaces/mkt/skills/write-blog-post/SKILL.md"),
            Some("write-blog-post".to_string())
        );
        assert_eq!(
            skill_name_from_path("runtime/projects/alpha/skills/Daily Digest/SKILL.md"),
            Some("daily-digest".to_string())
        );
        assert_eq!(
            skill_name_from_path("runtime/skills/deploy/SKILL.md"),
            Some("deploy".to_string())
        );
    }

    #[test]
    fn skill_name_dir_layout_needs_a_directory() {
        assert_eq!(
            skill_name_from_path("spaces/mkt/skills/SKILL.md"),
            None,
            "SKILL.md directly under skills/ has no directory to name it"
        );
        assert_eq!(skill_name_from_path("SKILL.md"), None);
    }

    #[test]
    fn dir_layout_parses_name_from_dir_not_file() {
        let content = "---\nname: ignored-name\ndescription: \"Writes a post\"\n---\n\n# Write blog post\n\nBody.";
        let parsed = parse_skill_markdown(content, "spaces/mkt/skills/write-blog-post/SKILL.md")
            .expect("parses");
        assert_eq!(parsed.name, "write-blog-post");
        assert_eq!(parsed.description, "Writes a post");
        assert_eq!(parsed.space_slug.as_deref(), Some("mkt"));
        assert_eq!(parsed.project_name, None);
        assert_eq!(parsed.path, "spaces/mkt/skills/write-blog-post/SKILL.md");
        assert!(
            parsed.needs_rewrite,
            "no id: in frontmatter → rewrite at same path"
        );
    }

    #[test]
    fn dir_layout_scopes_project_and_global() {
        let parsed = parse_skill_markdown(
            "# Deploy\n\nSteps.",
            "runtime/projects/alpha/skills/deploy/SKILL.md",
        )
        .expect("parses");
        assert_eq!(parsed.name, "deploy");
        assert_eq!(parsed.project_name.as_deref(), Some("alpha"));
        assert_eq!(parsed.space_slug, None);

        let parsed = parse_skill_markdown("# Deploy\n\nSteps.", "runtime/skills/deploy/SKILL.md")
            .expect("parses");
        assert_eq!(parsed.name, "deploy");
        assert_eq!(parsed.project_name, None);
        assert_eq!(parsed.space_slug, None);
    }

    #[test]
    fn default_path_renders_each_tier() {
        assert_eq!(
            default_skill_path("deploy", None, Some("team")),
            "spaces/team/skills/deploy.md"
        );
        assert_eq!(
            default_skill_path("deploy", Some("alpha"), None),
            "runtime/projects/alpha/skills/deploy.md"
        );
        assert_eq!(
            default_skill_path("deploy", None, None),
            "runtime/skills/deploy.md"
        );
        // Defensive: if both somehow set (CHECK normally prevents),
        // space wins. Documents precedence.
        assert_eq!(
            default_skill_path("deploy", Some("alpha"), Some("team")),
            "spaces/team/skills/deploy.md"
        );
    }

    #[test]
    fn default_path_slugifies_name() {
        assert_eq!(
            default_skill_path("Deploy Prod", Some("alpha"), None),
            "runtime/projects/alpha/skills/deploy-prod.md"
        );
        assert_eq!(
            default_skill_path("Hello, World!", None, Some("team")),
            "spaces/team/skills/hello-world.md"
        );
        assert_eq!(
            default_skill_path("path/traversal/../etc", None, None),
            "runtime/skills/path-traversal-etc.md"
        );
    }
}
