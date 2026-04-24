use sha1::{Digest, Sha1};

use crate::primitives::{NoteId, SkillId, WorkspaceId};

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GitFileHash(String);

impl GitFileHash {
    pub(super) fn from_sha1(hex: String) -> Self {
        Self(hex)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for GitFileHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Discriminator for document types stored in the library search index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocType {
    Note,
    Skill,
}

impl DocType {
    pub fn as_str(&self) -> &'static str {
        match self {
            DocType::Note => "note",
            DocType::Skill => "skill",
        }
    }
}

/// Fields extracted from a `RuntimeFile` for search indexing.
pub struct SearchableFields {
    pub doc_id: uuid::Uuid,
    pub doc_type: DocType,
    pub workspace_id: uuid::Uuid,
    pub title: String,
    pub body: String,
    pub tags: Vec<String>,
}

impl SearchableFields {
    pub fn text_for_embedding(&self) -> String {
        format!("{}\n\n{}", self.title, self.body)
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeFile {
    Note {
        doc_id: NoteId,
        workspace_id: WorkspaceId,
        workspace_name: String,
        title: String,
        body: String,
        tags: Vec<String>,
        created_at: String,
        updated_at: String,
        slug: String,
        id_prefix: String,
    },
    Skill {
        doc_id: SkillId,
        workspace_id: Option<WorkspaceId>,
        workspace_name: Option<String>,
        name: String,
        description: String,
        body: String,
        created_at: String,
        updated_at: String,
        slug: String,
        id_prefix: String,
        /// When set, the original file path on disk before canonicalisation.
        /// The `WriteToRuntime` job will remove this path if it differs from
        /// the canonical `relative_path()`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        original_path: Option<String>,
    },
    GitKeep {
        workspace_name: String,
        /// Subdirectory under the workspace folder (e.g. `"notes"`, `"skills"`).
        subdir: String,
    },
}

impl RuntimeFile {
    /// Build a `RuntimeFile::Note` from raw note fields.
    #[allow(clippy::too_many_arguments)]
    pub fn for_note(
        note_id: NoteId,
        workspace_id: WorkspaceId,
        workspace_name: &str,
        title: &str,
        body: &str,
        tags: &[String],
        created_at: &str,
        updated_at: &str,
    ) -> Self {
        RuntimeFile::Note {
            doc_id: note_id,
            workspace_id,
            workspace_name: workspace_name.to_string(),
            title: title.to_string(),
            body: body.to_string(),
            tags: tags.to_vec(),
            created_at: created_at.to_string(),
            updated_at: updated_at.to_string(),
            slug: slugify(title),
            id_prefix: note_id.to_string()[..8].to_string(),
        }
    }

    /// Build a `RuntimeFile::Skill` from raw skill fields.
    #[allow(clippy::too_many_arguments)]
    pub fn for_skill(
        skill_id: SkillId,
        workspace_id: Option<WorkspaceId>,
        workspace_name: Option<&str>,
        name: &str,
        description: &str,
        body: &str,
        created_at: &str,
        updated_at: &str,
    ) -> Self {
        Self::for_skill_with_original_path(
            skill_id,
            workspace_id,
            workspace_name,
            name,
            description,
            body,
            created_at,
            updated_at,
            None,
        )
    }

    /// Build a `RuntimeFile::Skill` with an optional `original_path` for
    /// files that need renaming on disk.
    #[allow(clippy::too_many_arguments)]
    pub fn for_skill_with_original_path(
        skill_id: SkillId,
        workspace_id: Option<WorkspaceId>,
        workspace_name: Option<&str>,
        name: &str,
        description: &str,
        body: &str,
        created_at: &str,
        updated_at: &str,
        original_path: Option<String>,
    ) -> Self {
        RuntimeFile::Skill {
            doc_id: skill_id,
            workspace_id,
            workspace_name: workspace_name.map(|s| s.to_string()),
            name: name.to_string(),
            description: description.to_string(),
            body: body.to_string(),
            created_at: created_at.to_string(),
            updated_at: updated_at.to_string(),
            slug: slugify(name),
            id_prefix: skill_id.to_string()[..8].to_string(),
            original_path,
        }
    }

    pub fn searchable_fields(&self) -> Option<SearchableFields> {
        match self {
            RuntimeFile::Note {
                doc_id,
                workspace_id,
                title,
                body,
                tags,
                ..
            } => Some(SearchableFields {
                doc_id: uuid::Uuid::from(*doc_id),
                doc_type: DocType::Note,
                workspace_id: uuid::Uuid::from(*workspace_id),
                title: title.clone(),
                body: body.clone(),
                tags: tags.clone(),
            }),
            RuntimeFile::Skill {
                doc_id,
                workspace_id,
                name,
                description,
                ..
            } => Some(SearchableFields {
                doc_id: uuid::Uuid::from(*doc_id),
                doc_type: DocType::Skill,
                workspace_id: workspace_id
                    .map(uuid::Uuid::from)
                    .unwrap_or(uuid::Uuid::nil()),
                title: name.clone(),
                body: description.clone(),
                tags: Vec::new(),
            }),
            RuntimeFile::GitKeep { .. } => None,
        }
    }

    pub(super) fn relative_path(&self) -> String {
        match self {
            RuntimeFile::Note {
                workspace_name,
                slug,
                id_prefix,
                ..
            } => format!(
                "runtime/workspaces/{}/notes/{}-{}.md",
                workspace_name, slug, id_prefix
            ),
            RuntimeFile::Skill {
                workspace_name,
                slug,
                id_prefix,
                ..
            } => match workspace_name {
                Some(ws) => format!("runtime/workspaces/{}/skills/{}-{}.md", ws, slug, id_prefix),
                None => format!("runtime/skills/{}-{}.md", slug, id_prefix),
            },
            RuntimeFile::GitKeep {
                workspace_name,
                subdir,
            } => {
                format!("runtime/workspaces/{workspace_name}/{subdir}/.gitkeep")
            }
        }
    }

    /// Render the file content on-the-fly from structured fields.
    pub(crate) fn content(&self) -> String {
        match self {
            RuntimeFile::Note {
                doc_id,
                title,
                body,
                tags,
                created_at,
                updated_at,
                ..
            } => {
                let tags_str = tags
                    .iter()
                    .map(|t| format!("\"{}\"", t))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "---\nid: {}\ntags: [{}]\ncreated: {}\nupdated: {}\n---\n\n# {}\n\n{}\n",
                    doc_id, tags_str, created_at, updated_at, title, body
                )
            }
            RuntimeFile::Skill {
                doc_id,
                name,
                description,
                body,
                created_at,
                updated_at,
                ..
            } => {
                format!(
                    "---\nid: {}\ncreated: {}\nupdated: {}\n---\n\n# {}\n\n{}\n\n---\n\n{}\n",
                    doc_id, created_at, updated_at, name, description, body
                )
            }
            RuntimeFile::GitKeep { .. } => String::new(),
        }
    }

    pub(super) fn commit_message(&self) -> String {
        match self {
            RuntimeFile::Note {
                slug, id_prefix, ..
            } => format!("note: {}-{}", slug, id_prefix),
            RuntimeFile::Skill {
                slug, id_prefix, ..
            } => format!("skill: {}-{}", slug, id_prefix),
            RuntimeFile::GitKeep {
                workspace_name,
                subdir,
            } => {
                format!("workspace: scaffold {workspace_name}/{subdir}")
            }
        }
    }

    /// The original file path before canonicalisation, if set.
    pub(crate) fn original_path(&self) -> Option<&str> {
        match self {
            RuntimeFile::Skill { original_path, .. } => original_path.as_deref(),
            _ => None,
        }
    }

    /// Set the original file path (for files that need renaming after import).
    pub(crate) fn set_original_path(&mut self, path: String) {
        if let RuntimeFile::Skill { original_path, .. } = self {
            *original_path = Some(path);
        }
    }

    /// Compute the git blob SHA-1 hash for the file content, identical to
    /// what `git hash-object` would produce.
    pub fn file_hash(&self) -> GitFileHash {
        let content = self.content();
        let header = format!("blob {}\0", content.len());
        let mut hasher = Sha1::new();
        hasher.update(header.as_bytes());
        hasher.update(content.as_bytes());
        GitFileHash(format!("{:x}", hasher.finalize()))
    }
}

/// Result of parsing a skill markdown file.
pub struct ParsedSkillFile {
    pub file: RuntimeFile,
    /// When `true` the file on disk lacks proper frontmatter (or an `id:` field)
    /// and should be rewritten with canonical headers after entity creation.
    pub needs_rewrite: bool,
}

/// Parse a skill markdown file back into a `RuntimeFile::Skill` variant.
///
/// `path` is the file's relative path in the library repo (e.g.
/// `runtime/skills/my-skill-abcd1234.md`). The function derives
/// `workspace_name` from the path and always stores `path` as
/// `original_path` on the returned `RuntimeFile`.
///
/// Handles three formats:
/// 1. **Full frontmatter** (as produced by `RuntimeFile::Skill::content()`) —
///    `id:`, `created:`, `updated:` present. `needs_rewrite = false`.
/// 2. **Frontmatter without `id:`** — timestamps may be present but no id.
///    Generates a new `SkillId`. `needs_rewrite = true`.
/// 3. **No frontmatter** — human-authored file starting with `# Name`.
///    Generates a new `SkillId`. `needs_rewrite = true`.
///
/// Returns `None` only if the content has no recognisable `# heading`.
pub fn parse_skill_markdown(content: &str, path: &str) -> Option<ParsedSkillFile> {
    let workspace_name = workspace_name_from_skill_path(path);
    let content = content.trim();

    let mut parsed = if content.starts_with("---") {
        parse_with_frontmatter(content, workspace_name)?
    } else {
        parse_without_frontmatter(content, workspace_name)?
    };

    parsed.file.set_original_path(path.to_string());

    Some(parsed)
}

/// Parse a skill file that has frontmatter (starts with `---`).
fn parse_with_frontmatter(
    content: &str,
    workspace_name: Option<String>,
) -> Option<ParsedSkillFile> {
    let rest = content.strip_prefix("---")?;
    let (frontmatter, after_fm) = rest.split_once("\n---")?;

    let mut id_str = None;
    let mut created_at = String::new();
    let mut updated_at = String::new();
    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some(val) = line.strip_prefix("id:") {
            id_str = Some(val.trim().to_string());
        } else if let Some(val) = line.strip_prefix("created:") {
            created_at = val.trim().to_string();
        } else if let Some(val) = line.strip_prefix("updated:") {
            updated_at = val.trim().to_string();
        }
    }

    let (skill_id, needs_rewrite) = match id_str {
        Some(ref s) => {
            let uuid = s.parse::<uuid::Uuid>().ok()?;
            (SkillId::from(uuid), false)
        }
        None => (SkillId::new(), true),
    };

    let (name, description, body) = parse_heading_and_body(after_fm)?;

    let slug = slugify(&name);
    let id_prefix = skill_id.to_string()[..8].to_string();

    Some(ParsedSkillFile {
        file: RuntimeFile::Skill {
            doc_id: skill_id,
            workspace_id: None,
            workspace_name,
            name,
            description,
            body,
            created_at,
            updated_at,
            slug,
            id_prefix,
            original_path: None,
        },
        needs_rewrite,
    })
}

/// Parse a skill file with no frontmatter (human-authored).
fn parse_without_frontmatter(
    content: &str,
    workspace_name: Option<String>,
) -> Option<ParsedSkillFile> {
    let (name, description, body) = parse_heading_and_body(content)?;
    let skill_id = SkillId::new();
    let slug = slugify(&name);
    let id_prefix = skill_id.to_string()[..8].to_string();

    Some(ParsedSkillFile {
        file: RuntimeFile::Skill {
            doc_id: skill_id,
            workspace_id: None,
            workspace_name,
            name,
            description,
            body,
            created_at: String::new(),
            updated_at: String::new(),
            slug,
            id_prefix,
            original_path: None,
        },
        needs_rewrite: true,
    })
}

/// Extract `(name, description, body)` from content after any frontmatter.
///
/// Expects `# Name` heading, optional description, optional `---` body separator.
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

/// Extract the workspace name from a skill file's relative path.
///
/// - `runtime/workspaces/{ws_name}/skills/*.md` → `Some(ws_name)`
/// - `runtime/skills/*.md` → `None` (global skill)
pub fn workspace_name_from_skill_path(relative_path: &str) -> Option<String> {
    let parts: Vec<&str> = relative_path.split('/').collect();
    // runtime/workspaces/{ws_name}/skills/{file}.md
    if parts.len() >= 5 && parts[0] == "runtime" && parts[1] == "workspaces" && parts[3] == "skills"
    {
        Some(parts[2].to_string())
    } else {
        None
    }
}

fn slugify(title: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_skill_markdown_roundtrip() {
        let skill_id = SkillId::new();
        let id_prefix = &skill_id.to_string()[..8];
        let original = RuntimeFile::for_skill(
            skill_id,
            Some(WorkspaceId::new()),
            Some("my-workspace"),
            "Deploy Script",
            "Deploys the app to production",
            "#!/bin/bash\necho deploy",
            "2025-01-01T00:00:00Z",
            "2025-06-01T00:00:00Z",
        );

        let content = original.content();
        let path = format!("runtime/workspaces/my-workspace/skills/deploy-script-{id_prefix}.md");
        let parsed = parse_skill_markdown(&content, &path).expect("should parse");
        assert!(!parsed.needs_rewrite);

        match parsed.file {
            RuntimeFile::Skill {
                doc_id,
                workspace_name,
                name,
                description,
                body,
                created_at,
                updated_at,
                original_path,
                ..
            } => {
                assert_eq!(doc_id, skill_id);
                assert_eq!(workspace_name.as_deref(), Some("my-workspace"));
                assert_eq!(name, "Deploy Script");
                assert_eq!(description, "Deploys the app to production");
                assert_eq!(body, "#!/bin/bash\necho deploy");
                assert_eq!(created_at, "2025-01-01T00:00:00Z");
                assert_eq!(updated_at, "2025-06-01T00:00:00Z");
                assert_eq!(original_path.as_deref(), Some(path.as_str()));
            }
            _ => panic!("expected Skill variant"),
        }
    }

    #[test]
    fn parse_skill_markdown_global() {
        let skill_id = SkillId::new();
        let id_prefix = &skill_id.to_string()[..8];
        let original = RuntimeFile::for_skill(
            skill_id,
            None,
            None,
            "Global Skill",
            "A global skill",
            "body content",
            "",
            "",
        );

        let content = original.content();
        let path = format!("runtime/skills/global-skill-{id_prefix}.md");
        let parsed = parse_skill_markdown(&content, &path).expect("should parse global skill");
        assert!(!parsed.needs_rewrite);

        match parsed.file {
            RuntimeFile::Skill {
                doc_id,
                workspace_id,
                workspace_name,
                name,
                ..
            } => {
                assert_eq!(doc_id, skill_id);
                assert_eq!(workspace_id, None);
                assert_eq!(workspace_name, None);
                assert_eq!(name, "Global Skill");
            }
            _ => panic!("expected Skill variant"),
        }
    }

    #[test]
    fn parse_skill_markdown_returns_none_for_bad_input() {
        let path = "runtime/skills/test.md";
        // No heading at all
        assert!(parse_skill_markdown("not markdown", path).is_none());
        // Frontmatter with bad UUID
        assert!(parse_skill_markdown("---\nid: not-a-uuid\n---\n\n# Name\n", path).is_none());
    }

    #[test]
    fn parse_skill_hash_matches_original() {
        let skill_id = SkillId::new();
        let id_prefix = &skill_id.to_string()[..8];
        let original = RuntimeFile::for_skill(
            skill_id,
            None,
            None,
            "Test",
            "desc",
            "body",
            "2025-01-01T00:00:00Z",
            "2025-01-01T00:00:00Z",
        );
        let original_hash = original.file_hash();

        let content = original.content();
        let path = format!("runtime/skills/test-{id_prefix}.md");
        let parsed = parse_skill_markdown(&content, &path).unwrap();
        let parsed_hash = parsed.file.file_hash();

        assert_eq!(original_hash, parsed_hash);
    }

    #[test]
    fn parse_skill_markdown_without_frontmatter() {
        let content = "# My Cool Skill\n\nDoes something useful\n\n---\n\nThe body template";
        let path = "runtime/skills/my-cool-skill.md";
        let parsed = parse_skill_markdown(content, path).expect("should parse");
        assert!(parsed.needs_rewrite);

        match &parsed.file {
            RuntimeFile::Skill {
                name,
                description,
                body,
                original_path,
                ..
            } => {
                assert_eq!(name, "My Cool Skill");
                assert_eq!(description, "Does something useful");
                assert_eq!(body, "The body template");
                assert_eq!(original_path.as_deref(), Some(path));
            }
            _ => panic!("expected Skill variant"),
        }
    }

    #[test]
    fn parse_skill_markdown_without_frontmatter_no_body() {
        let content = "# Simple Skill\n\nJust a description, no body";
        let path = "runtime/skills/simple-skill.md";
        let parsed = parse_skill_markdown(content, path).expect("should parse");
        assert!(parsed.needs_rewrite);

        match &parsed.file {
            RuntimeFile::Skill {
                name,
                description,
                body,
                ..
            } => {
                assert_eq!(name, "Simple Skill");
                assert_eq!(description, "Just a description, no body");
                assert!(body.is_empty());
            }
            _ => panic!("expected Skill variant"),
        }
    }

    #[test]
    fn parse_skill_markdown_frontmatter_without_id() {
        let content = "---\ncreated: 2025-01-01\nupdated: 2025-06-01\n---\n\n# My Skill\n\nDescription\n\n---\n\nBody";
        let path = "runtime/workspaces/team/skills/my-skill.md";
        let parsed = parse_skill_markdown(content, path).expect("should parse");
        assert!(parsed.needs_rewrite);

        match &parsed.file {
            RuntimeFile::Skill {
                name,
                workspace_name,
                original_path,
                ..
            } => {
                assert_eq!(name, "My Skill");
                assert_eq!(workspace_name.as_deref(), Some("team"));
                assert_eq!(original_path.as_deref(), Some(path));
            }
            _ => panic!("expected Skill variant"),
        }
    }

    #[test]
    fn parse_skill_markdown_empty_frontmatter_generates_id() {
        let content = "---\n---\n\n# Name\n\nDesc";
        let path = "runtime/skills/name.md";
        let parsed = parse_skill_markdown(content, path).expect("should parse");
        assert!(parsed.needs_rewrite);

        match &parsed.file {
            RuntimeFile::Skill { name, .. } => {
                assert_eq!(name, "Name");
            }
            _ => panic!("expected Skill variant"),
        }
    }

    #[test]
    fn workspace_name_from_skill_path_workspace_scoped() {
        let path = "runtime/workspaces/my-ws/skills/deploy-script-abc12345.md";
        assert_eq!(
            workspace_name_from_skill_path(path),
            Some("my-ws".to_string())
        );
    }

    #[test]
    fn workspace_name_from_skill_path_global() {
        let path = "runtime/skills/deploy-script-abc12345.md";
        assert_eq!(workspace_name_from_skill_path(path), None);
    }
}
