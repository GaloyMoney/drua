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
