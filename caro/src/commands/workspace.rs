use anyhow::Result;
use serde::Deserialize;

use crate::config::Config;
use crate::graphql::GraphqlClient;

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
    node: Workspace,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Workspace {
    id: String,
    name: String,
    description: Option<String>,
    created_at: Option<String>,
    lead: Option<Agent>,
}

#[derive(Debug, Deserialize)]
struct Agent {
    id: String,
    name: String,
    role: String,
}

#[derive(Debug, Deserialize)]
struct WorkspaceCreateResponse {
    #[serde(rename = "workspaceCreate")]
    workspace_create: WorkspaceCreatePayload,
}

#[derive(Debug, Deserialize)]
struct WorkspaceCreatePayload {
    workspace: Workspace,
}

#[derive(Debug, Deserialize)]
struct WorkspaceShowResponse {
    workspace: Workspace,
}

pub async fn list() -> Result<()> {
    let config = Config::load()?;
    let client = GraphqlClient::new(&config.server_url, &config.auth_token);

    let query = r#"
        query {
            workspaces(first: 50) {
                edges {
                    node {
                        id
                        name
                        description
                        createdAt
                    }
                }
            }
        }
    "#;

    let resp: WorkspacesResponse = client.query(query, serde_json::json!({})).await?;

    if resp.workspaces.edges.is_empty() {
        println!("No workspaces found. Create one with `caro workspace create <name>`.");
        return Ok(());
    }

    println!("{:<38} {:<24} DESCRIPTION", "ID", "NAME");
    println!("{}", "-".repeat(80));
    for edge in &resp.workspaces.edges {
        let ws = &edge.node;
        let desc = ws.description.as_deref().unwrap_or("");
        println!("{:<38} {:<24} {}", ws.id, ws.name, desc);
    }

    Ok(())
}

pub async fn create(name: &str, description: Option<&str>) -> Result<()> {
    let config = Config::load()?;
    let client = GraphqlClient::new(&config.server_url, &config.auth_token);

    let query = r#"
        mutation WorkspaceCreate($input: WorkspaceCreateInput!) {
            workspaceCreate(input: $input) {
                workspace {
                    id
                    name
                    lead {
                        id
                        name
                        role
                    }
                }
            }
        }
    "#;

    let mut input = serde_json::json!({ "name": name });
    if let Some(desc) = description {
        input["description"] = serde_json::json!(desc);
    }

    let resp: WorkspaceCreateResponse = client
        .query(query, serde_json::json!({ "input": input }))
        .await?;

    let ws = &resp.workspace_create.workspace;
    println!("Workspace created:");
    println!("  ID:   {}", ws.id);
    println!("  Name: {}", ws.name);

    if let Some(lead) = &ws.lead {
        println!("  Lead: {} ({})", lead.name, lead.id);
    }

    Ok(())
}

pub async fn show(id: &str) -> Result<()> {
    let config = Config::load()?;
    let client = GraphqlClient::new(&config.server_url, &config.auth_token);

    let query = r#"
        query Workspace($id: UUID!) {
            workspace(id: $id) {
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
    "#;

    let resp: WorkspaceShowResponse = client.query(query, serde_json::json!({ "id": id })).await?;

    let ws = &resp.workspace;
    println!("Workspace: {}", ws.name);
    println!("  ID:          {}", ws.id);
    if let Some(desc) = &ws.description {
        println!("  Description: {desc}");
    }
    if let Some(created) = &ws.created_at {
        println!("  Created:     {created}");
    }
    if let Some(lead) = &ws.lead {
        println!("  Lead agent:  {} ({}) [{}]", lead.name, lead.id, lead.role);
    }

    Ok(())
}
