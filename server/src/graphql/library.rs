use async_graphql::{Enum, InputObject, SimpleObject};

use super::primitives::*;

use drua_core::library::{DocType, GlobalSearchHit};

/// Workflow files are git-synced to the library repo but not exposed
/// here — `library.search_global` filters them out at the boundary.
#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
pub enum LibraryFileType {
    Skill,
    Note,
}

impl From<LibraryFileType> for DocType {
    fn from(t: LibraryFileType) -> Self {
        match t {
            LibraryFileType::Skill => DocType::Skill,
            LibraryFileType::Note => DocType::Note,
        }
    }
}

/// Workflow rows can't reach this conversion in practice because
/// `Library::search_global` drops them before fusion; default to Note
/// if one ever does.
impl From<DocType> for LibraryFileType {
    fn from(t: DocType) -> Self {
        match t {
            DocType::Skill => LibraryFileType::Skill,
            DocType::Note | DocType::Workflow => LibraryFileType::Note,
        }
    }
}

#[derive(InputObject)]
pub struct LibrarySearchInput {
    pub query: String,
    /// Empty / omitted = all types.
    pub types: Option<Vec<LibraryFileType>>,
    /// Omitted = all workspaces the subject can read.
    pub workspace_id: Option<WorkspaceId>,
    #[graphql(default = 50)]
    pub limit: i32,
}

#[derive(SimpleObject, Clone)]
pub struct LibrarySearchHit {
    pub id: UUID,
    pub workspace_id: WorkspaceId,
    pub r#type: LibraryFileType,
    pub title: String,
    pub snippet: String,
    pub score: f64,
    pub tags: Vec<String>,
}

impl LibrarySearchHit {
    pub fn from_domain(hit: GlobalSearchHit) -> Self {
        let snippet = make_snippet(&hit.content, 240);
        Self {
            id: UUID::from(hit.doc_id),
            workspace_id: WorkspaceId::from(hit.workspace_id),
            r#type: hit.doc_type.into(),
            title: hit.title,
            snippet,
            score: hit.score,
            tags: hit.tags,
        }
    }
}

fn make_snippet(content: &str, max_chars: usize) -> String {
    let trimmed = content.trim();
    let mut out = String::with_capacity(max_chars + 1);
    for ch in trimmed.chars().take(max_chars) {
        out.push(ch);
    }
    if trimmed.chars().count() > max_chars {
        out.push('…');
    }
    out
}
