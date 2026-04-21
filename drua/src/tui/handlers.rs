use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::state::{Focus, Mode, ScreenState};

/// Async side-effects returned from key handlers for the event loop to execute.
pub enum Action {
    None,
    Quit,
    Suspend,
    Refresh,
    CreateWorkspace { name: String, description: String },
    SendChat { agent_id: String, prompt: String },
    ToggleThreads,
}

/// Top-level key dispatcher — routes to mode/focus-specific handlers.
pub fn handle_key(state: &mut ScreenState, key: KeyEvent) -> Action {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    if ctrl {
        match key.code {
            KeyCode::Char('c') => return Action::Quit,
            KeyCode::Char('z') => return Action::Suspend,
            // Ctrl+O — jump to lead agent chat (global, like command-center)
            KeyCode::Char('o') => {
                state.select_lead_and_focus_chat();
                return Action::None;
            }
            // Ctrl+R — refresh workspaces
            KeyCode::Char('r') => return Action::Refresh,
            // Ctrl+H / Ctrl+L — focus left / right
            KeyCode::Char('h') => {
                state.focus_left();
                return Action::None;
            }
            KeyCode::Char('l') => {
                state.focus_right();
                return Action::None;
            }
            // Ctrl+T — toggle thread explorer
            KeyCode::Char('t') => return Action::ToggleThreads,
            _ => {}
        }
    }

    match state.mode {
        Mode::Browse => match state.focus {
            Focus::Sidebar => handle_sidebar_key(state, key),
            Focus::Agents => handle_agents_key(state, key),
            Focus::Chat => handle_chat_key(state, key),
            Focus::Threads => handle_threads_key(state, key),
        },
        Mode::CreateWorkspace => handle_create_key(state, key),
    }
}

fn handle_sidebar_key(state: &mut ScreenState, key: KeyEvent) -> Action {
    state.status_message = None;
    match key.code {
        KeyCode::Char('q') => Action::Quit,
        KeyCode::Char('j') | KeyCode::Down => {
            state.cursor_down();
            Action::None
        }
        KeyCode::Char('k') | KeyCode::Up => {
            state.cursor_up();
            Action::None
        }
        KeyCode::Enter => {
            state.select_lead_and_focus_chat();
            Action::None
        }
        KeyCode::Char('n') => {
            state.enter_create_mode();
            Action::None
        }
        KeyCode::Tab => {
            state.toggle_focus();
            Action::None
        }
        _ => Action::None,
    }
}

fn handle_agents_key(state: &mut ScreenState, key: KeyEvent) -> Action {
    state.status_message = None;
    match key.code {
        KeyCode::Char('j') | KeyCode::Down => {
            state.agent_cursor_down();
            Action::None
        }
        KeyCode::Char('k') | KeyCode::Up => {
            state.agent_cursor_up();
            Action::None
        }
        KeyCode::Tab => {
            state.toggle_focus();
            Action::None
        }
        KeyCode::Esc => {
            state.focus = Focus::Sidebar;
            Action::None
        }
        KeyCode::Enter => {
            let input = state.chat_input.trim().to_string();
            if input.is_empty() {
                // Switch to chat pane for typing
                state.focus = Focus::Chat;
                return Action::None;
            }
            send_chat_to_selected_agent(state, input)
        }
        _ => Action::None,
    }
}

fn handle_chat_key(state: &mut ScreenState, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc => {
            state.focus = Focus::Sidebar;
            Action::None
        }
        KeyCode::Tab => {
            state.toggle_focus();
            Action::None
        }
        KeyCode::Enter => {
            let input = state.chat_input.trim().to_string();
            if input.is_empty() {
                return Action::None;
            }
            send_chat_to_selected_agent(state, input)
        }
        KeyCode::Up => {
            state.chat_view.scroll_up();
            Action::None
        }
        KeyCode::Down => {
            state.chat_view.scroll_down();
            Action::None
        }
        _ => {
            handle_input_editing(state, &key);
            Action::None
        }
    }
}

fn handle_threads_key(state: &mut ScreenState, key: KeyEvent) -> Action {
    match key.code {
        // ←→ navigate positions (columns)
        KeyCode::Left | KeyCode::Char('h') => {
            state.grid_move_left();
            Action::None
        }
        KeyCode::Right | KeyCode::Char('l') => {
            state.grid_move_right();
            Action::None
        }
        // ↑↓ navigate threads (rows)
        KeyCode::Up | KeyCode::Char('k') => {
            state.grid_move_up();
            Action::None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            state.grid_move_down();
            Action::None
        }
        // g/G — jump to start/end of positions
        KeyCode::Char('g') => {
            state.grid_jump_start();
            Action::None
        }
        KeyCode::Char('G') => {
            state.grid_jump_end();
            Action::None
        }
        // Tab — jump to next unique/summary cell
        KeyCode::Tab => {
            state.grid_tab_next();
            Action::None
        }
        KeyCode::Esc => {
            state.focus = Focus::Sidebar;
            Action::None
        }
        _ => Action::None,
    }
}

// ── Shared input editing ────────────────────────────────────────────

/// Handle common text-editing key events on the chat input.
/// Follows the same readline-style shortcuts as command-center.
fn handle_input_editing(state: &mut ScreenState, key: &KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char('u') if ctrl => state.input_kill_to_start(),
        KeyCode::Char('w') if ctrl => state.input_kill_word(),
        KeyCode::Char('a') if ctrl => state.input_home(),
        KeyCode::Char('e') if ctrl => state.input_end(),
        KeyCode::Char(c) if !ctrl => state.input_insert_char(c),
        KeyCode::Backspace => state.input_backspace(),
        KeyCode::Left => state.input_move_left(),
        KeyCode::Right => state.input_move_right(),
        KeyCode::Home => state.input_home(),
        KeyCode::End => state.input_end(),
        _ => {}
    }
}

fn handle_create_key(state: &mut ScreenState, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc => {
            state.exit_create_mode();
            Action::None
        }
        KeyCode::Tab | KeyCode::BackTab => {
            state.input_field = (state.input_field + 1) % 2;
            Action::None
        }
        KeyCode::Backspace => {
            state.active_input_mut().pop();
            Action::None
        }
        KeyCode::Char(c) => {
            state.active_input_mut().push(c);
            Action::None
        }
        KeyCode::Enter => {
            if state.input_name.trim().is_empty() {
                state.status_message = Some("Name is required".to_string());
                return Action::None;
            }
            let name = state.input_name.trim().to_string();
            let description = state.input_description.trim().to_string();
            Action::CreateWorkspace { name, description }
        }
        _ => Action::None,
    }
}

/// Send chat to whichever agent is currently selected in the agents panel.
fn send_chat_to_selected_agent(state: &mut ScreenState, input: String) -> Action {
    match state.selected_agent_id() {
        Some(agent_id) => {
            state.chat_view.assistant.add_user_message(&input);
            state.input_clear();
            state.chat_view.reset_scroll();
            Action::SendChat {
                agent_id,
                prompt: input,
            }
        }
        None => {
            state.status_message = Some("No agent selected".to_string());
            Action::None
        }
    }
}
