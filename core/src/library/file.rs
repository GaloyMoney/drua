pub enum RuntimeFile<'a> {
    Note {
        workspace_name: &'a str,
        slug: &'a str,
        id_prefix: &'a str,
        content: &'a str,
    },
}

impl RuntimeFile<'_> {
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
}
