//! YAML wire-format for `WorkflowDefinition`. Lives here (not in
//! `library/`) because the schema is workflow-specific; the library
//! only owns the file-abstraction layer (`UpstreamOp`, `SyncedFile`,
//! `ParsedFile`).

use crate::library::{name_from_filename, slugify, DocType, ParsedFile, SyncedFile, UpstreamOp};
use crate::primitives::{WorkflowDefinitionId, WorkspaceId};
use crate::sandbox::{SandboxAgentMode, SandboxMode, SandboxSpecs};

use super::definition::{WorkflowSandboxDecl, WorkflowStepDef, WorkflowTrigger};

#[derive(Debug, serde::Serialize, serde::Deserialize, Default)]
struct WorkflowYaml {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<uuid::Uuid>,
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    trigger: WorkflowTriggerYaml,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    sandboxes: Vec<WorkflowSandboxYaml>,
    steps: Vec<WorkflowStepYaml>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    created: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    updated: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WorkflowSandboxYaml {
    Scratch {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        config: Option<ScratchYamlConfig>,
    },
    Repo {
        name: String,
        config: RepoYamlConfig,
    },
    /// References an existing sandbox in the workflow's workspace by
    /// name (workspace-unique). The workflow executor only attaches —
    /// no provisioning, no lifecycle management. No `config` block:
    /// the sandbox already has its own.
    Preexisting { name: String },
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct ScratchYamlConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cpu: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    memory: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    disk_size: Option<String>,
}

impl ScratchYamlConfig {
    fn into_specs(self) -> Option<SandboxSpecs> {
        match (self.cpu, self.memory, self.disk_size) {
            (Some(cpu), Some(memory), Some(disk_size)) => Some(SandboxSpecs {
                cpu,
                memory,
                disk_size,
            }),
            _ => None,
        }
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct RepoYamlConfig {
    repo_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cpu: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    memory: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    disk_size: Option<String>,
}

impl RepoYamlConfig {
    fn split(self) -> (SandboxMode, Option<SandboxSpecs>) {
        let mode = SandboxMode::Repo {
            repo_url: self.repo_url,
            branch: self.branch,
        };
        let specs = match (self.cpu, self.memory, self.disk_size) {
            (Some(cpu), Some(memory), Some(disk_size)) => Some(SandboxSpecs {
                cpu,
                memory,
                disk_size,
            }),
            _ => None,
        };
        (mode, specs)
    }
}

impl WorkflowSandboxYaml {
    fn from_runtime(d: &WorkflowSandboxDecl) -> Self {
        match d {
            WorkflowSandboxDecl::Preexisting { name } => {
                WorkflowSandboxYaml::Preexisting { name: name.clone() }
            }
            WorkflowSandboxDecl::Provisioned { name, mode, specs } => {
                let (cpu, memory, disk_size) = match specs {
                    Some(s) => (
                        Some(s.cpu.clone()),
                        Some(s.memory.clone()),
                        Some(s.disk_size.clone()),
                    ),
                    None => (None, None, None),
                };
                match mode {
                    SandboxMode::Scratch => {
                        let config = if cpu.is_some() || memory.is_some() || disk_size.is_some() {
                            Some(ScratchYamlConfig {
                                cpu,
                                memory,
                                disk_size,
                            })
                        } else {
                            None
                        };
                        WorkflowSandboxYaml::Scratch {
                            name: name.clone(),
                            config,
                        }
                    }
                    SandboxMode::Repo { repo_url, branch } => WorkflowSandboxYaml::Repo {
                        name: name.clone(),
                        config: RepoYamlConfig {
                            repo_url: repo_url.clone(),
                            branch: branch.clone(),
                            cpu,
                            memory,
                            disk_size,
                        },
                    },
                    // TODO(library-space): expose as a `WorkflowSandboxYaml::LibrarySpace`
                    // variant once workflow declarations support library-space sandboxes.
                    SandboxMode::LibrarySpace { slug, .. } => {
                        tracing::warn!(
                            %slug,
                            "library-space sandbox cannot be serialized to workflow yaml yet"
                        );
                        todo!("library-space sandbox setup not yet implemented")
                    }
                }
            }
        }
    }

    fn into_runtime(self) -> WorkflowSandboxDecl {
        match self {
            WorkflowSandboxYaml::Preexisting { name } => WorkflowSandboxDecl::Preexisting { name },
            WorkflowSandboxYaml::Scratch { name, config } => WorkflowSandboxDecl::Provisioned {
                name,
                mode: SandboxMode::Scratch,
                specs: config.unwrap_or_default().into_specs(),
            },
            WorkflowSandboxYaml::Repo { name, config } => {
                let (mode, specs) = config.split();
                WorkflowSandboxDecl::Provisioned { name, mode, specs }
            }
        }
    }
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WorkflowTriggerYaml {
    #[default]
    Manual,
    Webhook {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
    },
}

impl WorkflowTriggerYaml {
    fn from_runtime(t: &WorkflowTrigger) -> Self {
        match t {
            WorkflowTrigger::Manual => WorkflowTriggerYaml::Manual,
            WorkflowTrigger::Webhook { provider, .. } => WorkflowTriggerYaml::Webhook {
                provider: provider.clone(),
            },
        }
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WorkflowStepYaml {
    AgentStep {
        name: String,
        skill: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sandbox: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sandbox_mode: Option<SandboxAgentMode>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_seconds: Option<u64>,
    },
}

impl WorkflowStepYaml {
    fn from_runtime(s: &WorkflowStepDef) -> Self {
        match s {
            WorkflowStepDef::AgentStep {
                name,
                skill,
                sandbox,
                sandbox_mode,
                timeout_seconds,
            } => WorkflowStepYaml::AgentStep {
                name: name.clone(),
                skill: skill.clone(),
                sandbox: sandbox.clone(),
                sandbox_mode: *sandbox_mode,
                timeout_seconds: *timeout_seconds,
            },
        }
    }

    fn into_runtime(self) -> WorkflowStepDef {
        match self {
            WorkflowStepYaml::AgentStep {
                name,
                skill,
                sandbox,
                sandbox_mode,
                timeout_seconds,
            } => WorkflowStepDef::AgentStep {
                name,
                skill,
                sandbox,
                sandbox_mode,
                timeout_seconds,
            },
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn render_workflow_yaml(
    doc_id: WorkflowDefinitionId,
    name: &str,
    description: Option<&str>,
    trigger: &WorkflowTrigger,
    steps: &[WorkflowStepDef],
    sandboxes: &[WorkflowSandboxDecl],
    created_at: &str,
    updated_at: &str,
) -> String {
    let yaml = WorkflowYaml {
        id: Some(doc_id.into()),
        name: name.to_string(),
        description: description.map(|s| s.to_string()),
        trigger: WorkflowTriggerYaml::from_runtime(trigger),
        sandboxes: sandboxes
            .iter()
            .map(WorkflowSandboxYaml::from_runtime)
            .collect(),
        steps: steps.iter().map(WorkflowStepYaml::from_runtime).collect(),
        created: created_at.to_string(),
        updated: updated_at.to_string(),
    };
    serde_yaml::to_string(&yaml).unwrap_or_else(|e| format!("# yaml render error: {e}\n"))
}

pub struct ParsedWorkflowFile {
    pub parsed: ParsedFile,
    pub trigger: WorkflowTrigger,
    pub steps: Vec<WorkflowStepDef>,
    pub sandboxes: Vec<WorkflowSandboxDecl>,
    pub description: Option<String>,
}

/// `None` when the YAML is malformed or no name can be derived.
pub fn parse_workflow_yaml(content: &str, path: &str) -> Option<ParsedWorkflowFile> {
    let workspace_name = workspace_name_from_workflow_path(path);
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }

    let yaml: WorkflowYaml = serde_yaml::from_str(trimmed).ok()?;

    let (workflow_id, has_id) = match yaml.id {
        Some(uuid) => (WorkflowDefinitionId::from(uuid), true),
        None => (WorkflowDefinitionId::new(), false),
    };

    let name = if !yaml.name.is_empty() {
        yaml.name
    } else {
        name_from_filename(path)?
    };

    let trigger = match yaml.trigger {
        WorkflowTriggerYaml::Manual => WorkflowTrigger::Manual,
        WorkflowTriggerYaml::Webhook { provider } => WorkflowTrigger::Webhook {
            provider,
            secret: String::new(),
        },
    };

    let steps: Vec<WorkflowStepDef> = yaml
        .steps
        .into_iter()
        .map(WorkflowStepYaml::into_runtime)
        .collect();
    let sandboxes: Vec<WorkflowSandboxDecl> = yaml
        .sandboxes
        .into_iter()
        .map(WorkflowSandboxYaml::into_runtime)
        .collect();

    let description = yaml.description;

    let id_uuid = uuid::Uuid::from(workflow_id);
    let id_prefix = id_uuid.to_string()[..8].to_string();
    let slug = slugify(&name);
    let rendered = render_workflow_yaml(
        workflow_id,
        &name,
        description.as_deref(),
        &trigger,
        &steps,
        &sandboxes,
        &yaml.created,
        &yaml.updated,
    );

    let file = SyncedFile {
        doc_id: id_uuid,
        doc_type: DocType::Workflow,
        workspace_id: None,
        workspace_name,
        slug,
        id_prefix,
        created_at: yaml.created,
        updated_at: yaml.updated,
        title: name,
        body: description.clone().unwrap_or_default(),
        tags: Vec::new(),
        original_path: Some(path.to_string()),
        rendered,
    };

    let needs_rewrite = !has_id || file.relative_path() != path;

    Some(ParsedWorkflowFile {
        parsed: ParsedFile {
            file,
            needs_rewrite,
        },
        trigger,
        steps,
        sandboxes,
        description,
    })
}

/// `runtime/workspaces/{ws}/workflows/*.yml` → `Some(ws)`;
/// `runtime/workflows/*.yml` → `None` (global).
pub fn workspace_name_from_workflow_path(relative_path: &str) -> Option<String> {
    let parts: Vec<&str> = relative_path.split('/').collect();
    if parts.len() >= 5
        && parts[0] == "runtime"
        && parts[1] == "workspaces"
        && parts[3] == "workflows"
    {
        Some(parts[2].to_string())
    } else {
        None
    }
}

/// Build an `UpstreamOp::WriteFile` for a workflow definition.
/// Production code reaches this via `LibrarySynced::to_synced_file`
/// on `WorkflowDefinition`; this free function is the test/library
/// convenience that mirrors the old `UpstreamOp::for_workflow`.
#[allow(clippy::too_many_arguments)]
pub fn upstream_op_for_workflow(
    workflow_id: WorkflowDefinitionId,
    workspace_id: Option<WorkspaceId>,
    workspace_name: Option<&str>,
    name: &str,
    description: Option<&str>,
    trigger: WorkflowTrigger,
    steps: Vec<WorkflowStepDef>,
    sandboxes: Vec<WorkflowSandboxDecl>,
    created_at: &str,
    updated_at: &str,
    original_path: Option<String>,
) -> UpstreamOp {
    let id = uuid::Uuid::from(workflow_id);
    let id_prefix = id.to_string()[..8].to_string();
    let rendered = render_workflow_yaml(
        workflow_id,
        name,
        description,
        &trigger,
        &steps,
        &sandboxes,
        created_at,
        updated_at,
    );
    UpstreamOp::WriteFile(Box::new(SyncedFile {
        doc_id: id,
        doc_type: DocType::Workflow,
        workspace_id,
        workspace_name: workspace_name.map(|s| s.to_string()),
        slug: slugify(name),
        id_prefix,
        created_at: created_at.to_string(),
        updated_at: updated_at.to_string(),
        title: name.to_string(),
        body: description.unwrap_or_default().to_string(),
        tags: Vec::new(),
        original_path,
        rendered,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synced(op: &UpstreamOp) -> &SyncedFile {
        match op {
            UpstreamOp::WriteFile(s) => s,
            _ => panic!("expected WriteFile variant"),
        }
    }

    fn sample_steps() -> Vec<WorkflowStepDef> {
        vec![WorkflowStepDef::AgentStep {
            name: "investigate".to_string(),
            skill: "alert-investigator".to_string(),
            sandbox: Some("investigation".to_string()),
            sandbox_mode: None,
            timeout_seconds: Some(120),
        }]
    }

    fn sample_sandboxes() -> Vec<WorkflowSandboxDecl> {
        vec![WorkflowSandboxDecl::Provisioned {
            name: "investigation".to_string(),
            mode: SandboxMode::Scratch,
            specs: None,
        }]
    }

    fn build_op(
        workflow_id: WorkflowDefinitionId,
        name: &str,
        description: Option<&str>,
        trigger: WorkflowTrigger,
        sandboxes: Vec<WorkflowSandboxDecl>,
    ) -> UpstreamOp {
        upstream_op_for_workflow(
            workflow_id,
            None,
            None,
            name,
            description,
            trigger,
            sample_steps(),
            sandboxes,
            "2026-04-29T00:00:00Z",
            "2026-04-29T00:00:00Z",
            None,
        )
    }

    #[test]
    fn workflow_yaml_roundtrip_global() {
        let id = WorkflowDefinitionId::new();
        let original = upstream_op_for_workflow(
            id,
            None,
            None,
            "alert-response",
            Some("Investigate Honeycomb alerts"),
            WorkflowTrigger::Webhook {
                provider: Some("honeycomb".to_string()),
                secret: "whsec_should-not-be-serialized".to_string(),
            },
            sample_steps(),
            sample_sandboxes(),
            "2026-04-27T00:00:00Z",
            "2026-04-27T00:00:00Z",
            None,
        );

        let content = original.content();
        assert!(!content.contains("whsec_should-not-be-serialized"));

        let path = synced(&original).relative_path();
        let parsed = parse_workflow_yaml(&content, &path).expect("parses");
        assert!(!parsed.parsed.needs_rewrite);

        let s = &parsed.parsed.file;
        assert_eq!(s.doc_id, uuid::Uuid::from(id));
        assert_eq!(s.workspace_id, None);
        assert_eq!(s.workspace_name, None);
        assert_eq!(s.title, "alert-response");
        assert_eq!(
            parsed.description.as_deref(),
            Some("Investigate Honeycomb alerts")
        );
        match parsed.trigger {
            WorkflowTrigger::Webhook {
                ref provider,
                ref secret,
            } => {
                assert_eq!(provider.as_deref(), Some("honeycomb"));
                assert_eq!(secret, "");
            }
            _ => panic!("expected webhook trigger"),
        }
        assert_eq!(parsed.steps.len(), 1);
    }

    #[test]
    fn workflow_yaml_roundtrip_preserves_sandboxes() {
        let id = WorkflowDefinitionId::new();
        let original = build_op(
            id,
            "alert-response",
            None,
            WorkflowTrigger::Manual,
            sample_sandboxes(),
        );
        let content = original.content();
        let path = synced(&original).relative_path();
        let parsed = parse_workflow_yaml(&content, &path).expect("parses");
        assert_eq!(parsed.sandboxes.len(), 1);
        assert_eq!(parsed.sandboxes[0].name(), "investigation");
        assert!(matches!(
            parsed.sandboxes[0],
            WorkflowSandboxDecl::Provisioned {
                mode: SandboxMode::Scratch,
                ..
            }
        ));
    }

    #[test]
    fn workflow_yaml_workspace_scoped_path() {
        let id = WorkflowDefinitionId::new();
        let id_prefix = &id.to_string()[..8];
        let path = format!("runtime/workspaces/team/workflows/foo-{id_prefix}.yml");
        assert_eq!(
            workspace_name_from_workflow_path(&path),
            Some("team".to_string())
        );
    }

    #[test]
    fn workflow_yaml_without_id_needs_rewrite() {
        let content = "\
name: simple-flow
trigger:
  type: manual
steps:
  - type: agent_step
    name: step
    skill: my-skill
";
        let path = "runtime/workflows/simple-flow.yml";
        let parsed = parse_workflow_yaml(content, path).expect("parses");
        assert!(parsed.parsed.needs_rewrite);
        assert_eq!(parsed.parsed.file.title, "simple-flow");
        assert!(matches!(parsed.trigger, WorkflowTrigger::Manual));
    }

    #[test]
    fn workflow_yaml_returns_none_for_empty() {
        assert!(parse_workflow_yaml("", "runtime/workflows/x.yml").is_none());
    }

    #[test]
    fn workflow_yaml_renders_typed_sandboxes_with_nested_config() {
        let id = WorkflowDefinitionId::new();
        let original = build_op(
            id,
            "alert-response",
            None,
            WorkflowTrigger::Manual,
            vec![
                WorkflowSandboxDecl::Provisioned {
                    name: "investigation".to_string(),
                    mode: SandboxMode::Scratch,
                    specs: None,
                },
                WorkflowSandboxDecl::Provisioned {
                    name: "build".to_string(),
                    mode: SandboxMode::Repo {
                        repo_url: "https://github.com/GaloyMoney/drua".to_string(),
                        branch: Some("main".to_string()),
                    },
                    specs: Some(SandboxSpecs {
                        cpu: "1".to_string(),
                        memory: "2Gi".to_string(),
                        disk_size: "20Gi".to_string(),
                    }),
                },
                WorkflowSandboxDecl::Preexisting {
                    name: "oncall-shell".to_string(),
                },
            ],
        );
        let content = original.content();
        assert!(content.contains("type: scratch"));
        assert!(content.contains("type: repo"));
        assert!(content.contains("type: preexisting"));
        assert!(content.contains("config:"));
        let path = synced(&original).relative_path();
        let parsed = parse_workflow_yaml(&content, &path).expect("parses");
        assert_eq!(parsed.sandboxes.len(), 3);
    }

    #[test]
    fn workflow_yaml_roundtrip_preserves_preexisting_sandbox() {
        let id = WorkflowDefinitionId::new();
        let original = build_op(
            id,
            "uses-existing",
            None,
            WorkflowTrigger::Manual,
            vec![WorkflowSandboxDecl::Preexisting {
                name: "investigation".to_string(),
            }],
        );
        let content = original.content();
        assert!(content.contains("type: preexisting"));

        let path = synced(&original).relative_path();
        let parsed = parse_workflow_yaml(&content, &path).expect("parses");
        assert_eq!(parsed.sandboxes.len(), 1);
        assert!(matches!(
            &parsed.sandboxes[0],
            WorkflowSandboxDecl::Preexisting { name } if name == "investigation"
        ));
    }
}
