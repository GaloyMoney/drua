use anyhow::Result;
use serde::Deserialize;

use crate::config::Config;
use crate::graphql::GraphqlClient;

#[derive(Debug, Deserialize)]
struct ProjectsResponse {
    projects: ProjectConnection,
}

#[derive(Debug, Deserialize)]
struct ProjectConnection {
    edges: Vec<ProjectEdge>,
}

#[derive(Debug, Deserialize)]
struct ProjectEdge {
    node: Project,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Project {
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
struct ProjectCreateResponse {
    #[serde(rename = "projectCreate")]
    project_create: ProjectCreatePayload,
}

#[derive(Debug, Deserialize)]
struct ProjectCreatePayload {
    project: Project,
}

#[derive(Debug, Deserialize)]
struct ProjectShowResponse {
    project: Project,
}

pub async fn list() -> Result<()> {
    let config = Config::load()?;
    let client = GraphqlClient::new(&config.server_url, &config.auth_token);

    let query = r#"
        query {
            projects(first: 50) {
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

    let resp: ProjectsResponse = client.query(query, serde_json::json!({})).await?;

    if resp.projects.edges.is_empty() {
        println!("No projects found. Create one with `drua project create <name>`.");
        return Ok(());
    }

    println!("{:<38} {:<24} DESCRIPTION", "ID", "NAME");
    println!("{}", "-".repeat(80));
    for edge in &resp.projects.edges {
        let project = &edge.node;
        let desc = project.description.as_deref().unwrap_or("");
        println!("{:<38} {:<24} {}", project.id, project.name, desc);
    }

    Ok(())
}

pub async fn create(name: &str, description: Option<&str>) -> Result<()> {
    let config = Config::load()?;
    let client = GraphqlClient::new(&config.server_url, &config.auth_token);

    let query = r#"
        mutation ProjectCreate($input: ProjectCreateInput!) {
            projectCreate(input: $input) {
                project {
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

    let resp: ProjectCreateResponse = client
        .query(query, serde_json::json!({ "input": input }))
        .await?;

    let project = &resp.project_create.project;
    println!("Project created:");
    println!("  ID:   {}", project.id);
    println!("  Name: {}", project.name);

    if let Some(lead) = &project.lead {
        println!("  Lead: {} ({})", lead.name, lead.id);
    }

    Ok(())
}

pub async fn show(id: &str) -> Result<()> {
    let config = Config::load()?;
    let client = GraphqlClient::new(&config.server_url, &config.auth_token);

    let query = r#"
        query Project($id: UUID!) {
            project(id: $id) {
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

    let resp: ProjectShowResponse = client.query(query, serde_json::json!({ "id": id })).await?;

    let project = &resp.project;
    println!("Project: {}", project.name);
    println!("  ID:          {}", project.id);
    if let Some(desc) = &project.description {
        println!("  Description: {desc}");
    }
    if let Some(created) = &project.created_at {
        println!("  Created:     {created}");
    }
    if let Some(lead) = &project.lead {
        println!("  Lead agent:  {} ({}) [{}]", lead.name, lead.id, lead.role);
    }

    Ok(())
}
