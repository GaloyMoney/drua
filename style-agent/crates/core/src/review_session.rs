//! Pure state machine for the labeling review workflow.
//!
//! Both the TUI (ratatui) and Telegram bot drive this same state machine.
//! It performs no I/O — callers persist the returned data structs.

use std::collections::{HashMap, HashSet};

use rand::seq::SliceRandom;
use rand::thread_rng;

use crate::label_store::{content_hash, AuditSuspect, ChunkKey, LabelRecord, LabelStore};
use crate::labeler::KNOWN_PRIMARY_LABELS;
use crate::store::{ReviewChunk, VectorStore};

/// Convert audit suspects into `ReviewChunk`s for the review state machine.
///
/// Each suspect becomes a review chunk with its current human label pre-selected
/// and a hint about what the ML classifier predicted.
pub fn review_chunks_from_suspects(suspects: &[AuditSuspect]) -> Vec<ReviewChunk> {
    suspects
        .iter()
        .enumerate()
        .map(|(i, s)| ReviewChunk {
            id: format!("suspect-{i}"),
            content: s.content.clone(),
            file_path: s.key.file_path.clone(),
            repo: s.key.repo.clone(),
            chunk_type: s.key.chunk_type.clone(),
            entity_name: if s.key.entity_name.is_empty() {
                None
            } else {
                Some(s.key.entity_name.clone())
            },
            language: String::new(),
            line_start: 0,
            line_end: 0,
            labels: vec![s.human_label.clone()],
            layer: None,
            uses: vec![],
            label_source: "human".to_string(),
            reviewed: false,
        })
        .collect()
}

/// Data returned when a chunk is confirmed, sufficient to build a `LabelRecord`.
pub struct ConfirmData {
    pub point_id: String,
    pub labels: Vec<String>,
    pub heuristic_labels: Vec<String>,
    pub content: String,
    pub chunk_key: ChunkKey,
}

/// Data returned when undoing a confirm.
pub struct UndoData {
    pub point_id: String,
    pub labels: Vec<String>,
    pub label_source: String,
}

/// Summary stats for a review session.
pub struct SessionSummary {
    pub reviewed: usize,
    pub skipped: usize,
    pub total_reviewed: usize,
    pub total_count: usize,
}

/// Filter mode for the chunk list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterMode {
    /// Show only unreviewed chunks (default).
    Unreviewed,
    /// Show all chunks.
    All,
    /// Show only chunks with heuristic labels (not yet human-reviewed).
    HeuristicOnly,
}

impl FilterMode {
    pub fn next(self) -> Self {
        match self {
            Self::Unreviewed => Self::All,
            Self::All => Self::HeuristicOnly,
            Self::HeuristicOnly => Self::Unreviewed,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Unreviewed => "unreviewed",
            Self::All => "all",
            Self::HeuristicOnly => "heuristic",
        }
    }
}

/// Entry saved on confirm so we can undo.
struct UndoEntry {
    chunk_index: usize,
    original_labels: Vec<String>,
    original_label_source: String,
}

/// Pure state machine for the review workflow.
///
/// No async, no I/O — callers drive the state and persist returned data.
/// Chunks are priority-weighted so underrepresented labels appear first.
pub struct ReviewSession {
    all_chunks: Vec<ReviewChunk>,
    filtered_indices: Vec<usize>,
    current_index: usize,
    selected_labels: HashSet<String>,
    label_list: Vec<String>,
    filter_mode: FilterMode,
    saturated_labels: HashSet<String>,
    /// Per-label human review counts (for priority weighting).
    label_counts: HashMap<String, usize>,
    /// Target reviews per label (chunks with labels farther from target appear first).
    target_per_label: usize,
    reviewed_this_session: usize,
    skipped_this_session: usize,
    undo_stack: Vec<UndoEntry>,
}

impl ReviewSession {
    /// Create a new session from chunks loaded from the store.
    ///
    /// `label_counts` maps each label to how many human reviews it already has.
    /// `target_per_label` is the target number of reviews per label.
    /// Chunks with underrepresented labels are shown first (priority-weighted shuffle).
    pub fn new(
        chunks: Vec<ReviewChunk>,
        saturated_labels: HashSet<String>,
        label_counts: HashMap<String, usize>,
        target_per_label: usize,
    ) -> Self {
        let mut label_list: Vec<String> =
            KNOWN_PRIMARY_LABELS.iter().map(|s| s.to_string()).collect();

        // Collect custom labels from chunks that aren't in the known list.
        for chunk in &chunks {
            for label in &chunk.labels {
                if !label_list.contains(label) {
                    label_list.push(label.clone());
                }
            }
        }

        let mut session = Self {
            all_chunks: chunks,
            filtered_indices: Vec::new(),
            current_index: 0,
            selected_labels: HashSet::new(),
            label_list,
            filter_mode: FilterMode::Unreviewed,
            saturated_labels,
            label_counts,
            target_per_label,
            reviewed_this_session: 0,
            skipped_this_session: 0,
            undo_stack: Vec::new(),
        };
        session.rebuild_filter();
        session.sync_selected_labels();
        session
    }

    // ── Queries (pure) ──────────────────────────────────────────────

    /// Get the currently visible chunk, if any.
    pub fn current_chunk(&self) -> Option<&ReviewChunk> {
        self.filtered_indices
            .get(self.current_index)
            .map(|&idx| &self.all_chunks[idx])
    }

    /// Current position within the filtered list.
    pub fn current_index(&self) -> usize {
        self.current_index
    }

    /// Number of chunks matching the current filter.
    pub fn filtered_count(&self) -> usize {
        self.filtered_indices.len()
    }

    /// Total number of chunks loaded.
    pub fn total_count(&self) -> usize {
        self.all_chunks.len()
    }

    /// Number of chunks marked as reviewed (across all chunks).
    pub fn total_reviewed(&self) -> usize {
        self.all_chunks.iter().filter(|c| c.reviewed).count()
    }

    /// Chunks confirmed this session.
    pub fn reviewed_this_session(&self) -> usize {
        self.reviewed_this_session
    }

    /// Chunks skipped this session.
    pub fn skipped_this_session(&self) -> usize {
        self.skipped_this_session
    }

    /// The currently selected labels for the current chunk.
    pub fn selected_labels(&self) -> &HashSet<String> {
        &self.selected_labels
    }

    /// Check whether a specific label is selected.
    pub fn is_label_selected(&self, label: &str) -> bool {
        self.selected_labels.contains(label)
    }

    /// The full label list (known + custom).
    pub fn label_list(&self) -> &[String] {
        &self.label_list
    }

    /// Current filter mode.
    pub fn filter_mode(&self) -> FilterMode {
        self.filter_mode
    }

    /// Whether there are no more chunks to review in the current filter.
    pub fn is_done(&self) -> bool {
        self.filtered_indices.is_empty()
    }

    /// Session summary stats.
    pub fn summary(&self) -> SessionSummary {
        SessionSummary {
            reviewed: self.reviewed_this_session,
            skipped: self.skipped_this_session,
            total_reviewed: self.total_reviewed(),
            total_count: self.total_count(),
        }
    }

    // ── Actions (mutate state) ──────────────────────────────────────

    /// Toggle a label (single-select radio semantics).
    ///
    /// If the label is already selected, deselect it. Otherwise clear all
    /// and select this one.
    pub fn toggle_label(&mut self, label: &str) {
        if self.selected_labels.contains(label) {
            self.selected_labels.remove(label);
        } else {
            self.selected_labels.clear();
            self.selected_labels.insert(label.to_string());
        }
    }

    /// Confirm the current chunk with the selected labels.
    ///
    /// Returns data sufficient to persist to both the database and JSONL,
    /// or `None` if there is no current chunk.
    pub fn confirm(&mut self) -> Option<ConfirmData> {
        let idx = *self.filtered_indices.get(self.current_index)?;
        let labels: Vec<String> = self.selected_labels.iter().cloned().collect();

        let chunk = &mut self.all_chunks[idx];
        self.undo_stack.push(UndoEntry {
            chunk_index: idx,
            original_labels: chunk.labels.clone(),
            original_label_source: chunk.label_source.clone(),
        });

        let data = ConfirmData {
            point_id: chunk.id.clone(),
            labels: labels.clone(),
            heuristic_labels: chunk.labels.clone(),
            content: chunk.content.clone(),
            chunk_key: chunk.chunk_key(),
        };

        chunk.labels = labels;
        chunk.label_source = "human".to_string();
        chunk.reviewed = true;
        self.reviewed_this_session += 1;

        // Advance position.
        if self.filter_mode == FilterMode::Unreviewed
            || self.filter_mode == FilterMode::HeuristicOnly
        {
            self.filtered_indices.remove(self.current_index);
            if !self.filtered_indices.is_empty() {
                self.current_index = self.current_index.min(self.filtered_indices.len() - 1);
            }
        } else if self.current_index + 1 < self.filtered_indices.len() {
            self.current_index += 1;
        }

        self.sync_selected_labels();
        Some(data)
    }

    /// Skip to the next chunk without confirming.
    pub fn skip(&mut self) {
        if self.current_index + 1 < self.filtered_indices.len() {
            self.current_index += 1;
            self.skipped_this_session += 1;
            self.sync_selected_labels();
        }
    }

    /// Go back to the previous chunk.
    pub fn prev(&mut self) {
        if self.current_index > 0 {
            self.current_index -= 1;
            self.sync_selected_labels();
        }
    }

    /// Undo the last confirm, restoring the chunk to its previous state.
    ///
    /// Returns data needed to revert in the database, or `None` if there's
    /// nothing to undo.
    pub fn undo(&mut self) -> Option<UndoData> {
        let entry = self.undo_stack.pop()?;
        let chunk = &mut self.all_chunks[entry.chunk_index];
        let data = UndoData {
            point_id: chunk.id.clone(),
            labels: entry.original_labels.clone(),
            label_source: entry.original_label_source.clone(),
        };
        chunk.labels = entry.original_labels;
        chunk.label_source = entry.original_label_source;
        chunk.reviewed = false;
        self.reviewed_this_session = self.reviewed_this_session.saturating_sub(1);

        // Re-apply filter so the chunk reappears, then navigate to it.
        self.rebuild_filter();
        if let Some(pos) = self
            .filtered_indices
            .iter()
            .position(|&i| i == entry.chunk_index)
        {
            self.current_index = pos;
        }
        self.sync_selected_labels();

        Some(data)
    }

    /// Cycle to the next filter mode and rebuild indices.
    pub fn cycle_filter(&mut self) {
        self.filter_mode = self.filter_mode.next();
        self.rebuild_filter();
        self.sync_selected_labels();
    }

    /// Apply a text search on top of the current filter mode.
    pub fn apply_search(&mut self, query: &str) {
        let query_lower = query.to_lowercase();
        self.filtered_indices = self
            .all_chunks
            .iter()
            .enumerate()
            .filter(|(_, chunk)| {
                let passes_base = match self.filter_mode {
                    FilterMode::All => true,
                    FilterMode::Unreviewed => !chunk.reviewed,
                    FilterMode::HeuristicOnly => {
                        chunk.label_source == "heuristic" && !chunk.reviewed
                    }
                };
                if !passes_base {
                    return false;
                }
                chunk
                    .labels
                    .iter()
                    .any(|l| l.to_lowercase().contains(&query_lower))
                    || chunk.chunk_type.to_lowercase().contains(&query_lower)
                    || chunk.repo.to_lowercase().contains(&query_lower)
                    || chunk.file_path.to_lowercase().contains(&query_lower)
                    || chunk
                        .entity_name
                        .as_ref()
                        .is_some_and(|n| n.to_lowercase().contains(&query_lower))
            })
            .map(|(i, _)| i)
            .collect();

        self.current_index = 0;
        self.sync_selected_labels();
    }

    /// Clear search and re-apply the base filter.
    pub fn clear_search(&mut self) {
        self.rebuild_filter();
        self.sync_selected_labels();
    }

    /// Add a custom label (auto-selects it).
    pub fn add_custom_label(&mut self, label: String) {
        if !label.is_empty() && !self.label_list.contains(&label) {
            self.label_list.push(label.clone());
            // Auto-select the newly added label (single-select: clear first)
            self.selected_labels.clear();
            self.selected_labels.insert(label);
        }
    }

    // ── Internal ────────────────────────────────────────────────────

    /// Rebuild `filtered_indices` from the current filter mode.
    ///
    /// After filtering, indices are sorted by priority weight (underrepresented
    /// labels first) with random tiebreaking within the same weight tier.
    fn rebuild_filter(&mut self) {
        self.filtered_indices = self
            .all_chunks
            .iter()
            .enumerate()
            .filter(|(_, chunk)| {
                // Skip chunks whose heuristic label is saturated.
                if !self.saturated_labels.is_empty()
                    && chunk
                        .labels
                        .iter()
                        .any(|l| self.saturated_labels.contains(l))
                {
                    return false;
                }
                match self.filter_mode {
                    FilterMode::All => true,
                    FilterMode::Unreviewed => !chunk.reviewed,
                    FilterMode::HeuristicOnly => {
                        chunk.label_source == "heuristic" && !chunk.reviewed
                    }
                }
            })
            .map(|(i, _)| i)
            .collect();

        // Shuffle first, then stable-sort by descending weight so that
        // underrepresented labels float to the top with random order within tiers.
        self.filtered_indices.shuffle(&mut thread_rng());

        // Pre-compute weights to avoid borrow conflicts with sort_by.
        let weights: HashMap<usize, usize> = self
            .filtered_indices
            .iter()
            .map(|&idx| {
                let weight = self.all_chunks[idx]
                    .labels
                    .first()
                    .map(|label| {
                        let count = self.label_counts.get(label).copied().unwrap_or(0);
                        self.target_per_label.saturating_sub(count)
                    })
                    .unwrap_or(0);
                (idx, weight)
            })
            .collect();

        self.filtered_indices.sort_by(|a, b| {
            let wa = weights.get(a).copied().unwrap_or(0);
            let wb = weights.get(b).copied().unwrap_or(0);
            wb.cmp(&wa) // descending: higher weight first
        });

        if !self.filtered_indices.is_empty() {
            self.current_index = self.current_index.min(self.filtered_indices.len() - 1);
        } else {
            self.current_index = 0;
        }
    }

    /// Sync `selected_labels` with the current chunk's labels.
    fn sync_selected_labels(&mut self) {
        let labels: Vec<String> = self
            .current_chunk()
            .map(|c| c.labels.clone())
            .unwrap_or_default();
        self.selected_labels.clear();
        for label in labels {
            self.selected_labels.insert(label);
        }
    }
}

// ── Persistence helpers ──────────────────────────────────────────────

/// Persist a confirmed review to both the database and the JSONL label store.
pub fn persist_confirm(
    store: &VectorStore,
    label_store: &LabelStore,
    data: &ConfirmData,
) -> anyhow::Result<()> {
    store.set_chunk_labels(&data.point_id, &data.labels, "human", true)?;

    label_store.append(&LabelRecord {
        key: data.chunk_key.clone(),
        content_hash: content_hash(&data.content),
        content: data.content.clone(),
        labels: data.labels.clone(),
        heuristic_labels: data.heuristic_labels.clone(),
        label_source: "human".to_string(),
        reviewed_at: now_iso8601(),
    })?;

    Ok(())
}

/// Persist an undo — revert labels in the database and remove the last JSONL line.
pub fn persist_undo(
    store: &VectorStore,
    label_store: &LabelStore,
    data: &UndoData,
) -> anyhow::Result<()> {
    store.set_chunk_labels(&data.point_id, &data.labels, &data.label_source, false)?;

    label_store.pop_last()?;

    Ok(())
}

/// Persist a confirmed re-review to the JSONL label store only (no DB write).
///
/// Used by `--confused` mode where chunks come from the audit suspects file
/// rather than from the database.
pub fn persist_confirm_label_only(
    label_store: &LabelStore,
    data: &ConfirmData,
) -> anyhow::Result<()> {
    label_store.append(&LabelRecord {
        key: data.chunk_key.clone(),
        content_hash: content_hash(&data.content),
        content: data.content.clone(),
        labels: data.labels.clone(),
        heuristic_labels: data.heuristic_labels.clone(),
        label_source: "human".to_string(),
        reviewed_at: now_iso8601(),
    })?;
    Ok(())
}

/// Undo a label-only persist — just remove the last JSONL line (no DB revert).
pub fn persist_undo_label_only(label_store: &LabelStore) -> anyhow::Result<()> {
    label_store.pop_last()?;
    Ok(())
}

/// Compute per-label human review counts from the label store.
pub fn compute_label_counts(label_store: &LabelStore) -> HashMap<String, usize> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    if let Ok(all) = label_store.load_all() {
        for record in all.values() {
            for l in &record.labels {
                *counts.entry(l.clone()).or_insert(0) += 1;
            }
        }
    }
    counts
}

/// Compute which labels have reached the minimum review target.
pub fn compute_saturated_labels(label_store: &LabelStore, min_per_label: usize) -> HashSet<String> {
    compute_label_counts(label_store)
        .into_iter()
        .filter(|(_, count)| *count >= min_per_label)
        .map(|(label, _)| label)
        .collect()
}

/// Current time as ISO 8601 UTC string.
pub fn now_iso8601() -> String {
    use std::time::SystemTime;
    let dur = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    let (days, rem) = (secs / 86400, secs % 86400);
    let (hours, rem) = (rem / 3600, rem % 3600);
    let (mins, s) = (rem / 60, rem % 60);
    let (y, m, d) = days_to_ymd(days);
    format!("{y:04}-{m:02}-{d:02}T{hours:02}:{mins:02}:{s:02}Z")
}

fn days_to_ymd(mut days: u64) -> (u64, u64, u64) {
    let mut y = 1970;
    loop {
        let year_days = if is_leap(y) { 366 } else { 365 };
        if days < year_days {
            break;
        }
        days -= year_days;
        y += 1;
    }
    let month_days: [u64; 12] = if is_leap(y) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut m = 0;
    for md in &month_days {
        if days < *md {
            break;
        }
        days -= *md;
        m += 1;
    }
    (y, m + 1, days + 1)
}

fn is_leap(y: u64) -> bool {
    (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_chunk(id: &str, reviewed: bool, label_source: &str, labels: Vec<&str>) -> ReviewChunk {
        ReviewChunk {
            id: id.to_string(),
            content: format!("fn {id}() {{}}"),
            file_path: format!("src/{id}.rs"),
            repo: "test-repo".to_string(),
            chunk_type: "function".to_string(),
            entity_name: Some(id.to_string()),
            language: "rust".to_string(),
            line_start: 1,
            line_end: 1,
            labels: labels.into_iter().map(String::from).collect(),
            layer: None,
            uses: vec![],
            label_source: label_source.to_string(),
            reviewed,
        }
    }

    #[test]
    fn new_session_filters_unreviewed() {
        let chunks = vec![
            make_chunk("a", false, "heuristic", vec!["entity"]),
            make_chunk("b", true, "human", vec!["service"]),
            make_chunk("c", false, "", vec![]),
        ];
        let session = ReviewSession::new(chunks, HashSet::new(), HashMap::new(), 50);
        // Default filter is Unreviewed — should show a and c.
        assert_eq!(session.filtered_count(), 2);
        assert_eq!(session.total_count(), 3);
    }

    #[test]
    fn toggle_label_single_select() {
        let chunks = vec![make_chunk("a", false, "", vec![])];
        let mut session = ReviewSession::new(chunks, HashSet::new(), HashMap::new(), 50);

        session.toggle_label("entity");
        assert!(session.is_label_selected("entity"));

        // Selecting another clears the first.
        session.toggle_label("service");
        assert!(!session.is_label_selected("entity"));
        assert!(session.is_label_selected("service"));

        // Toggling same label deselects.
        session.toggle_label("service");
        assert!(!session.is_label_selected("service"));
        assert!(session.selected_labels().is_empty());
    }

    #[test]
    fn confirm_advances_and_returns_data() {
        let chunks = vec![
            make_chunk("a", false, "heuristic", vec!["entity"]),
            make_chunk("b", false, "", vec![]),
        ];
        let mut session = ReviewSession::new(chunks, HashSet::new(), HashMap::new(), 50);

        session.toggle_label("service");
        let data = session.confirm().unwrap();
        assert_eq!(data.point_id, "a");
        assert_eq!(data.labels, vec!["service".to_string()]);
        assert_eq!(data.heuristic_labels, vec!["entity".to_string()]);

        // After confirm in Unreviewed mode, "a" is removed from filtered.
        assert_eq!(session.filtered_count(), 1);
        assert_eq!(session.current_chunk().unwrap().id, "b");
        assert_eq!(session.reviewed_this_session(), 1);
    }

    #[test]
    fn undo_restores_chunk() {
        let chunks = vec![
            make_chunk("a", false, "heuristic", vec!["entity"]),
            make_chunk("b", false, "", vec![]),
        ];
        let mut session = ReviewSession::new(chunks, HashSet::new(), HashMap::new(), 50);

        session.toggle_label("service");
        session.confirm();

        let undo_data = session.undo().unwrap();
        assert_eq!(undo_data.point_id, "a");
        assert_eq!(undo_data.labels, vec!["entity".to_string()]);
        assert_eq!(undo_data.label_source, "heuristic");

        // Chunk reappears in filtered list.
        assert_eq!(session.filtered_count(), 2);
        assert_eq!(session.reviewed_this_session(), 0);
    }

    #[test]
    fn skip_advances_without_saving() {
        let chunks = vec![
            make_chunk("a", false, "heuristic", vec!["entity"]),
            make_chunk("b", false, "heuristic", vec!["service"]),
        ];
        // Give "entity" more weight (fewer reviews) so "a" sorts first.
        let mut counts = HashMap::new();
        counts.insert("entity".to_string(), 0);
        counts.insert("service".to_string(), 40);
        let mut session = ReviewSession::new(chunks, HashSet::new(), counts, 50);

        let first_id = session.current_chunk().unwrap().id.clone();
        session.skip();
        assert_eq!(session.current_index(), 1);
        assert_eq!(session.skipped_this_session(), 1);
        // After skip, we should be on a different chunk.
        assert_ne!(session.current_chunk().unwrap().id, first_id);
    }

    #[test]
    fn cycle_filter() {
        let chunks = vec![
            make_chunk("a", false, "heuristic", vec!["entity"]),
            make_chunk("b", true, "human", vec!["service"]),
        ];
        let mut session = ReviewSession::new(chunks, HashSet::new(), HashMap::new(), 50);

        assert_eq!(session.filter_mode(), FilterMode::Unreviewed);
        assert_eq!(session.filtered_count(), 1);

        session.cycle_filter();
        assert_eq!(session.filter_mode(), FilterMode::All);
        assert_eq!(session.filtered_count(), 2);

        session.cycle_filter();
        assert_eq!(session.filter_mode(), FilterMode::HeuristicOnly);
        assert_eq!(session.filtered_count(), 1);
    }

    #[test]
    fn add_custom_label() {
        let chunks = vec![make_chunk("a", false, "", vec![])];
        let mut session = ReviewSession::new(chunks, HashSet::new(), HashMap::new(), 50);

        let initial_len = session.label_list().len();
        session.add_custom_label("my_custom".to_string());
        assert_eq!(session.label_list().len(), initial_len + 1);
        assert!(session.is_label_selected("my_custom"));
    }

    #[test]
    fn now_iso8601_format() {
        let ts = now_iso8601();
        // Should look like 2026-03-19T...Z
        assert!(ts.starts_with("20"), "timestamp: {ts}");
        assert!(ts.ends_with('Z'), "timestamp: {ts}");
        assert_eq!(ts.len(), 20, "timestamp: {ts}");
    }
}
