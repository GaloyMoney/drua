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
    ExportThread { agent_id: String, path: String },
}

/// Top-level key dispatcher — routes to mode/focus-specific handlers.
pub fn handle_key(state: &mut ScreenState, key: KeyEvent) -> Action {
    let kb = &state.keybindings;

    // ── Global bindings (checked first, regardless of mode/focus) ────
    if kb.global.quit.matches(&key) {
        return Action::Quit;
    }
    if kb.global.suspend.matches(&key) {
        return Action::Suspend;
    }
    if kb.global.focus_lead.matches(&key) {
        state.select_lead_and_focus_chat();
        return Action::None;
    }
    if kb.global.refresh.matches(&key) {
        return Action::Refresh;
    }
    if kb.global.focus_left.matches(&key) {
        state.focus_left();
        return Action::None;
    }
    if kb.global.focus_right.matches(&key) {
        state.focus_right();
        return Action::None;
    }
    if kb.global.toggle_threads.matches(&key) {
        return Action::ToggleThreads;
    }
    if kb.global.show_help.matches(&key) {
        state.show_help = !state.show_help;
        return Action::None;
    }

    match state.mode {
        Mode::Browse => match state.focus {
            Focus::Sidebar => handle_sidebar_key(state, key),
            Focus::Agents => handle_agents_key(state, key),
            Focus::Chat => handle_chat_key(state, key),
            Focus::Threads => handle_threads_key(state, key),
        },
        Mode::CreateWorkspace => handle_create_key(state, key),
        Mode::ExportThread => handle_export_key(state, key),
    }
}

fn handle_sidebar_key(state: &mut ScreenState, key: KeyEvent) -> Action {
    state.status_message = None;
    let kb = &state.keybindings.sidebar;

    if kb.quit.matches(&key) {
        return Action::Quit;
    }
    if kb.navigate_down.matches(&key) {
        state.cursor_down();
        return Action::None;
    }
    if kb.navigate_up.matches(&key) {
        state.cursor_up();
        return Action::None;
    }
    if kb.select.matches(&key) {
        state.select_lead_and_focus_chat();
        return Action::None;
    }
    if kb.new_workspace.matches(&key) {
        state.enter_create_mode();
        return Action::None;
    }
    if kb.toggle_focus.matches(&key) {
        state.toggle_focus();
        return Action::None;
    }
    Action::None
}

fn handle_agents_key(state: &mut ScreenState, key: KeyEvent) -> Action {
    state.status_message = None;
    let kb = &state.keybindings.agents;

    if kb.navigate_down.matches(&key) {
        state.agent_cursor_down();
        return Action::None;
    }
    if kb.navigate_up.matches(&key) {
        state.agent_cursor_up();
        return Action::None;
    }
    if kb.toggle_focus.matches(&key) {
        state.toggle_focus();
        return Action::None;
    }
    if kb.back.matches(&key) {
        state.focus = Focus::Sidebar;
        return Action::None;
    }
    if kb.open_chat.matches(&key) {
        // Close thread view so chat pane is visible, switch to chat for typing
        state.thread_view = None;
        state.focus = Focus::Chat;
        return Action::None;
    }
    Action::None
}

fn handle_chat_key(state: &mut ScreenState, key: KeyEvent) -> Action {
    let kb = &state.keybindings.chat;

    if kb.back.matches(&key) {
        state.focus = Focus::Sidebar;
        return Action::None;
    }
    if kb.toggle_focus.matches(&key) {
        state.toggle_focus();
        return Action::None;
    }
    if kb.send.matches(&key) {
        let input = state.chat_input.trim().to_string();
        if input.is_empty() {
            return Action::None;
        }
        return send_chat_to_selected_agent(state, input);
    }
    if kb.scroll_up.matches(&key) {
        state.chat_view.scroll_up();
        return Action::None;
    }
    if kb.scroll_down.matches(&key) {
        state.chat_view.scroll_down();
        return Action::None;
    }

    // Fall through to text input editing
    handle_input_editing(state, &key);
    Action::None
}

fn handle_threads_key(state: &mut ScreenState, key: KeyEvent) -> Action {
    let kb = &state.keybindings.threads;

    if kb.navigate_left.matches(&key) {
        state.grid_move_left();
        return Action::None;
    }
    if kb.navigate_right.matches(&key) {
        state.grid_move_right();
        return Action::None;
    }
    if kb.navigate_up.matches(&key) {
        state.grid_move_up();
        return Action::None;
    }
    if kb.navigate_down.matches(&key) {
        state.grid_move_down();
        return Action::None;
    }
    if kb.jump_start.matches(&key) {
        state.grid_jump_start();
        return Action::None;
    }
    if kb.jump_end.matches(&key) {
        state.grid_jump_end();
        return Action::None;
    }
    if kb.tab_next.matches(&key) {
        state.grid_tab_next();
        return Action::None;
    }
    if kb.export.matches(&key) {
        state.enter_export_mode();
        return Action::None;
    }
    if kb.back.matches(&key) {
        state.focus = Focus::Sidebar;
        return Action::None;
    }
    Action::None
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
    let kb = &state.keybindings.create_workspace;

    if kb.cancel.matches(&key) {
        state.exit_create_mode();
        return Action::None;
    }
    if kb.switch_field.matches(&key) {
        state.input_field = (state.input_field + 1) % 2;
        return Action::None;
    }
    if kb.confirm.matches(&key) {
        if state.input_name.trim().is_empty() {
            state.status_message = Some("Name is required".to_string());
            return Action::None;
        }
        let name = state.input_name.trim().to_string();
        let description = state.input_description.trim().to_string();
        return Action::CreateWorkspace { name, description };
    }

    // Fall through to character input for the create form
    match key.code {
        KeyCode::Backspace => {
            state.active_input_mut().pop();
        }
        KeyCode::Char(c) => {
            state.active_input_mut().push(c);
        }
        _ => {}
    }
    Action::None
}

fn handle_export_key(state: &mut ScreenState, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc => {
            state.exit_export_mode();
            Action::None
        }
        KeyCode::Backspace => {
            state.export_path.pop();
            Action::None
        }
        KeyCode::Char(c) => {
            state.export_path.push(c);
            Action::None
        }
        KeyCode::Enter => {
            let path = state.export_path.trim().to_string();
            if path.is_empty() {
                state.status_message = Some("Path is required".to_string());
                return Action::None;
            }
            match state.selected_agent_id() {
                Some(agent_id) => {
                    state.exit_export_mode();
                    Action::ExportThread { agent_id, path }
                }
                None => {
                    state.status_message = Some("No agent selected".to_string());
                    Action::None
                }
            }
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
