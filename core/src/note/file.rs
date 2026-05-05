use drua_library::GitFileHash;

use crate::primitives::NoteId;
use crate::skill::file::{name_from_filename, slugify};

/// Renders a note as markdown with frontmatter — the canonical
/// on-disk form. Identical bytes round-trip via the library's git
/// hash short-circuit.
pub fn render_note_markdown(
    doc_id: uuid::Uuid,
    title: &str,
    body: &str,
    tags: &[String],
    created_at: &str,
    updated_at: &str,
) -> String {
    render_note_markdown_with_pin(doc_id, title, body, tags, created_at, updated_at, false)
}

/// Variant that emits `pinned: true` in the frontmatter when set.
/// Used by space-scoped note authoring (the on-disk markdown is the
/// source of truth for the pin bit there).
pub fn render_note_markdown_with_pin(
    doc_id: uuid::Uuid,
    title: &str,
    body: &str,
    tags: &[String],
    created_at: &str,
    updated_at: &str,
    pinned: bool,
) -> String {
    let tags_str = tags
        .iter()
        .map(|t| format!("\"{}\"", t))
        .collect::<Vec<_>>()
        .join(", ");
    let pinned_line = if pinned { "pinned: true\n" } else { "" };
    format!(
        "---\nid: {}\ntags: [{}]\ncreated: {}\nupdated: {}\n{}---\n\n# {}\n\n{}\n",
        doc_id, tags_str, created_at, updated_at, pinned_line, title, body
    )
}

/// Default repo-relative path for a note created through the
/// DB-driven service surface (`Notes::store`). The importer does NOT
/// use this — paths it sees are whatever the author wrote.
pub fn default_note_path(
    title: &str,
    project_name: Option<&str>,
    space_slug: Option<&str>,
) -> String {
    let slug = slugify(title);
    if let Some(space) = space_slug {
        format!("spaces/{space}/notes/{slug}.md")
    } else if let Some(project) = project_name {
        format!("runtime/projects/{project}/notes/{slug}.md")
    } else {
        format!("runtime/notes/{slug}.md")
    }
}

/// `runtime/projects/{project}/notes/*.md` → `Some(project)`.
pub fn project_name_from_note_path(relative_path: &str) -> Option<String> {
    let parts: Vec<&str> = relative_path.split('/').collect();
    if parts.len() >= 5 && parts[0] == "runtime" && parts[1] == "projects" && parts[3] == "notes" {
        Some(parts[2].to_string())
    } else {
        None
    }
}

/// `spaces/{slug}/notes/*.md` → `Some(slug)`.
pub fn space_slug_from_note_path(relative_path: &str) -> Option<String> {
    let parts: Vec<&str> = relative_path.split('/').collect();
    if parts.len() >= 4 && parts[0] == "spaces" && parts[2] == "notes" {
        Some(parts[1].to_string())
    } else {
        None
    }
}

/// Parsed note from on-disk content. `project_name` and `space_slug`
/// are mutually exclusive — the path determines which (if any) is set.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ParsedNote {
    pub note_id: NoteId,
    pub project_name: Option<String>,
    pub space_slug: Option<String>,
    pub title: String,
    pub content: String,
    pub tags: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
    /// Initial pinned state from the frontmatter (`pinned: true`); only
    /// honoured at first import — subsequent updates leave the bit
    /// alone (the entity's `pin` / `unpin` events are the source of
    /// truth post-create).
    pub pinned: bool,
    /// Repo-relative path the file was found at — sacred per
    /// path-as-identity. The importer never rewrites this.
    pub path: String,
}

impl ParsedNote {
    /// Canonical on-disk form for the parsed note.
    pub fn render(&self) -> String {
        render_note_markdown(
            self.note_id.into(),
            &self.title,
            &self.content,
            &self.tags,
            &self.created_at,
            &self.updated_at,
        )
    }

    pub fn file_hash(&self) -> GitFileHash {
        GitFileHash::new(self.render())
    }
}

#[derive(serde::Deserialize, Default)]
struct NoteFrontmatter {
    #[serde(default)]
    id: Option<uuid::Uuid>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    created: Option<String>,
    #[serde(default)]
    updated: Option<String>,
    #[serde(default)]
    pinned: bool,
}

/// Parses canonical note markdown:
///   `---\nid: …\ntags: […]\ncreated: …\nupdated: …\n---\n\n# {title}\n\n{body}`
///
/// Falls back to filename-derived title when the `# heading` is missing
/// (mirrors `parse_skill_markdown` precedence).
pub fn parse_note_markdown(content: &str, path: &str) -> Option<ParsedNote> {
    let project_name = project_name_from_note_path(path);
    let space_slug = space_slug_from_note_path(path);
    let content = content.trim();
    if content.is_empty() {
        return None;
    }

    let (fm, after_fm) = if let Some(rest) = content.strip_prefix("---") {
        let (fm_str, after) = rest.split_once("\n---")?;
        let fm: NoteFrontmatter = serde_yaml::from_str(fm_str.trim()).unwrap_or_default();
        (fm, after.trim_start_matches('\n').to_string())
    } else {
        (NoteFrontmatter::default(), content.to_string())
    };

    let note_id = fm.id.map(NoteId::from).unwrap_or_else(NoteId::new);

    let (title, body) = if let Some(line) = after_fm.lines().next() {
        if let Some(heading) = line.strip_prefix("# ") {
            let title = heading.trim().to_string();
            let body = after_fm[line.len()..]
                .trim_start_matches('\n')
                .trim_end_matches('\n')
                .to_string();
            (title, body)
        } else {
            let title = name_from_filename(path)?;
            (title, after_fm.trim_end_matches('\n').to_string())
        }
    } else {
        let title = name_from_filename(path)?;
        (title, String::new())
    };

    if title.is_empty() {
        return None;
    }

    Some(ParsedNote {
        note_id,
        project_name,
        space_slug,
        title,
        content: body,
        tags: fm.tags,
        created_at: fm.created.unwrap_or_default(),
        updated_at: fm.updated.unwrap_or_default(),
        pinned: fm.pinned,
        path: path.to_string(),
    })
}

#[cfg(test)]
mod path_tests {
    use super::*;

    #[test]
    fn project_name_extraction() {
        assert_eq!(
            project_name_from_note_path("runtime/projects/alpha/notes/foo.md"),
            Some("alpha".to_string())
        );
        assert_eq!(
            project_name_from_note_path("spaces/team/notes/foo.md"),
            None
        );
    }

    #[test]
    fn space_slug_extraction() {
        assert_eq!(
            space_slug_from_note_path("spaces/team/notes/foo.md"),
            Some("team".to_string())
        );
        assert_eq!(
            space_slug_from_note_path("runtime/projects/alpha/notes/foo.md"),
            None
        );
    }

    #[test]
    fn default_path_renders_each_tier() {
        assert_eq!(
            default_note_path("Hello World", None, Some("team")),
            "spaces/team/notes/hello-world.md"
        );
        assert_eq!(
            default_note_path("Hello World", Some("alpha"), None),
            "runtime/projects/alpha/notes/hello-world.md"
        );
    }

    #[test]
    fn parse_note_round_trip_preserves_id() {
        let id = NoteId::new();
        let rendered = render_note_markdown(
            id.into(),
            "My Note",
            "body content",
            &["t1".to_string()],
            "",
            "",
        );
        let parsed = parse_note_markdown(&rendered, "spaces/team/notes/my-note.md").unwrap();
        assert_eq!(parsed.note_id, id);
        assert_eq!(parsed.title, "My Note");
        assert_eq!(parsed.content, "body content");
        assert_eq!(parsed.space_slug.as_deref(), Some("team"));
        assert_eq!(parsed.path, "spaces/team/notes/my-note.md");
    }
}
