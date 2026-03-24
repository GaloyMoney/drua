use std::collections::{HashMap, HashSet};

use style_agent_core::review_session::{ConfirmData, FilterMode, ReviewSession, UndoData};
use style_agent_core::store::ReviewChunk;

/// Input mode for the TUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputMode {
    /// Normal navigation mode.
    Normal,
    /// Typing a custom label.
    AddLabel(String),
    /// Search/filter input.
    Search(String),
}

/// The main TUI application state.
///
/// Delegates all review logic to `ReviewSession`; keeps only UI-specific
/// state (cursor position, scroll offset, input mode).
pub struct App {
    /// Shared review state machine.
    pub session: ReviewSession,
    /// Selected label index in the right panel (TUI-only).
    pub label_cursor: usize,
    /// Scroll offset for code panel (TUI-only).
    pub code_scroll: u16,
    /// Current input mode (TUI-only).
    pub input_mode: InputMode,
    /// Whether the app should quit (TUI-only).
    pub should_quit: bool,
}

impl App {
    pub fn new(
        chunks: Vec<ReviewChunk>,
        saturated_labels: HashSet<String>,
        label_counts: HashMap<String, usize>,
        target_per_label: usize,
    ) -> Self {
        App {
            session: ReviewSession::new(chunks, saturated_labels, label_counts, target_per_label),
            label_cursor: 0,
            code_scroll: 0,
            input_mode: InputMode::Normal,
            should_quit: false,
        }
    }

    // ── Delegated queries ───────────────────────────────────────────

    pub fn current_chunk(&self) -> Option<&ReviewChunk> {
        self.session.current_chunk()
    }

    pub fn current_index(&self) -> usize {
        self.session.current_index()
    }

    pub fn filtered_count(&self) -> usize {
        self.session.filtered_count()
    }

    pub fn total_count(&self) -> usize {
        self.session.total_count()
    }

    pub fn total_reviewed(&self) -> usize {
        self.session.total_reviewed()
    }

    pub fn reviewed_this_session(&self) -> usize {
        self.session.reviewed_this_session()
    }

    pub fn filter_mode(&self) -> FilterMode {
        self.session.filter_mode()
    }

    pub fn label_list(&self) -> &[String] {
        self.session.label_list()
    }

    pub fn is_label_selected(&self, label: &str) -> bool {
        self.session.is_label_selected(label)
    }

    // ── Delegated actions (with TUI scroll reset) ───────────────────

    pub fn toggle_label(&mut self) {
        if let Some(label) = self.session.label_list().get(self.label_cursor).cloned() {
            self.session.toggle_label(&label);
        }
    }

    pub fn confirm_and_next(&mut self) -> Option<ConfirmData> {
        let data = self.session.confirm();
        self.code_scroll = 0;
        data
    }

    pub fn undo(&mut self) -> Option<UndoData> {
        let data = self.session.undo();
        self.code_scroll = 0;
        data
    }

    pub fn prev_chunk(&mut self) {
        self.session.prev();
        self.code_scroll = 0;
    }

    pub fn skip_chunk(&mut self) {
        self.session.skip();
        self.code_scroll = 0;
    }

    pub fn cycle_filter(&mut self) {
        self.session.cycle_filter();
    }

    pub fn apply_filter(&mut self) {
        self.session.clear_search();
    }

    pub fn apply_search(&mut self, query: &str) {
        self.session.apply_search(query);
    }

    pub fn add_custom_label(&mut self, label: String) {
        self.session.add_custom_label(label);
    }

    // ── TUI-only methods ────────────────────────────────────────────

    pub fn label_up(&mut self) {
        if self.label_cursor > 0 {
            self.label_cursor -= 1;
        }
    }

    pub fn label_down(&mut self) {
        if self.label_cursor + 1 < self.session.label_list().len() {
            self.label_cursor += 1;
        }
    }

    pub fn scroll_code_up(&mut self) {
        self.code_scroll = self.code_scroll.saturating_sub(3);
    }

    pub fn scroll_code_down(&mut self, max_lines: u16) {
        let content_lines = self
            .current_chunk()
            .map(|c| c.content.lines().count() as u16)
            .unwrap_or(0);
        if self.code_scroll + max_lines < content_lines + 2 {
            self.code_scroll += 3;
        }
    }
}
