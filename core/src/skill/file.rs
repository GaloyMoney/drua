use drua_library::GitFileHash;

use crate::primitives::SkillId;

/// Renders a skill as markdown with frontmatter — the canonical
/// on-disk form. Identical bytes round-trip via the library's git
/// hash short-circuit.
pub fn render_skill_markdown(
    doc_id: uuid::Uuid,
    name: &str,
    description: &str,
    body: &str,
    created_at: &str,
    updated_at: &str,
) -> String {
    format!(
        "---\nid: {}\nname: \"{}\"\ndescription: \"{}\"\ncreated: {}\nupdated: {}\n---\n\n{}\n",
        doc_id,
        name.replace('"', "\\\""),
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
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    created: Option<String>,
    #[serde(default)]
    updated: Option<String>,
}

/// Name/description priority:
/// 1. Frontmatter `name:`/`description:` (canonical)
/// 2. `# Heading` / first paragraph (legacy)
/// 3. Filename (last resort)
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

    let has_fm_name = fm.name.is_some();

    let (name, description, body) = if let Some(fm_name) = fm.name {
        let desc = fm.description.unwrap_or_default();
        let body = after_fm.trim().to_string();
        (fm_name, desc, body)
    } else if let Some((h_name, h_desc, h_body)) = parse_heading_and_body(after_fm) {
        let desc = fm.description.unwrap_or(h_desc);
        (h_name, desc, h_body)
    } else {
        let name = name_from_filename(path)?;
        let desc = fm.description.unwrap_or_default();
        let body = after_fm.trim().to_string();
        (name, desc, body)
    };

    let created_at = fm.created.unwrap_or_default();
    let updated_at = fm.updated.unwrap_or_default();

    // Rewrite the file in place when frontmatter is missing critical
    // fields. The path itself is sacred — never renamed; this rewrite
    // is content-only.
    let _ = path;
    let needs_rewrite = !has_id || !has_fm_name;

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
    let (name, description, body) = if let Some(parsed) = parse_heading_and_body(content) {
        parsed
    } else {
        let name = name_from_filename(path)?;
        (name, String::new(), content.trim().to_string())
    };

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

fn parse_heading_and_body(content: &str) -> Option<(String, String, String)> {
    let content = content.trim_start_matches('\n');

    let name_line = content.lines().next()?;
    let name = name_line.strip_prefix("# ")?.trim().to_string();
    if name.is_empty() {
        return None;
    }

    let after_name = &content[name_line.len()..].trim_start_matches('\n');

    let (description, body) = if let Some((desc, bod)) = after_name.split_once("\n---\n") {
        (desc.trim().to_string(), bod.trim().to_string())
    } else {
        (after_name.trim().to_string(), String::new())
    };

    Some((name, description, body))
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
    if let Some(space) = space_slug {
        format!("spaces/{space}/skills/{name}.md")
    } else if let Some(project) = project_name {
        format!("runtime/projects/{project}/skills/{name}.md")
    } else {
        format!("runtime/skills/{name}.md")
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
}
