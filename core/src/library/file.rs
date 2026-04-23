use sha1::{Digest, Sha1};

use crate::primitives::{NoteId, WorkspaceId};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GitFileHash(String);

impl GitFileHash {
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
}

impl DocType {
    pub fn as_str(&self) -> &'static str {
        match self {
            DocType::Note => "note",
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

pub enum RuntimeFile {
    Note {
        doc_id: NoteId,
        workspace_id: WorkspaceId,
        workspace_name: String,
        title: String,
        body: String,
        tags: Vec<String>,
        created_at: String,
        slug: String,
        id_prefix: String,
    },
}

impl RuntimeFile {
    /// Build a `RuntimeFile::Note` from raw note fields.
    pub fn for_note(
        note_id: NoteId,
        workspace_id: WorkspaceId,
        workspace_name: &str,
        title: &str,
        body: &str,
        tags: &[String],
        created_at: &str,
    ) -> Self {
        RuntimeFile::Note {
            doc_id: note_id,
            workspace_id,
            workspace_name: workspace_name.to_string(),
            title: title.to_string(),
            body: body.to_string(),
            tags: tags.to_vec(),
            created_at: created_at.to_string(),
            slug: slugify(title),
            id_prefix: note_id.to_string()[..8].to_string(),
        }
    }

    pub fn doc_type(&self) -> DocType {
        match self {
            RuntimeFile::Note { .. } => DocType::Note,
        }
    }

    pub fn searchable_fields(&self) -> SearchableFields {
        match self {
            RuntimeFile::Note {
                doc_id,
                workspace_id,
                title,
                body,
                tags,
                ..
            } => SearchableFields {
                doc_id: uuid::Uuid::from(*doc_id),
                doc_type: DocType::Note,
                workspace_id: uuid::Uuid::from(*workspace_id),
                title: title.clone(),
                body: body.clone(),
                tags: tags.clone(),
            },
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
                ..
            } => {
                let tags_str = tags
                    .iter()
                    .map(|t| format!("\"{}\"", t))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "---\nid: {}\ntags: [{}]\ncreated: {}\n---\n\n# {}\n\n{}\n",
                    doc_id, tags_str, created_at, title, body
                )
            }
        }
    }

    pub(super) fn commit_message(&self) -> String {
        match self {
            RuntimeFile::Note {
                slug, id_prefix, ..
            } => format!("note: {}-{}", slug, id_prefix),
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
