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
/// importer should re-render to canonical form (legacy/imported
/// formats normalise on first sync).
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ParsedSkill {
    pub skill_id: SkillId,
    pub project_name: Option<String>,
    pub name: String,
    pub description: String,
    pub body: String,
    pub created_at: String,
    pub updated_at: String,
    pub original_path: String,
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
    let content = content.trim();
    if content.is_empty() {
        return None;
    }

    let mut parsed = if content.starts_with("---") {
        parse_skill_with_frontmatter(content, project_name, path)?
    } else {
        parse_skill_without_frontmatter(content, project_name, path)?
    };

    parsed.original_path = path.to_string();
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

    let canonical_path = canonical_skill_path(skill_id, &name, project_name.as_deref());
    let needs_rewrite = !has_id || !has_fm_name || canonical_path != path;

    Some(ParsedSkill {
        skill_id,
        project_name,
        name,
        description,
        body,
        created_at,
        updated_at,
        original_path: String::new(),
        needs_rewrite,
    })
}

fn parse_skill_without_frontmatter(
    content: &str,
    project_name: Option<String>,
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
        name,
        description,
        body,
        created_at: String::new(),
        updated_at: String::new(),
        original_path: String::new(),
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
/// `runtime/skills/*.md` → `None` (global skill).
pub fn project_name_from_skill_path(relative_path: &str) -> Option<String> {
    let parts: Vec<&str> = relative_path.split('/').collect();
    if parts.len() >= 5 && parts[0] == "runtime" && parts[1] == "projects" && parts[3] == "skills" {
        Some(parts[2].to_string())
    } else {
        None
    }
}

/// `runtime/skills/ci-check-019dc56a.md` → `"Ci Check"`.
pub fn name_from_filename(path: &str) -> Option<String> {
    let filename = path.rsplit('/').next()?;
    let stem = filename
        .strip_suffix(".md")
        .or_else(|| filename.strip_suffix(".yml"))
        .unwrap_or(filename);

    let base = if let Some((prefix, suffix)) = stem.rsplit_once('-') {
        if suffix.len() == 8 && suffix.chars().all(|c| c.is_ascii_hexdigit()) {
            prefix
        } else {
            stem
        }
    } else {
        stem
    };

    if base.is_empty() {
        return None;
    }

    let name = base
        .split('-')
        .filter(|s| !s.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(c) => {
                    let upper: String = c.to_uppercase().collect();
                    format!("{upper}{}", chars.as_str())
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ");

    if name.is_empty() {
        None
    } else {
        Some(name)
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

pub fn canonical_skill_path(id: SkillId, name: &str, project_name: Option<&str>) -> String {
    let id_uuid = uuid::Uuid::from(id);
    let id_prefix = &id_uuid.to_string()[..8];
    let slug = slugify(name);
    match project_name {
        Some(project) => format!("runtime/projects/{project}/skills/{slug}-{id_prefix}.md"),
        None => format!("runtime/skills/{slug}-{id_prefix}.md"),
    }
}
