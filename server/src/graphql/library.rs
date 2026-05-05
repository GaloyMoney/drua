use async_graphql::{Enum, InputObject, SimpleObject};

use drua_core::library::{DocType, SearchHit};
use drua_core::note::NOTE_DOC_TYPE;
use drua_core::skill::SKILL_DOC_TYPE;

use super::primitives::*;

const SPACE_FILE_DOC_TYPE_STR: &str = "space_file";

/// Workflow files are git-synced to the library repo but not exposed
/// here — `AuthedSearch::search` filters them out at the boundary.
#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
pub enum LibraryFileType {
    Skill,
    Note,
    SpaceFile,
}

impl From<LibraryFileType> for DocType {
    fn from(t: LibraryFileType) -> Self {
        match t {
            LibraryFileType::Skill => SKILL_DOC_TYPE,
            LibraryFileType::Note => NOTE_DOC_TYPE,
            LibraryFileType::SpaceFile => DocType::new(SPACE_FILE_DOC_TYPE_STR),
        }
    }
}

/// Workflow rows are filtered out before reaching this conversion;
/// non-recognised doc types default to Note as a defensive fallback.
impl From<DocType> for LibraryFileType {
    fn from(t: DocType) -> Self {
        match t.as_str() {
            "skill" => LibraryFileType::Skill,
            "space_file" => LibraryFileType::SpaceFile,
            _ => LibraryFileType::Note,
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
    pub fn from_domain(hit: SearchHit) -> Self {
        let snippet = make_snippet(&hit.fields.content, 240);
        // Two flavours of space-scoped hits:
        //   1. raw space files     (doc_type = "space_file")
        //   2. entity-backed skills under `spaces/<slug>/skills/...`
        //      (doc_type = "skill" but path lives under `spaces/`)
        // Both must surface the space slug + a project_id of `null`
        // so the UI doesn't mislabel a space-scoped entity as a
        // project-scoped one.
        let is_space_file = hit.fields.doc_type.as_str() == SPACE_FILE_DOC_TYPE_STR;
        let path_under_space = hit
            .fields
            .path
            .as_deref()
            .is_some_and(|p| p.starts_with("spaces/"));
        let is_space_scoped = is_space_file || path_under_space;
        let project_id = if is_space_scoped {
            None
        } else {
            hit.fields.scope_id.map(ProjectId::from)
        };
        let space_slug = if is_space_scoped {
            hit.fields.scope_slug.clone()
        } else {
            None
        };
        let relative_path = if is_space_file {
            hit.fields.path.clone()
        } else {
            None
        };
        Self {
            id: UUID::from(hit.fields.doc_id),
            project_id,
            r#type: hit.fields.doc_type.into(),
            title: hit.fields.name,
            snippet,
            score: hit.score,
            tags: Vec::new(),
            space_slug,
            relative_path,
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
