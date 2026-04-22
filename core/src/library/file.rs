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

pub enum RuntimeFile {
    Note {
        workspace_name: String,
        slug: String,
        id_prefix: String,
        content: String,
    },
}

impl RuntimeFile {
    /// Build a `RuntimeFile::Note` from raw note fields.
    ///
    /// Used by both `Note::as_runtime_file` (existing entity) and the store
    /// flow (pre-entity, where `created_at` is empty).
    pub fn for_note(
        note_id: NoteId,
        workspace_id: WorkspaceId,
        workspace_name: &str,
        title: &str,
        content: &str,
        tags: &[String],
        created_at: &str,
    ) -> Self {
        let tags_str = tags
            .iter()
            .map(|t| format!("\"{}\"", t))
            .collect::<Vec<_>>()
            .join(", ");

        let md_content = format!(
            "---\nid: {}\nworkspace: {}\ntags: [{}]\ncreated: {}\n---\n\n# {}\n\n{}\n",
            note_id, workspace_id, tags_str, created_at, title, content
        );

        RuntimeFile::Note {
            workspace_name: workspace_name.to_string(),
            slug: slugify(title),
            id_prefix: note_id.to_string()[..8].to_string(),
            content: md_content,
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

    pub(super) fn content(&self) -> &str {
        match self {
            RuntimeFile::Note { content, .. } => content,
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
