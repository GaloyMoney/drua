use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::state::{Focus, Mode, ScreenState};

/// Async side-effects returned from key handlers for the event loop to execute.
pub enum Action {
    None,
    Quit,
    Refresh,
    CreateWorkspace { name: String, description: String },
    SendChat { agent_id: String, prompt: String },
}

/// Top-level key dispatcher — routes to mode/focus-specific handlers.
pub fn handle_key(state: &mut ScreenState, key: KeyEvent) -> Action {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Action::Quit;
    }

    match state.mode {
        Mode::Browse => match state.focus {
            Focus::Sidebar => handle_sidebar_key(state, key),
            Focus::Agents => handle_agents_key(state, key),
            Focus::Chat => handle_chat_key(state, key),
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
        KeyCode::Char('n') => {
            state.enter_create_mode();
            Action::None
        }
        KeyCode::Char('r') => Action::Refresh,
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
        KeyCode::Backspace => {
            state.chat_input.pop();
            Action::None
        }
        KeyCode::Up => {
            state.chat_view.scroll_up();
            Action::None
        }
        KeyCode::Down => {
            state.chat_view.scroll_down();
            Action::None
        }
        KeyCode::Char(c) => {
            state.chat_input.push(c);
            Action::None
        }
        _ => Action::None,
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
            state.chat_input.clear();
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
