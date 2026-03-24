use std::io;

use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use style_agent_core::label_store::{load_audit_suspects, LabelStore};
use style_agent_core::review_session::{
    compute_label_counts, compute_saturated_labels, persist_confirm, persist_confirm_label_only,
    persist_undo, persist_undo_label_only, review_chunks_from_suspects,
};
use style_agent_core::store::VectorStore;

use crate::config::Config;
use crate::tui::app::{App, InputMode};
use crate::tui::ui;

pub fn run(config: &Config, confused: bool) -> anyhow::Result<()> {
    let min_per_label = config.services.min_reviews_per_label;
    let db_path = config.db_path();
    let store = VectorStore::new(&db_path)?;
    store.ensure_collection()?;
    let label_store = LabelStore::new(config.labels_dir().join("labels.jsonl"));

    let label_counts = compute_label_counts(&label_store);
    let saturated = compute_saturated_labels(&label_store, min_per_label);
    if !saturated.is_empty() {
        eprintln!(
            "Skipping saturated labels (>={min_per_label} reviews): {}",
            saturated.iter().cloned().collect::<Vec<_>>().join(", ")
        );
    }

    eprintln!("Loading chunks...");
    let chunks = if confused {
        let suspects_path = config.labels_dir().join("audit-suspects.jsonl");
        let suspects = load_audit_suspects(&suspects_path)?;
        if suspects.is_empty() {
            eprintln!("No audit suspects found. Run `make audit-labels` first.");
            return Ok(());
        }
        eprintln!("Loaded {} audit suspects for re-review.", suspects.len());
        review_chunks_from_suspects(&suspects)
    } else {
        store.scroll_all_chunks()?
    };
    let total = chunks.len();

    if total == 0 {
        eprintln!("No chunks found. Run `style-agent bootstrap` or `style-agent index` first.");
        return Ok(());
    }

    eprintln!("Loaded {total} chunks. Launching TUI...");

    let mut app = App::new(chunks, saturated, label_counts, min_per_label);

    // Setup terminal.
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_event_loop(&mut terminal, &mut app, &store, &label_store, confused);

    // Restore terminal.
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result?;

    // Print summary.
    println!("\n--- Review Session Summary ---");
    println!("Reviewed this session: {}", app.reviewed_this_session());
    println!(
        "Total reviewed: {}/{}",
        app.total_reviewed(),
        app.total_count()
    );
    println!();

    Ok(())
}

fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    store: &VectorStore,
    label_store: &LabelStore,
    confused: bool,
) -> anyhow::Result<()> {
    loop {
        terminal.draw(|frame| ui::draw(frame, app))?;

        if app.should_quit {
            break;
        }

        if event::poll(std::time::Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                match &app.input_mode {
                    InputMode::Normal => {
                        handle_normal_key(
                            key.code,
                            key.modifiers,
                            app,
                            store,
                            label_store,
                            confused,
                        )?;
                    }
                    InputMode::AddLabel(_) => {
                        handle_add_label_key(key.code, app);
                    }
                    InputMode::Search(_) => {
                        handle_search_key(key.code, app);
                    }
                }
            }
        }
    }
    Ok(())
}

fn handle_normal_key(
    code: KeyCode,
    modifiers: KeyModifiers,
    app: &mut App,
    store: &VectorStore,
    label_store: &LabelStore,
    confused: bool,
) -> anyhow::Result<()> {
    match code {
        // Quit
        KeyCode::Char('q') => {
            app.should_quit = true;
        }
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.should_quit = true;
        }

        // Navigate labels
        KeyCode::Char('j') | KeyCode::Down => {
            app.label_down();
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.label_up();
        }

        // Toggle label
        KeyCode::Char(' ') | KeyCode::Enter => {
            app.toggle_label();
        }

        // Confirm and next
        KeyCode::Char('n') | KeyCode::Tab => {
            if let Some(data) = app.confirm_and_next() {
                if confused {
                    persist_confirm_label_only(label_store, &data)?;
                } else {
                    persist_confirm(store, label_store, &data)?;
                }
            }
        }

        // Undo last confirm
        KeyCode::Char('u') if !modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(undo) = app.undo() {
                if confused {
                    persist_undo_label_only(label_store)?;
                } else {
                    persist_undo(store, label_store, &undo)?;
                }
            }
        }

        // Previous chunk
        KeyCode::Char('p') | KeyCode::BackTab => {
            app.prev_chunk();
        }

        // Skip
        KeyCode::Char('s') => {
            app.skip_chunk();
        }

        // Filter mode
        KeyCode::Char('f') => {
            app.cycle_filter();
        }

        // Add custom label
        KeyCode::Char('a') => {
            app.input_mode = InputMode::AddLabel(String::new());
        }

        // Yank (copy) code to clipboard
        KeyCode::Char('y') => {
            if let Some(chunk) = app.current_chunk() {
                let _ = copy_to_clipboard(&chunk.content);
            }
        }

        // Search
        KeyCode::Char('/') => {
            app.input_mode = InputMode::Search(String::new());
        }

        // Scroll code panel
        KeyCode::Char('J') => {
            app.scroll_code_down(20);
        }
        KeyCode::Char('K') => {
            app.scroll_code_up();
        }

        // Ctrl+d / Ctrl+u for page scrolling
        KeyCode::Char('d') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.scroll_code_down(20);
        }
        KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.scroll_code_up();
        }

        _ => {}
    }
    Ok(())
}

fn handle_add_label_key(code: KeyCode, app: &mut App) {
    let text = match &app.input_mode {
        InputMode::AddLabel(t) => t.clone(),
        _ => return,
    };

    match code {
        KeyCode::Enter => {
            let label = text.trim().to_string();
            app.add_custom_label(label);
            app.input_mode = InputMode::Normal;
        }
        KeyCode::Esc => {
            app.input_mode = InputMode::Normal;
        }
        KeyCode::Backspace => {
            let mut t = text;
            t.pop();
            app.input_mode = InputMode::AddLabel(t);
        }
        KeyCode::Char(c) => {
            let mut t = text;
            t.push(c);
            app.input_mode = InputMode::AddLabel(t);
        }
        _ => {}
    }
}

fn handle_search_key(code: KeyCode, app: &mut App) {
    let text = match &app.input_mode {
        InputMode::Search(t) => t.clone(),
        _ => return,
    };

    match code {
        KeyCode::Enter => {
            if text.is_empty() {
                app.apply_filter();
            } else {
                app.apply_search(&text);
            }
            app.input_mode = InputMode::Normal;
        }
        KeyCode::Esc => {
            app.apply_filter();
            app.input_mode = InputMode::Normal;
        }
        KeyCode::Backspace => {
            let mut t = text;
            t.pop();
            app.input_mode = InputMode::Search(t);
        }
        KeyCode::Char(c) => {
            let mut t = text;
            t.push(c);
            app.input_mode = InputMode::Search(t);
        }
        _ => {}
    }
}

fn copy_to_clipboard(text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("pbcopy").stdin(Stdio::piped()).spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes())?;
    }
    child.wait()?;
    Ok(())
}
