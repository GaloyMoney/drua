use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::state::{Focus, MillerFocus, Mode, ScreenState};

/// Async side-effects returned from key handlers for the event loop to execute.
pub enum Action {
    None,
    Quit,
    Suspend,
    Refresh,
    CreateProject {
        name: String,
        description: String,
    },
    SendChat {
        agent_id: String,
        prompt: String,
    },
    ToggleThreads,
    OpenWorkflows,
    RefreshWorkflows,
    TriggerWorkflow {
        definition_id: String,
        payload: serde_json::Value,
    },
    ExportThread {
        agent_id: String,
        path: String,
    },
    FetchStepConversation {
        agent_id: String,
        agent_name: String,
    },
    ToggleThreadsForAgent {
        agent_id: String,
    },
}

pub fn handle_key(state: &mut ScreenState, key: KeyEvent) -> Action {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    if ctrl {
        // ^C/^Z fire from any mode — they're terminal-level signals.
        match key.code {
            KeyCode::Char('c') => return Action::Quit,
            KeyCode::Char('z') => return Action::Suspend,
            _ => {}
        }
        // Browse-mode-only shortcuts. Modals (CreateProject, ExportThread,
        // ProjectPicker) own their input — letting `^R`/`^T`/`^H`/`^L`/`^O`
        // fire here would mutate state the modal depends on
        // (e.g. `^R` refresh would swap the projects list mid-picker).
        if matches!(state.mode, Mode::Browse) {
            match key.code {
                KeyCode::Char('p') => {
                    state.enter_project_picker();
                    return Action::None;
                }
                KeyCode::Char('o') => {
                    state.select_lead_and_focus_chat();
                    return Action::None;
                }
                KeyCode::Char('r') if state.focus == Focus::Workflows => {
                    return enter_trigger_for_selected_workflow(state);
                }
                KeyCode::Char('r') => return Action::Refresh,
                KeyCode::Char('h') => {
                    state.focus_left();
                    return Action::None;
                }
                KeyCode::Char('l') => {
                    state.focus_right();
                    return Action::None;
                }
                KeyCode::Char('t')
                    if state.focus == Focus::Workflows
                        && state.workflows.conversation.is_some() =>
                {
                    let conv = state.workflows.conversation.as_ref().unwrap();
                    return Action::ToggleThreadsForAgent {
                        agent_id: conv.agent_id.clone(),
                    };
                }
                KeyCode::Char('t') => return Action::ToggleThreads,
                KeyCode::Char('w')
                    if state.focus != Focus::Workflows
                        && (state.focus != Focus::Chat || state.chat_input.is_empty()) =>
                {
                    return Action::OpenWorkflows;
                }
                _ => {}
            }
        }
    }

    match &state.mode {
        Mode::Browse => match state.focus {
            Focus::Agents => handle_agents_key(state, key),
            Focus::Chat => handle_chat_key(state, key),
            Focus::Threads => handle_threads_key(state, key),
            Focus::Workflows => handle_workflows_key(state, key),
        },
        Mode::CreateProject => handle_create_key(state, key),
        Mode::ExportThread => handle_export_key(state, key),
        Mode::ProjectPicker { .. } => handle_picker_key(state, key),
        Mode::TriggerWorkflow { .. } => handle_trigger_workflow_key(state, key),
    }
}

fn handle_workflows_key(state: &mut ScreenState, key: KeyEvent) -> Action {
    state.status_message = None;
    match state.workflows.focus {
        MillerFocus::Definitions => handle_workflow_definitions_key(state, key),
        MillerFocus::YamlDetail => handle_workflow_yaml_key(state, key),
        MillerFocus::Runs => handle_workflow_runs_key(state, key),
        MillerFocus::StepDetail => handle_workflow_step_detail_key(state, key),
    }
}

fn handle_workflow_definitions_key(state: &mut ScreenState, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Char('j') | KeyCode::Down => {
            state.workflow_cursor_down();
            Action::None
        }
        KeyCode::Char('k') | KeyCode::Up => {
            state.workflow_cursor_up();
            Action::None
        }
        KeyCode::Enter => {
            state.workflow_focus_yaml();
            Action::None
        }
        KeyCode::Right | KeyCode::Char('l') => {
            state.workflow_focus_right();
            Action::None
        }
        KeyCode::Char('r') => Action::RefreshWorkflows,
        KeyCode::Esc => {
            state.focus = Focus::Chat;
            Action::None
        }
        _ => Action::None,
    }
}

fn handle_workflow_yaml_key(state: &mut ScreenState, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Char('j') | KeyCode::Down => {
            state.workflow_detail_scroll_down();
            Action::None
        }
        KeyCode::Char('k') | KeyCode::Up => {
            state.workflow_detail_scroll_up();
            Action::None
        }
        KeyCode::Right | KeyCode::Char('l') => {
            state.workflow_focus_right();
            Action::None
        }
        KeyCode::Left | KeyCode::Char('h') | KeyCode::Esc => {
            state.workflow_focus_left();
            Action::None
        }
        _ => Action::None,
    }
}

fn handle_workflow_runs_key(state: &mut ScreenState, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Char('j') | KeyCode::Down => {
            state.workflow_runs_cursor_down();
            Action::None
        }
        KeyCode::Char('k') | KeyCode::Up => {
            state.workflow_runs_cursor_up();
            Action::None
        }
        KeyCode::Char('J') | KeyCode::PageDown => {
            state.workflow_detail_scroll_down();
            Action::None
        }
        KeyCode::Char('K') | KeyCode::PageUp => {
            state.workflow_detail_scroll_up();
            Action::None
        }
        KeyCode::Left | KeyCode::Char('h') => {
            state.workflow_focus_left();
            Action::None
        }
        KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter => {
            state.workflow_focus_right();
            Action::None
        }
        KeyCode::Char('r') => Action::RefreshWorkflows,
        KeyCode::Esc => {
            state.focus = Focus::Chat;
            Action::None
        }
        _ => Action::None,
    }
}

fn handle_workflow_step_detail_key(state: &mut ScreenState, key: KeyEvent) -> Action {
    if state.workflows.conversation.is_some() {
        return handle_workflow_conversation_key(state, key);
    }
    match key.code {
        KeyCode::Char('j') | KeyCode::Down => {
            state.workflow_step_cursor_down();
            Action::None
        }
        KeyCode::Char('k') | KeyCode::Up => {
            state.workflow_step_cursor_up();
            Action::None
        }
        KeyCode::Left | KeyCode::Char('h') => {
            state.workflow_focus_left();
            Action::None
        }
        KeyCode::Enter => {
            state.workflow_toggle_expanded();
            Action::None
        }
        KeyCode::Char('c') => {
            if let Some((agent_id, agent_name)) = state.selected_step_agent_id() {
                Action::FetchStepConversation {
                    agent_id,
                    agent_name,
                }
            } else {
                state.status_message = Some("No agent for this step".to_string());
                Action::None
            }
        }
        KeyCode::Char('r') => Action::RefreshWorkflows,
        KeyCode::Esc => {
            state.focus = Focus::Chat;
            Action::None
        }
        _ => Action::None,
    }
}

fn handle_workflow_conversation_key(state: &mut ScreenState, key: KeyEvent) -> Action {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if ctrl {
        match key.code {
            KeyCode::Char('d') => {
                state.conversation_fast_scroll_down();
                return Action::None;
            }
            KeyCode::Char('u') => {
                state.conversation_fast_scroll_up();
                return Action::None;
            }
            _ => {}
        }
    }
    match key.code {
        KeyCode::Char('j') | KeyCode::Down => {
            state.conversation_scroll_down();
            Action::None
        }
        KeyCode::Char('k') | KeyCode::Up => {
            state.conversation_scroll_up();
            Action::None
        }
        KeyCode::Char('c') | KeyCode::Left | KeyCode::Char('h') => {
            state.hide_step_conversation();
            Action::None
        }
        KeyCode::Esc => {
            state.hide_step_conversation();
            Action::None
        }
        _ => Action::None,
    }
}

fn enter_trigger_for_selected_workflow(state: &mut ScreenState) -> Action {
    match state.selected_workflow_definition_id() {
        Some(definition_id) => {
            let name = state.selected_workflow_name();
            state.enter_trigger_workflow(definition_id, name);
            Action::None
        }
        None => Action::None,
    }
}

fn handle_picker_key(state: &mut ScreenState, key: KeyEvent) -> Action {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Esc => {
            state.exit_project_picker();
            return Action::None;
        }
        KeyCode::Enter => {
            let _ = state.picker_confirm();
            return Action::None;
        }
        _ => {}
    }
    // matches_len+1 because the virtual "Create new" row sits at index = matches_len.
    let matches_len = match &state.mode {
        Mode::ProjectPicker { input, .. } => state.picker_matches(&input.clone()).len(),
        _ => return Action::None,
    };
    let Mode::ProjectPicker { input, list_cursor } = &mut state.mode else {
        return Action::None;
    };
    match key.code {
        KeyCode::Up => *list_cursor = list_cursor.saturating_sub(1),
        KeyCode::Char('p') if ctrl => *list_cursor = list_cursor.saturating_sub(1),
        KeyCode::Down if *list_cursor < matches_len => *list_cursor += 1,
        KeyCode::Char('n') if ctrl && *list_cursor < matches_len => *list_cursor += 1,
        KeyCode::Backspace => {
            input.pop();
            *list_cursor = 0;
        }
        KeyCode::Char(c) if !ctrl => {
            input.push(c);
            *list_cursor = 0;
        }
        _ => {}
    }
    Action::None
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
            state.focus = Focus::Chat;
            Action::None
        }
        KeyCode::Enter => {
            state.thread_view = None;
            state.focus = Focus::Chat;
            Action::None
        }
        _ => Action::None,
    }
}

fn handle_chat_key(state: &mut ScreenState, key: KeyEvent) -> Action {
    match key.code {
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
        KeyCode::Left | KeyCode::Char('h') => {
            state.grid_move_left();
            Action::None
        }
        KeyCode::Right | KeyCode::Char('l') => {
            state.grid_move_right();
            Action::None
        }
        KeyCode::Up | KeyCode::Char('k') => {
            state.grid_move_up();
            Action::None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            state.grid_move_down();
            Action::None
        }
        KeyCode::Char('g') => {
            state.grid_jump_start();
            Action::None
        }
        KeyCode::Char('G') => {
            state.grid_jump_end();
            Action::None
        }
        KeyCode::Tab => {
            state.grid_cycle_section();
            Action::None
        }
        KeyCode::BackTab => {
            state.grid_tab_next();
            Action::None
        }
        KeyCode::Char('e') => {
            state.enter_export_mode();
            Action::None
        }
        KeyCode::Esc => {
            state.thread_view = None;
            state.focus = state.thread_return_focus.take().unwrap_or(Focus::Chat);
            if state.focus != Focus::Workflows {
                state.loaded_agent_id = None;
            }
            Action::None
        }
        _ => Action::None,
    }
}

/// Readline-style shortcuts on the chat input.
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
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
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
        KeyCode::Char(c) if !ctrl => {
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
            Action::CreateProject { name, description }
        }
        _ => Action::None,
    }
}

fn handle_export_key(state: &mut ScreenState, key: KeyEvent) -> Action {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Esc => {
            state.exit_export_mode();
            Action::None
        }
        KeyCode::Backspace => {
            state.export_path.pop();
            Action::None
        }
        KeyCode::Char(c) if !ctrl => {
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

fn handle_trigger_workflow_key(state: &mut ScreenState, key: KeyEvent) -> Action {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Esc => {
            state.exit_trigger_workflow();
            Action::None
        }
        KeyCode::Backspace => {
            if let Mode::TriggerWorkflow { payload, error, .. } = &mut state.mode {
                payload.pop();
                *error = None;
            }
            Action::None
        }
        KeyCode::Char('u') if ctrl => {
            if let Mode::TriggerWorkflow { payload, error, .. } = &mut state.mode {
                payload.clear();
                *error = None;
            }
            Action::None
        }
        KeyCode::Char(c) if !ctrl => {
            if let Mode::TriggerWorkflow { payload, error, .. } = &mut state.mode {
                payload.push(c);
                *error = None;
            }
            Action::None
        }
        KeyCode::Enter => {
            let (definition_id, payload_text) = match &state.mode {
                Mode::TriggerWorkflow {
                    definition_id,
                    payload,
                    ..
                } => (definition_id.clone(), payload.trim().to_string()),
                _ => return Action::None,
            };
            let payload_text = if payload_text.is_empty() {
                "{}".to_string()
            } else {
                payload_text
            };
            match serde_json::from_str(&payload_text) {
                Ok(payload) => {
                    state.exit_trigger_workflow();
                    Action::TriggerWorkflow {
                        definition_id,
                        payload,
                    }
                }
                Err(e) => {
                    if let Mode::TriggerWorkflow { error, .. } = &mut state.mode {
                        *error = Some(format!("Invalid JSON: {e}"));
                    }
                    Action::None
                }
            }
        }
        _ => Action::None,
    }
}

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

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;
    use crate::tui::state::{AgentItem, MillerFocus, ProjectItem};

    fn project() -> ProjectItem {
        ProjectItem {
            id: "p-1".into(),
            name: "Project".into(),
            description: None,
            created_at: None,
            lead: None,
            agents: vec![AgentItem {
                id: "a-1".into(),
                name: "lead".into(),
                role: "PROJECT_LEAD".into(),
                model: "test".into(),
                sandbox: None,
            }],
        }
    }

    fn state() -> ScreenState {
        ScreenState::new(vec![project()], "http://s".into(), "u".into(), None)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    #[test]
    fn ctrl_w_opens_workflows() {
        let mut state = state();
        let action = handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL),
        );
        assert!(matches!(action, Action::OpenWorkflows));
    }

    #[test]
    fn ctrl_w_is_noop_when_already_focused_on_workflows() {
        let mut state = state();
        state.enter_workflows();
        state.workflows.focus = MillerFocus::Runs;

        let action = handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL),
        );

        assert!(matches!(action, Action::None));
        assert_eq!(state.workflows.focus, MillerFocus::Runs);
    }

    #[test]
    fn ctrl_w_still_deletes_chat_word_when_input_has_text() {
        let mut state = state();
        state.chat_input = "hello world".into();
        state.input_cursor = state.chat_input.len();

        let action = handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL),
        );

        assert!(matches!(action, Action::None));
        assert_eq!(state.chat_input, "hello ");
    }

    #[test]
    fn trigger_modal_empty_payload_submits_empty_object() {
        let mut state = state();
        state.enter_trigger_workflow("wf-1".into(), "wf".into());
        if let Mode::TriggerWorkflow { payload, .. } = &mut state.mode {
            payload.clear();
        }

        let action = handle_key(&mut state, key(KeyCode::Enter));

        match action {
            Action::TriggerWorkflow {
                definition_id,
                payload,
            } => {
                assert_eq!(definition_id, "wf-1");
                assert_eq!(payload, serde_json::json!({}));
            }
            _ => panic!("expected trigger action"),
        }
    }

    #[test]
    fn trigger_modal_invalid_json_stays_open_with_error() {
        let mut state = state();
        state.enter_trigger_workflow("wf-1".into(), "wf".into());
        if let Mode::TriggerWorkflow { payload, .. } = &mut state.mode {
            *payload = "{".into();
        }

        let action = handle_key(&mut state, key(KeyCode::Enter));

        assert!(matches!(action, Action::None));
        assert!(matches!(
            state.mode,
            Mode::TriggerWorkflow { error: Some(_), .. }
        ));
    }

    #[test]
    fn arrow_right_moves_focus_from_definitions_to_runs() {
        let mut state = state();
        state.enter_workflows();
        state.workflows.runs = vec![crate::tui::state::WorkflowRunItem {
            id: "r-1".into(),
            state: "SUCCEEDED".into(),
            started_at: "now".into(),
            completed_at: None,
            step_results: vec![],
        }];

        let action = handle_key(&mut state, key(KeyCode::Right));

        assert!(matches!(action, Action::None));
        assert_eq!(state.workflows.focus, MillerFocus::Runs);
    }

    #[test]
    fn arrow_left_moves_focus_from_runs_to_definitions() {
        let mut state = state();
        state.enter_workflows();
        state.workflows.focus = MillerFocus::Runs;

        let action = handle_key(&mut state, key(KeyCode::Left));

        assert!(matches!(action, Action::None));
        assert_eq!(state.workflows.focus, MillerFocus::Definitions);
    }

    #[test]
    fn esc_exits_workflows_from_any_focus() {
        for focus in [
            MillerFocus::Definitions,
            MillerFocus::Runs,
            MillerFocus::StepDetail,
        ] {
            let mut state = state();
            state.enter_workflows();
            state.workflows.focus = focus;

            let action = handle_key(&mut state, key(KeyCode::Esc));

            assert!(matches!(action, Action::None));
            assert_eq!(state.focus, Focus::Chat);
        }
    }

    fn state_with_conversation() -> ScreenState {
        let mut state = state();
        state.enter_workflows();
        state.workflows.focus = MillerFocus::StepDetail;
        state.show_step_conversation("agent-1".into(), "test-agent".into(), vec![]);
        state.update_conversation_viewport_height(40);
        state
    }

    #[test]
    fn ctrl_d_fast_scrolls_conversation_down() {
        let mut state = state_with_conversation();
        let action = handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
        );
        assert!(matches!(action, Action::None));
        assert_eq!(state.workflows.conversation.as_ref().unwrap().scroll, 20);
    }

    #[test]
    fn ctrl_u_fast_scrolls_conversation_up() {
        let mut state = state_with_conversation();
        if let Some(conv) = &mut state.workflows.conversation {
            conv.scroll = 30;
        }
        let action = handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
        );
        assert!(matches!(action, Action::None));
        assert_eq!(state.workflows.conversation.as_ref().unwrap().scroll, 10);
    }

    #[test]
    fn ctrl_t_in_conversation_returns_toggle_threads_for_agent() {
        let mut state = state_with_conversation();
        let action = handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
        );
        match action {
            Action::ToggleThreadsForAgent { agent_id } => {
                assert_eq!(agent_id, "agent-1");
            }
            _ => panic!("expected ToggleThreadsForAgent"),
        }
    }

    #[test]
    fn esc_on_threads_returns_to_workflow_when_return_focus_set() {
        let mut state = state();
        state.focus = Focus::Threads;
        state.thread_return_focus = Some(Focus::Workflows);
        state.thread_view = Some(crate::tui::state::ThreadGridState {
            threads: vec![],
            positions: vec![],
            grid: vec![],
            details: std::collections::HashMap::new(),
            cursor_col: 0,
            cursor_row: 0,
            scroll_col: 0,
            visible_cols: 0,
            system_positions: vec![],
            system_grid: vec![],
            tool_def_counts: vec![],
            cursor_section: crate::tui::state::GridSection::Messages,
            system_details: std::collections::HashMap::new(),
        });

        let action = handle_key(&mut state, key(KeyCode::Esc));

        assert!(matches!(action, Action::None));
        assert_eq!(state.focus, Focus::Workflows);
        assert!(state.thread_view.is_none());
    }
}
