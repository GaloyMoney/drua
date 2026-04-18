use std::io;

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use serde::Deserialize;

use crate::config::Config;
use crate::graphql::GraphqlClient;
use crate::tui::app::{AgentItem, App, WorkspaceItem};
use crate::tui::ui;

#[derive(Debug, Deserialize)]
struct MeResponse {
    me: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WorkspacesResponse {
    workspaces: WorkspaceConnection,
}

#[derive(Debug, Deserialize)]
struct WorkspaceConnection {
    edges: Vec<WorkspaceEdge>,
}

#[derive(Debug, Deserialize)]
struct WorkspaceEdge {
    node: WorkspaceNode,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceNode {
    id: String,
    name: String,
    description: Option<String>,
    created_at: Option<String>,
    lead: Option<AgentNode>,
}

#[derive(Debug, Deserialize)]
struct AgentNode {
    id: String,
    name: String,
    role: String,
}

const WORKSPACES_QUERY: &str = r#"
    query {
        workspaces(first: 50) {
            edges {
                node {
                    id
                    name
                    description
                    createdAt
                    lead {
                        id
                        name
                        role
                    }
                }
            }
        }
    }
"#;

async fn fetch_workspaces(client: &GraphqlClient) -> Result<Vec<WorkspaceItem>> {
    let resp: WorkspacesResponse = client
        .query(WORKSPACES_QUERY, serde_json::json!({}))
        .await?;

    let items = resp
        .workspaces
        .edges
        .into_iter()
        .map(|edge| {
            let node = edge.node;
            WorkspaceItem {
                id: node.id,
                name: node.name,
                description: node.description,
                created_at: node.created_at,
                lead: node.lead.map(|a| AgentItem {
                    id: a.id,
                    name: a.name,
                    role: a.role,
                }),
            }
        })
        .collect();

    Ok(items)
}

async fn fetch_user_id(client: &GraphqlClient) -> Result<String> {
    let resp: MeResponse = client.query("{ me }", serde_json::json!({})).await?;
    resp.me.ok_or_else(|| anyhow::anyhow!("not authenticated"))
}

pub async fn run() -> Result<()> {
    let config = Config::load()?;
    let client = GraphqlClient::new(&config.server_url, &config.auth_token);

    let user_id = fetch_user_id(&client).await?;
    let workspaces = fetch_workspaces(&client).await?;

    let mut app = App::new(workspaces, config.server_url.clone(), user_id);

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_event_loop(&mut terminal, &mut app, &client).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

async fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    client: &GraphqlClient,
) -> Result<()> {
    loop {
        terminal.draw(|frame| ui::draw(frame, app))?;

        if app.should_quit {
            break;
        }

        if event::poll(std::time::Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                    app.should_quit = true;
                    continue;
                }

                match key.code {
                    KeyCode::Char('q') => app.should_quit = true,
                    KeyCode::Char('j') | KeyCode::Down => app.cursor_down(),
                    KeyCode::Char('k') | KeyCode::Up => app.cursor_up(),
                    KeyCode::Char('r') => {
                        if let Ok(workspaces) = fetch_workspaces(client).await {
                            app.replace_workspaces(workspaces);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}
