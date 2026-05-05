use std::io;
use std::path::PathBuf;

use code_assistant_core::anti_pattern_triage::{
    append_discarded, append_example, build_queue, default_catalog_path, default_discarded_path,
    default_examples_path, load_patterns, make_discarded, make_example, save_patterns, NewExample,
    Pattern, QueueLoadStats, TriageEntry, SOURCE_INLINE_REVIEW,
};
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
    Frame, Terminal,
};

use crate::config::Config;

pub fn run(config: &Config) -> anyhow::Result<()> {
    let labels_dir = config.labels_dir();
    let catalog_path = default_catalog_path(&labels_dir);
    let examples_path = default_examples_path(&labels_dir);
    let discarded_path = default_discarded_path(&labels_dir);
    let inline_path = resolve_inline_reviews_path();

    eprintln!("Loading inline reviews from {}", inline_path.display());
    let stats = build_queue(&inline_path, &examples_path, &discarded_path)?;
    let patterns = load_patterns(&catalog_path)?;

    print_startup_summary(&stats, patterns.len());

    if stats.entries.is_empty() {
        eprintln!("No entries to triage. Exiting.");
        return Ok(());
    }

    let mut app = App::new(stats.entries, patterns, catalog_path, examples_path);
    app.discarded_path = discarded_path;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_event_loop(&mut terminal, &mut app);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result?;

    println!("\n--- Triage Session Summary ---");
    println!("Discarded:        {}", app.session_discarded);
    println!("Attached:         {}", app.session_attached);
    println!("New patterns:     {}", app.session_new_patterns);
    println!("Skipped:          {}", app.session_skipped);
    println!("Remaining:        {}", app.remaining());
    println!();

    Ok(())
}

fn resolve_inline_reviews_path() -> PathBuf {
    if let Ok(p) = std::env::var("INLINE_REVIEWS_PATH") {
        return PathBuf::from(p);
    }
    let base = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join(".claude/data/inline-reviews.jsonl")
}

fn print_startup_summary(stats: &QueueLoadStats, pattern_count: usize) {
    eprintln!(
        "Patterns: {} | Total inline reviews: {} | Already triaged: {} ({} discarded, {} examples) | Remaining: {}",
        pattern_count,
        stats.total,
        stats.already_triaged,
        stats.discarded_count,
        stats.example_count,
        stats.entries.len(),
    );
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode {
    Normal,
    Help,
    /// Picking a slug for attach. `pending` is empty before the primary is chosen.
    SlugPicker {
        query: String,
        cursor: usize,
        pending: AttachPending,
    },
    /// Sub-prompt between attaches: a / n / enter.
    AttachAnother(AttachPending),
    NewSlug {
        text: String,
        pending: AttachPending,
    },
    NewCategory {
        slug: String,
        text: String,
        pending: AttachPending,
    },
    NewComment(NewCommentState),
}

/// Slugs accumulated for the in-progress attach. Empty until the primary is chosen.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct AttachPending {
    primary: Option<String>,
    additional: Vec<String>,
}

impl AttachPending {
    fn add(&mut self, slug: String) {
        if self.primary.is_none() {
            self.primary = Some(slug);
        } else if !self.additional.contains(&slug) && Some(&slug) != self.primary.as_ref() {
            self.additional.push(slug);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NewCommentState {
    slug: String,
    category: String,
    text: String,
    pending: AttachPending,
}

struct App {
    entries: Vec<TriageEntry>,
    cursor: usize,
    patterns: Vec<Pattern>,
    catalog_path: PathBuf,
    examples_path: PathBuf,
    discarded_path: PathBuf,
    mode: Mode,
    should_quit: bool,
    code_scroll: u16,
    session_discarded: usize,
    session_attached: usize,
    session_new_patterns: usize,
    session_skipped: usize,
    error: Option<String>,
}

impl App {
    fn new(
        entries: Vec<TriageEntry>,
        patterns: Vec<Pattern>,
        catalog_path: PathBuf,
        examples_path: PathBuf,
    ) -> Self {
        Self {
            entries,
            cursor: 0,
            patterns,
            catalog_path,
            examples_path,
            discarded_path: PathBuf::new(),
            mode: Mode::Normal,
            should_quit: false,
            code_scroll: 0,
            session_discarded: 0,
            session_attached: 0,
            session_new_patterns: 0,
            session_skipped: 0,
            error: None,
        }
    }

    fn current(&self) -> Option<&TriageEntry> {
        self.entries.get(self.cursor)
    }

    fn advance(&mut self) {
        self.cursor += 1;
        self.code_scroll = 0;
        if self.cursor >= self.entries.len() {
            self.should_quit = true;
        }
    }

    fn remaining(&self) -> usize {
        self.entries.len().saturating_sub(self.cursor)
    }

    fn discard_current(&mut self) -> anyhow::Result<()> {
        let Some(entry) = self.current() else {
            return Ok(());
        };
        let reason = entry.guess.label().to_string();
        let record = make_discarded(SOURCE_INLINE_REVIEW, &entry.source_id, &reason);
        append_discarded(&self.discarded_path, &record)?;
        self.session_discarded += 1;
        self.advance();
        Ok(())
    }

    fn finalize_attach(&mut self, pending: AttachPending) -> anyhow::Result<()> {
        let Some(primary) = pending.primary else {
            return Ok(());
        };
        let Some(entry) = self.current() else {
            return Ok(());
        };
        let example = make_example(NewExample {
            primary_slug: &primary,
            additional_slugs: pending.additional,
            code: &entry.review.context,
            source: SOURCE_INLINE_REVIEW,
            source_id: Some(entry.source_id.clone()),
            original_comment: &entry.review.comment,
            file: &entry.review.file,
            repo: &entry.review.repo,
        });
        append_example(&self.examples_path, &example)?;
        self.session_attached += 1;
        self.advance();
        Ok(())
    }

    fn create_pattern(
        &mut self,
        slug: String,
        category: String,
        canonical_comment: String,
    ) -> anyhow::Result<()> {
        let pattern = Pattern {
            slug,
            category,
            canonical_comment,
            active: true,
        };
        self.patterns.push(pattern);
        save_patterns(&self.catalog_path, &self.patterns)?;
        self.session_new_patterns += 1;
        Ok(())
    }
}

fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> anyhow::Result<()> {
    loop {
        terminal.draw(|frame| draw(frame, app))?;
        if app.should_quit {
            break;
        }
        if event::poll(std::time::Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                handle_key(key.code, key.modifiers, app)?;
            }
        }
    }
    Ok(())
}

fn handle_key(code: KeyCode, mods: KeyModifiers, app: &mut App) -> anyhow::Result<()> {
    if mods.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c') {
        app.should_quit = true;
        return Ok(());
    }

    match app.mode.clone() {
        Mode::Normal => handle_normal(code, mods, app),
        Mode::Help => {
            app.mode = Mode::Normal;
            Ok(())
        }
        Mode::SlugPicker {
            query,
            cursor,
            pending,
        } => handle_slug_picker(code, query, cursor, pending, app),
        Mode::AttachAnother(pending) => handle_attach_another(code, pending, app),
        Mode::NewSlug { text, pending } => handle_new_slug(code, text, pending, app),
        Mode::NewCategory {
            slug,
            text,
            pending,
        } => handle_new_category(code, slug, text, pending, app),
        Mode::NewComment(state) => handle_new_comment(code, state, app),
    }
}

fn handle_normal(code: KeyCode, mods: KeyModifiers, app: &mut App) -> anyhow::Result<()> {
    app.error = None;
    match code {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Char('?') => app.mode = Mode::Help,
        KeyCode::Char('d') if mods.contains(KeyModifiers::CONTROL) => {
            app.code_scroll = app.code_scroll.saturating_add(10);
        }
        KeyCode::Char('u') if mods.contains(KeyModifiers::CONTROL) => {
            app.code_scroll = app.code_scroll.saturating_sub(10);
        }
        KeyCode::Char('d') => {
            if let Err(e) = app.discard_current() {
                app.error = Some(format!("discard failed: {e}"));
            }
        }
        KeyCode::Char('s') => {
            app.session_skipped += 1;
            app.advance();
        }
        KeyCode::Char('a') => {
            if app.patterns.is_empty() {
                app.error = Some("No patterns yet. Press 'n' to create the first one.".to_string());
            } else {
                app.mode = Mode::SlugPicker {
                    query: String::new(),
                    cursor: 0,
                    pending: AttachPending::default(),
                };
            }
        }
        KeyCode::Char('n') => {
            app.mode = Mode::NewSlug {
                text: String::new(),
                pending: AttachPending::default(),
            };
        }
        KeyCode::Char('J') => app.code_scroll = app.code_scroll.saturating_add(3),
        KeyCode::Char('K') => app.code_scroll = app.code_scroll.saturating_sub(3),
        _ => {}
    }
    Ok(())
}

fn handle_slug_picker(
    code: KeyCode,
    query: String,
    cursor: usize,
    pending: AttachPending,
    app: &mut App,
) -> anyhow::Result<()> {
    let matches = filter_patterns_excluding(&app.patterns, &query, &pending);
    let len = matches.len();
    // Clamp cursor in case the visible list shrank since the last keystroke.
    let cursor = if len == 0 { 0 } else { cursor.min(len - 1) };

    match code {
        KeyCode::Esc => app.mode = Mode::Normal,
        KeyCode::Enter => {
            if let Some(slug) = matches.get(cursor).map(|p| p.slug.clone()) {
                let mut next = pending;
                next.add(slug);
                app.mode = Mode::AttachAnother(next);
            }
        }
        KeyCode::Up => {
            app.mode = Mode::SlugPicker {
                query,
                cursor: cursor.saturating_sub(1),
                pending,
            };
        }
        KeyCode::Down => {
            let new_cursor = if len == 0 {
                0
            } else {
                (cursor + 1).min(len.saturating_sub(1))
            };
            app.mode = Mode::SlugPicker {
                query,
                cursor: new_cursor,
                pending,
            };
        }
        KeyCode::Backspace => {
            let mut t = query;
            t.pop();
            app.mode = Mode::SlugPicker {
                query: t,
                cursor: 0,
                pending,
            };
        }
        KeyCode::Char(c) => {
            let mut t = query;
            t.push(c);
            app.mode = Mode::SlugPicker {
                query: t,
                cursor: 0,
                pending,
            };
        }
        _ => {}
    }
    Ok(())
}

fn handle_attach_another(
    code: KeyCode,
    pending: AttachPending,
    app: &mut App,
) -> anyhow::Result<()> {
    match code {
        KeyCode::Esc => app.mode = Mode::Normal,
        KeyCode::Enter => {
            app.mode = Mode::Normal;
            if let Err(e) = app.finalize_attach(pending) {
                app.error = Some(format!("attach failed: {e}"));
            }
        }
        KeyCode::Char('a') => {
            app.mode = Mode::SlugPicker {
                query: String::new(),
                cursor: 0,
                pending,
            };
        }
        KeyCode::Char('n') => {
            app.mode = Mode::NewSlug {
                text: String::new(),
                pending,
            };
        }
        _ => {}
    }
    Ok(())
}

fn handle_new_slug(
    code: KeyCode,
    text: String,
    pending: AttachPending,
    app: &mut App,
) -> anyhow::Result<()> {
    match code {
        KeyCode::Esc => app.mode = Mode::Normal,
        KeyCode::Enter => {
            let slug = sanitize_slug(&text);
            if slug.is_empty() {
                app.error = Some("Slug cannot be empty.".to_string());
                app.mode = Mode::Normal;
            } else if app.patterns.iter().any(|p| p.slug == slug) {
                app.error = Some(format!("Slug '{slug}' already exists."));
                app.mode = Mode::Normal;
            } else {
                app.mode = Mode::NewCategory {
                    slug,
                    text: String::new(),
                    pending,
                };
            }
        }
        KeyCode::Backspace => {
            let mut t = text;
            t.pop();
            app.mode = Mode::NewSlug { text: t, pending };
        }
        KeyCode::Char(c) => {
            let mut t = text;
            t.push(c);
            app.mode = Mode::NewSlug { text: t, pending };
        }
        _ => {}
    }
    Ok(())
}

fn handle_new_category(
    code: KeyCode,
    slug: String,
    text: String,
    pending: AttachPending,
    app: &mut App,
) -> anyhow::Result<()> {
    match code {
        KeyCode::Esc => app.mode = Mode::Normal,
        KeyCode::Enter => {
            let category = text.trim().to_string();
            if category.is_empty() {
                app.error = Some("Category cannot be empty.".to_string());
                app.mode = Mode::Normal;
            } else {
                app.mode = Mode::NewComment(NewCommentState {
                    slug,
                    category,
                    text: String::new(),
                    pending,
                });
            }
        }
        KeyCode::Backspace => {
            let mut t = text;
            t.pop();
            app.mode = Mode::NewCategory {
                slug,
                text: t,
                pending,
            };
        }
        KeyCode::Char(c) => {
            let mut t = text;
            t.push(c);
            app.mode = Mode::NewCategory {
                slug,
                text: t,
                pending,
            };
        }
        _ => {}
    }
    Ok(())
}

fn handle_new_comment(code: KeyCode, state: NewCommentState, app: &mut App) -> anyhow::Result<()> {
    match code {
        KeyCode::Esc => app.mode = Mode::Normal,
        KeyCode::Tab => {
            let comment = state.text.trim().to_string();
            if comment.is_empty() {
                app.error = Some("Canonical comment cannot be empty.".to_string());
                app.mode = Mode::Normal;
            } else if let Err(e) = app.create_pattern(state.slug.clone(), state.category, comment) {
                app.error = Some(format!("create pattern failed: {e}"));
                app.mode = Mode::Normal;
            } else {
                let mut next = state.pending;
                next.add(state.slug);
                app.mode = Mode::AttachAnother(next);
            }
        }
        KeyCode::Enter => {
            let mut t = state.text;
            t.push('\n');
            app.mode = Mode::NewComment(NewCommentState { text: t, ..state });
        }
        KeyCode::Backspace => {
            let mut t = state.text;
            t.pop();
            app.mode = Mode::NewComment(NewCommentState { text: t, ..state });
        }
        KeyCode::Char(c) => {
            let mut t = state.text;
            t.push(c);
            app.mode = Mode::NewComment(NewCommentState { text: t, ..state });
        }
        _ => {}
    }
    Ok(())
}

fn sanitize_slug(s: &str) -> String {
    s.trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

fn filter_patterns<'a>(patterns: &'a [Pattern], query: &str) -> Vec<&'a Pattern> {
    let q = query.to_lowercase();
    patterns
        .iter()
        .filter(|p| {
            q.is_empty()
                || p.slug.to_lowercase().contains(&q)
                || p.category.to_lowercase().contains(&q)
        })
        .collect()
}

fn filter_patterns_excluding<'a>(
    patterns: &'a [Pattern],
    query: &str,
    pending: &AttachPending,
) -> Vec<&'a Pattern> {
    filter_patterns(patterns, query)
        .into_iter()
        .filter(|p| {
            Some(&p.slug) != pending.primary.as_ref() && !pending.additional.contains(&p.slug)
        })
        .collect()
}

// ── Rendering ──────────────────────────────────────────────────────────

fn draw(frame: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(2)])
        .split(frame.area());

    let main = chunks[0];
    let status = chunks[1];

    let panes = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6),
            Constraint::Min(1),
            Constraint::Length(5),
        ])
        .split(main);

    draw_comment_pane(frame, app, panes[0]);
    draw_context_pane(frame, app, panes[1]);
    draw_meta_pane(frame, app, panes[2]);
    draw_status_bar(frame, app, status);

    match &app.mode {
        Mode::Help => draw_help_popup(frame),
        Mode::SlugPicker {
            query,
            cursor,
            pending,
        } => draw_slug_picker(frame, app, query, *cursor, pending),
        Mode::AttachAnother(pending) => draw_attach_another(frame, pending),
        Mode::NewSlug { text, .. } => {
            draw_input_popup(frame, "New pattern slug (kebab-case):", text)
        }
        Mode::NewCategory { text, .. } => draw_input_popup(frame, "Category:", text),
        Mode::NewComment(state) => draw_multiline_popup(
            frame,
            "Canonical comment (Tab to submit, Esc to cancel):",
            &state.text,
        ),
        Mode::Normal => {}
    }
}

fn draw_comment_pane(frame: &mut Frame, app: &App, area: Rect) {
    let title = match app.current() {
        Some(_) => "Original comment",
        None => "Original comment (queue empty)",
    };
    let text = app
        .current()
        .map(|e| e.review.comment.as_str())
        .unwrap_or("");
    let para = Paragraph::new(text)
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false });
    frame.render_widget(para, area);
}

fn draw_context_pane(frame: &mut Frame, app: &App, area: Rect) {
    let entry = match app.current() {
        Some(e) => e,
        None => {
            let p = Paragraph::new("").block(Block::default().borders(Borders::ALL).title("Code"));
            frame.render_widget(p, area);
            return;
        }
    };

    let para = Paragraph::new(entry.review.context.as_str())
        .block(Block::default().borders(Borders::ALL).title("Code context"))
        .scroll((app.code_scroll, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(para, area);
}

fn draw_meta_pane(frame: &mut Frame, app: &App, area: Rect) {
    let entry = match app.current() {
        Some(e) => e,
        None => {
            let p =
                Paragraph::new("").block(Block::default().borders(Borders::ALL).title("Metadata"));
            frame.render_widget(p, area);
            return;
        }
    };

    let guess_color = match entry.guess {
        code_assistant_core::anti_pattern_triage::HeuristicGuess::StyleLikely => Color::Green,
        code_assistant_core::anti_pattern_triage::HeuristicGuess::LogicLikely => Color::Red,
        code_assistant_core::anti_pattern_triage::HeuristicGuess::Unsure => Color::Yellow,
    };

    let lines = vec![
        Line::from(vec![
            Span::styled(" repo:  ", Style::default().fg(Color::Cyan)),
            Span::raw(&entry.review.repo),
        ]),
        Line::from(vec![
            Span::styled(" file:  ", Style::default().fg(Color::Cyan)),
            Span::raw(format!("{}:{}", entry.review.file, entry.review.line)),
        ]),
        Line::from(vec![
            Span::styled(" guess: ", Style::default().fg(Color::Cyan)),
            Span::styled(entry.guess.label(), Style::default().fg(guess_color)),
        ]),
    ];
    let para =
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("Metadata"));
    frame.render_widget(para, area);
}

fn draw_status_bar(frame: &mut Frame, app: &App, area: Rect) {
    let pos = if app.entries.is_empty() {
        0
    } else {
        app.cursor + 1
    };
    let progress = format!(
        " {}/{} | discarded: {} | attached: {} | new: {} | skipped: {} | patterns: {} ",
        pos.min(app.entries.len()),
        app.entries.len(),
        app.session_discarded,
        app.session_attached,
        app.session_new_patterns,
        app.session_skipped,
        app.patterns.len(),
    );
    let keys = " d:discard  a:attach  n:new pattern  s:skip  q:quit  ?:help ";

    let line1 = Line::from(Span::styled(
        progress,
        Style::default().fg(Color::Black).bg(Color::Cyan),
    ));
    let line2 = if let Some(err) = &app.error {
        Line::from(Span::styled(
            format!(" {err} "),
            Style::default().fg(Color::White).bg(Color::Red),
        ))
    } else {
        Line::from(Span::styled(keys, Style::default().fg(Color::DarkGray)))
    };

    let para = Paragraph::new(vec![line1, line2]);
    frame.render_widget(para, area);
}

fn draw_help_popup(frame: &mut Frame) {
    let area = centered_rect(70, 60, frame.area());
    let lines = vec![
        Line::from("Anti-pattern triage TUI"),
        Line::from(""),
        Line::from("  d  discard current entry"),
        Line::from("  a  attach to existing pattern (fuzzy picker)"),
        Line::from("  n  create new pattern from this entry"),
        Line::from("  s  skip (no persist)"),
        Line::from("  q  quit"),
        Line::from("  ?  this help"),
        Line::from(""),
        Line::from("  J/K        scroll code context"),
        Line::from("  Ctrl-d/u   page-scroll code context"),
        Line::from(""),
        Line::from("  Press any key to dismiss"),
    ];
    let para = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Help")
            .style(Style::default().bg(Color::DarkGray).fg(Color::White)),
    );
    frame.render_widget(para, area);
}

fn draw_slug_picker(
    frame: &mut Frame,
    app: &App,
    query: &str,
    cursor: usize,
    pending: &AttachPending,
) {
    let area = centered_rect(70, 60, frame.area());
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(1),
        ])
        .split(area);

    let title = if pending.primary.is_none() {
        "Attach to primary pattern (fuzzy, Esc to cancel)"
    } else {
        "Attach additional pattern (fuzzy, Esc to cancel)"
    };
    let prompt = Paragraph::new(Line::from(vec![
        Span::raw(query.to_string()),
        Span::styled("_", Style::default().fg(Color::Yellow)),
    ]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .style(Style::default().bg(Color::DarkGray).fg(Color::White)),
    );
    frame.render_widget(prompt, chunks[0]);

    let pending_para = Paragraph::new(Line::from(pending_summary_spans(pending))).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Selected so far")
            .style(Style::default().bg(Color::DarkGray).fg(Color::White)),
    );
    frame.render_widget(pending_para, chunks[1]);

    let matches = filter_patterns_excluding(&app.patterns, query, pending);
    let items: Vec<ListItem> = matches
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let prefix = if i == cursor { ">" } else { " " };
            let style = if i == cursor {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            ListItem::new(Line::from(Span::styled(
                format!(
                    " {} {} [{}]  {}",
                    prefix, p.slug, p.category, p.canonical_comment
                ),
                style,
            )))
        })
        .collect();

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!("{} matches", matches.len()))
            .style(Style::default().bg(Color::DarkGray).fg(Color::White)),
    );
    frame.render_widget(list, chunks[2]);
}

fn draw_attach_another(frame: &mut Frame, pending: &AttachPending) {
    let area = centered_rect(60, 30, frame.area());
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(1)])
        .split(area);

    let pending_para = Paragraph::new(Line::from(pending_summary_spans(pending))).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Selected so far")
            .style(Style::default().bg(Color::DarkGray).fg(Color::White)),
    );
    frame.render_widget(pending_para, chunks[0]);

    let lines = vec![
        Line::from(""),
        Line::from("  a      attach another existing pattern"),
        Line::from("  n      create a new pattern and attach"),
        Line::from("  Enter  done — save example and advance"),
        Line::from("  Esc    cancel (no save)"),
    ];
    let para = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Attach another?")
            .style(Style::default().bg(Color::DarkGray).fg(Color::White)),
    );
    frame.render_widget(para, chunks[1]);
}

fn pending_summary_spans(pending: &AttachPending) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    spans.push(Span::styled(" primary: ", Style::default().fg(Color::Cyan)));
    match &pending.primary {
        Some(p) => spans.push(Span::styled(
            p.clone(),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )),
        None => spans.push(Span::styled("(none)", Style::default().fg(Color::DarkGray))),
    }
    spans.push(Span::styled(
        "   additional: ",
        Style::default().fg(Color::Cyan),
    ));
    if pending.additional.is_empty() {
        spans.push(Span::styled("(none)", Style::default().fg(Color::DarkGray)));
    } else {
        spans.push(Span::raw(pending.additional.join(", ")));
    }
    spans
}

fn draw_input_popup(frame: &mut Frame, prompt: &str, text: &str) {
    let area = centered_rect(60, 20, frame.area());
    let para = Paragraph::new(Line::from(vec![
        Span::raw(text.to_string()),
        Span::styled("_", Style::default().fg(Color::Yellow)),
    ]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(prompt.to_string())
            .style(Style::default().bg(Color::DarkGray).fg(Color::White)),
    );
    frame.render_widget(para, area);
}

fn draw_multiline_popup(frame: &mut Frame, prompt: &str, text: &str) {
    let area = centered_rect(70, 50, frame.area());
    let mut lines: Vec<Line> = text
        .split('\n')
        .map(|l| Line::from(Span::raw(l.to_string())))
        .collect();
    if let Some(last) = lines.last_mut() {
        last.spans
            .push(Span::styled("_", Style::default().fg(Color::Yellow)));
    }
    let para = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(prompt.to_string())
                .style(Style::default().bg(Color::DarkGray).fg(Color::White)),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(para, area);
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_sanitization() {
        assert_eq!(
            sanitize_slug("Manual Error Mapping"),
            "manual-error-mapping"
        );
        assert_eq!(sanitize_slug("  raw_uuid!  "), "raw-uuid");
        assert_eq!(sanitize_slug(""), "");
    }

    #[test]
    fn slug_picker_filter() {
        let patterns = vec![
            Pattern {
                slug: "manual_error_mapping".into(),
                category: "error-handling".into(),
                canonical_comment: "use ?".into(),
                active: true,
            },
            Pattern {
                slug: "raw_uuid".into(),
                category: "domain-primitives".into(),
                canonical_comment: "use typed id".into(),
                active: true,
            },
        ];
        assert_eq!(filter_patterns(&patterns, "").len(), 2);
        assert_eq!(filter_patterns(&patterns, "uuid").len(), 1);
        assert_eq!(filter_patterns(&patterns, "error").len(), 1);
        assert_eq!(filter_patterns(&patterns, "domain").len(), 1);
        assert_eq!(filter_patterns(&patterns, "zzz").len(), 0);
    }
}
