use sha1::{Digest, Sha1};

use crate::primitives::{NoteId, SkillId, WorkspaceId};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
    },
    GitKeep {
        workspace_name: String,
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
            RuntimeFile::GitKeep { workspace_name } => {
                format!("runtime/workspaces/{}/notes/.gitkeep", workspace_name)
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
            RuntimeFile::GitKeep { workspace_name } => {
                format!("workspace: scaffold {}", workspace_name)
            }
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

/// Parse a skill markdown file (as produced by `RuntimeFile::Skill::content()`)
/// back into a `RuntimeFile::Skill` variant.
///
/// `workspace_id` and `workspace_name` are resolved by the caller from the
/// file path — they are not encoded in the markdown itself.
///
/// Returns `None` if the content cannot be parsed (missing frontmatter, id, etc.).
pub fn parse_skill_markdown(
    content: &str,
    workspace_id: Option<WorkspaceId>,
    workspace_name: Option<String>,
) -> Option<RuntimeFile> {
    // 1. Extract frontmatter between --- delimiters
    let content = content.trim();
    let rest = content.strip_prefix("---")?;
    let (frontmatter, after_fm) = rest.split_once("\n---")?;

    // 2. Parse frontmatter fields
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

    let id_str = id_str?;
    let skill_id: SkillId = id_str.parse::<uuid::Uuid>().ok()?.into();

    // 3. After frontmatter: expect `\n\n# {name}\n\n{description}\n\n---\n\n{body}\n`
    let after_fm = after_fm.trim_start_matches('\n');

    // Extract name from `# heading`
    let name_line = after_fm.lines().next()?;
    let name = name_line.strip_prefix("# ")?.trim().to_string();
    if name.is_empty() {
        return None;
    }

    // Everything after the heading line
    let after_name = &after_fm[name_line.len()..].trim_start_matches('\n');

    // Split on the body separator `---`
    let (description, body) = if let Some((desc, bod)) = after_name.split_once("\n---\n") {
        (desc.trim().to_string(), bod.trim().to_string())
    } else {
        // No body separator — treat everything as description
        (after_name.trim().to_string(), String::new())
    };

    let slug = slugify(&name);
    let id_prefix = skill_id.to_string()[..8].to_string();

    Some(RuntimeFile::Skill {
        doc_id: skill_id,
        workspace_id,
        workspace_name,
        name,
        description,
        body,
        created_at,
        updated_at,
        slug,
        id_prefix,
    })
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
        let ws_id = WorkspaceId::new();
        let original = RuntimeFile::for_skill(
            skill_id,
            Some(ws_id),
            Some("my-workspace"),
            "Deploy Script",
            "Deploys the app to production",
            "#!/bin/bash\necho deploy",
            "2025-01-01T00:00:00Z",
            "2025-06-01T00:00:00Z",
        );

        let content = original.content();
        let parsed = parse_skill_markdown(&content, Some(ws_id), Some("my-workspace".into()))
            .expect("should parse");

        match parsed {
            RuntimeFile::Skill {
                doc_id,
                workspace_id,
                workspace_name,
                name,
                description,
                body,
                created_at,
                updated_at,
                ..
            } => {
                assert_eq!(doc_id, skill_id);
                assert_eq!(workspace_id, Some(ws_id));
                assert_eq!(workspace_name.as_deref(), Some("my-workspace"));
                assert_eq!(name, "Deploy Script");
                assert_eq!(description, "Deploys the app to production");
                assert_eq!(body, "#!/bin/bash\necho deploy");
                assert_eq!(created_at, "2025-01-01T00:00:00Z");
                assert_eq!(updated_at, "2025-06-01T00:00:00Z");
            }
            _ => panic!("expected Skill variant"),
        }
    }

    #[test]
    fn parse_skill_markdown_global() {
        let skill_id = SkillId::new();
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
        let parsed = parse_skill_markdown(&content, None, None).expect("should parse global skill");

        match parsed {
            RuntimeFile::Skill {
                doc_id,
                workspace_id,
                name,
                ..
            } => {
                assert_eq!(doc_id, skill_id);
                assert_eq!(workspace_id, None);
                assert_eq!(name, "Global Skill");
            }
            _ => panic!("expected Skill variant"),
        }
    }

    #[test]
    fn parse_skill_markdown_returns_none_for_bad_input() {
        assert!(parse_skill_markdown("not markdown", None, None).is_none());
        assert!(parse_skill_markdown("---\n---\n\n# Name\n", None, None).is_none());
        assert!(parse_skill_markdown("---\nid: not-a-uuid\n---\n\n# Name\n", None, None).is_none());
    }

    #[test]
    fn parse_skill_hash_matches_original() {
        let skill_id = SkillId::new();
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
        let parsed = parse_skill_markdown(&content, None, None).unwrap();
        let parsed_hash = parsed.file_hash();

        assert_eq!(original_hash, parsed_hash);
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
