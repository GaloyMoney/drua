use async_graphql::{Enum, InputObject, SimpleObject};

use super::primitives::*;

use drua_core::library::{DocType, GlobalSearchHit};

/// Workflow files are git-synced to the library repo but not exposed
/// here — `library.search_global` filters them out at the boundary.
#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
pub enum LibraryFileType {
    Skill,
    Note,
    SpaceFile,
}

impl From<LibraryFileType> for DocType {
    fn from(t: LibraryFileType) -> Self {
        match t {
            LibraryFileType::Skill => DocType::Skill,
            LibraryFileType::Note => DocType::Note,
            LibraryFileType::SpaceFile => DocType::SpaceFile,
        }
    }
}

/// `Workflow` rows can't reach this conversion in practice —
/// `Library::search_global` drops them before fusion. Default to Note
/// if one ever does.
impl From<DocType> for LibraryFileType {
    fn from(t: DocType) -> Self {
        match t {
            DocType::Skill => LibraryFileType::Skill,
            DocType::Note => LibraryFileType::Note,
            DocType::SpaceFile => LibraryFileType::SpaceFile,
            DocType::Workflow => LibraryFileType::Note,
        }
    }
}

#[derive(InputObject)]
pub struct LibrarySearchInput {
    pub query: String,
    /// Empty / omitted = all types.
    pub types: Option<Vec<LibraryFileType>>,
    /// Omitted = all projects the subject can read.
    pub project_id: Option<ProjectId>,
    #[graphql(default = 50)]
    pub limit: i32,
}

#[derive(SimpleObject, Clone)]
pub struct LibrarySearchHit {
    pub id: UUID,
    /// `null` for global content (skills with no project) and for
    /// space files (which are scoped to a `space_slug` instead).
    pub project_id: Option<ProjectId>,
    pub r#type: LibraryFileType,
    pub title: String,
    pub snippet: String,
    pub score: f64,
    pub tags: Vec<String>,
    /// Set on `SpaceFile` hits. Use this — not `project_id` — to
    /// label the result's scope in UIs.
    pub space_slug: Option<String>,
    /// Set on `SpaceFile` hits: the file's path inside `spaces/<slug>/`.
    pub relative_path: Option<String>,
}

impl LibrarySearchHit {
    pub fn from_domain(hit: GlobalSearchHit) -> Self {
        let snippet = make_snippet(&hit.content, 240);
        let project_id = if hit.project_id.is_nil() {
            None
        } else {
            Some(ProjectId::from(hit.project_id))
        };
        Self {
            id: UUID::from(hit.doc_id),
            project_id,
            r#type: hit.doc_type.into(),
            title: hit.title,
            snippet,
            score: hit.score,
            tags: hit.tags,
            space_slug: hit.space_slug,
            relative_path: hit.relative_path,
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
