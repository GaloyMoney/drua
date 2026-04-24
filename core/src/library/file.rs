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
        workspace_id: WorkspaceId,
        workspace_name: String,
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
        workspace_id: WorkspaceId,
        workspace_name: &str,
        name: &str,
        description: &str,
        body: &str,
        created_at: &str,
        updated_at: &str,
    ) -> Self {
        RuntimeFile::Skill {
            doc_id: skill_id,
            workspace_id,
            workspace_name: workspace_name.to_string(),
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
                workspace_id: uuid::Uuid::from(*workspace_id),
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
            } => format!(
                "runtime/workspaces/{}/skills/{}-{}.md",
                workspace_name, slug, id_prefix
            ),
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
