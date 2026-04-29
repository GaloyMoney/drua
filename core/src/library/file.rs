use sha1::{Digest, Sha1};

use crate::primitives::{NoteId, SkillId, WorkflowDefinitionId, WorkspaceId};
use crate::sandbox::{SandboxAgentMode, SandboxMode, SandboxSpecs};
use crate::workflow::{WorkflowSandboxDecl, WorkflowStepDef, WorkflowTrigger};

use super::synced::{slugify, ParsedFile, SyncedFile};

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GitFileHash(String);

impl GitFileHash {
    pub(super) fn from_sha1(hex: String) -> Self {
        Self(hex)
    }

    /// Identical to `git hash-object`: hashes `blob {len}\0{bytes}`.
    pub fn from_blob_bytes(bytes: &[u8]) -> Self {
        let header = format!("blob {}\0", bytes.len());
        let mut hasher = Sha1::new();
        hasher.update(header.as_bytes());
        hasher.update(bytes);
        Self(format!("{:x}", hasher.finalize()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for GitFileHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocType {
    Note,
    Skill,
    Workflow,
}

impl DocType {
    pub fn as_str(&self) -> &'static str {
        match self {
            DocType::Note => "note",
            DocType::Skill => "skill",
            DocType::Workflow => "workflow",
        }
    }

    pub fn subdir(&self) -> &'static str {
        match self {
            DocType::Note => "notes",
            DocType::Skill => "skills",
            DocType::Workflow => "workflows",
        }
    }

    pub fn ext(&self) -> &'static str {
        match self {
            DocType::Note => "md",
            DocType::Skill => "md",
            DocType::Workflow => "yml",
        }
    }
}

pub struct SearchableFields {
    pub doc_id: uuid::Uuid,
    pub doc_type: DocType,
    pub workspace_id: uuid::Uuid,
    pub title: String,
    pub body: String,
    pub tags: Vec<String>,
}

impl SearchableFields {
    pub fn text_for_embedding(&self) -> String {
        format!("{}\n\n{}", self.title, self.body)
    }
}

/// One operation to apply to the upstream git library — inbox payload +
/// `WriteToRuntime` job input. `Synced` carries an entity-backed file
/// write; `WorkspaceInit`/`WorkspaceCleanup` are scaffolding / teardown ops
/// with no backing entity.
///
/// `Synced` is boxed because `SyncedFile` is several hundred bytes and
/// dominates enum size — boxing keeps `UpstreamOp` cheap to pass around
/// (clippy: `large_enum_variant`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UpstreamOp {
    WriteFile(Box<SyncedFile>),
    /// Job runner writes `.gitkeep` markers in `notes/`, `skills/`, and
    /// `workflows/` under `runtime/workspaces/{workspace_name}/` in a
    /// single commit + push.
    WorkspaceInit {
        workspace_name: String,
    },
    /// Job runner removes the entire `runtime/workspaces/{workspace_name}/`
    /// directory from the library repo and pushes.
    WorkspaceCleanup {
        workspace_name: String,
    },
}

impl UpstreamOp {
    #[allow(clippy::too_many_arguments)]
    pub fn for_note(
        note_id: NoteId,
        workspace_id: WorkspaceId,
        workspace_name: &str,
        title: &str,
        body: &str,
        tags: &[String],
        created_at: &str,
        updated_at: &str,
    ) -> Self {
        let id = uuid::Uuid::from(note_id);
        let id_prefix = id.to_string()[..8].to_string();
        let rendered = render_note_markdown(id, title, body, tags, created_at, updated_at);
        UpstreamOp::WriteFile(Box::new(SyncedFile {
            doc_id: id,
            doc_type: DocType::Note,
            workspace_id: Some(workspace_id),
            workspace_name: Some(workspace_name.to_string()),
            slug: slugify(title),
            id_prefix,
            created_at: created_at.to_string(),
            updated_at: updated_at.to_string(),
            title: title.to_string(),
            body: body.to_string(),
            tags: tags.to_vec(),
            original_path: None,
            rendered,
        }))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn for_skill(
        skill_id: SkillId,
        workspace_id: Option<WorkspaceId>,
        workspace_name: Option<&str>,
        name: &str,
        description: &str,
        body: &str,
        created_at: &str,
        updated_at: &str,
    ) -> Self {
        Self::for_skill_with_original_path(
            skill_id,
            workspace_id,
            workspace_name,
            name,
            description,
            body,
            created_at,
            updated_at,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn for_skill_with_original_path(
        skill_id: SkillId,
        workspace_id: Option<WorkspaceId>,
        workspace_name: Option<&str>,
        name: &str,
        description: &str,
        body: &str,
        created_at: &str,
        updated_at: &str,
        original_path: Option<String>,
    ) -> Self {
        let id = uuid::Uuid::from(skill_id);
        let id_prefix = id.to_string()[..8].to_string();
        let rendered = render_skill_markdown(id, name, description, body, created_at, updated_at);
        UpstreamOp::WriteFile(Box::new(SyncedFile {
            doc_id: id,
            doc_type: DocType::Skill,
            workspace_id,
            workspace_name: workspace_name.map(|s| s.to_string()),
            slug: slugify(name),
            id_prefix,
            created_at: created_at.to_string(),
            updated_at: updated_at.to_string(),
            title: name.to_string(),
            body: description.to_string(),
            tags: Vec::new(),
            original_path,
            rendered,
        }))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn for_workflow(
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
    ) -> Self {
        Self::for_workflow_with_original_path(
            workflow_id,
            workspace_id,
            workspace_name,
            name,
            description,
            trigger,
            steps,
            sandboxes,
            created_at,
            updated_at,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn for_workflow_with_original_path(
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
    ) -> Self {
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

    pub fn searchable_fields(&self) -> Option<SearchableFields> {
        match self {
            UpstreamOp::WriteFile(s) => Some(s.searchable_fields()),
            UpstreamOp::WorkspaceInit { .. } | UpstreamOp::WorkspaceCleanup { .. } => None,
        }
    }

    pub(super) fn relative_path(&self) -> String {
        match self {
            UpstreamOp::WriteFile(s) => s.relative_path(),
            UpstreamOp::WorkspaceInit { workspace_name }
            | UpstreamOp::WorkspaceCleanup { workspace_name } => {
                format!("runtime/workspaces/{workspace_name}")
            }
        }
    }

    pub(crate) fn content(&self) -> String {
        match self {
            UpstreamOp::WriteFile(s) => s.rendered.clone(),
            UpstreamOp::WorkspaceInit { .. } | UpstreamOp::WorkspaceCleanup { .. } => String::new(),
        }
    }

    pub(super) fn commit_message(&self) -> String {
        match self {
            UpstreamOp::WriteFile(s) => s.commit_message(),
            UpstreamOp::WorkspaceInit { workspace_name } => {
                format!("workspace: init {workspace_name}")
            }
            UpstreamOp::WorkspaceCleanup { workspace_name } => {
                format!("workspace: delete {workspace_name}")
            }
        }
    }

    pub(crate) fn original_path(&self) -> Option<&str> {
        match self {
            UpstreamOp::WriteFile(s) => s.original_path.as_deref(),
            _ => None,
        }
    }

    pub(crate) fn idempotency_key(&self) -> String {
        match self {
            UpstreamOp::WorkspaceInit { workspace_name } => {
                format!("workspace-init:{workspace_name}")
            }
            UpstreamOp::WorkspaceCleanup { workspace_name } => {
                format!("workspace-cleanup:{workspace_name}")
            }
            UpstreamOp::WriteFile(s) => s.file_hash().to_string(),
        }
    }

    /// `git hash-object` over the rendered bytes (empty for non-content variants).
    pub fn file_hash(&self) -> GitFileHash {
        GitFileHash::from_blob_bytes(self.content().as_bytes())
    }
}

pub fn render_note_markdown(
    doc_id: uuid::Uuid,
    title: &str,
    body: &str,
    tags: &[String],
    created_at: &str,
    updated_at: &str,
) -> String {
    let tags_str = tags
        .iter()
        .map(|t| format!("\"{}\"", t))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "---\nid: {}\ntags: [{}]\ncreated: {}\nupdated: {}\n---\n\n# {}\n\n{}\n",
        doc_id, tags_str, created_at, updated_at, title, body
    )
}

pub fn render_skill_markdown(
    doc_id: uuid::Uuid,
    name: &str,
    description: &str,
    body: &str,
    created_at: &str,
    updated_at: &str,
) -> String {
    format!(
        "---\nid: {}\nname: \"{}\"\ndescription: \"{}\"\ncreated: {}\nupdated: {}\n---\n\n{}\n",
        doc_id,
        name.replace('"', "\\\""),
        description.replace('"', "\\\""),
        created_at,
        updated_at,
        body
    )
}

/// Handles three formats:
/// 1. Full frontmatter (canonical) — `needs_rewrite = false`.
/// 2. Frontmatter without `id:` — generates a new `SkillId`, `needs_rewrite = true`.
/// 3. No frontmatter (human-authored) — generates a new `SkillId`, `needs_rewrite = true`.
///
/// Returns `None` only if the content has no recognisable form.
pub fn parse_skill_markdown(content: &str, path: &str) -> Option<ParsedFile> {
    let workspace_name = workspace_name_from_skill_path(path);
    let content = content.trim();
    if content.is_empty() {
        return None;
    }

    let mut parsed = if content.starts_with("---") {
        parse_skill_with_frontmatter(content, workspace_name, path)?
    } else {
        parse_skill_without_frontmatter(content, workspace_name, path)?
    };

    parsed.file.original_path = Some(path.to_string());
    Some(parsed)
}

#[derive(serde::Deserialize, Default)]
struct SkillFrontmatter {
    #[serde(default)]
    id: Option<uuid::Uuid>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    created: Option<String>,
    #[serde(default)]
    updated: Option<String>,
}

/// Name/description priority:
/// 1. Frontmatter `name:`/`description:` (canonical)
/// 2. `# Heading` / first paragraph (legacy)
/// 3. Filename (last resort)
fn parse_skill_with_frontmatter(
    content: &str,
    workspace_name: Option<String>,
    path: &str,
) -> Option<ParsedFile> {
    let rest = content.strip_prefix("---")?;
    let (frontmatter_str, after_fm) = rest.split_once("\n---")?;

    let fm: SkillFrontmatter = serde_yaml::from_str(frontmatter_str.trim()).unwrap_or_default();

    let (skill_id, has_id) = match fm.id {
        Some(uuid) => (SkillId::from(uuid), true),
        None => (SkillId::new(), false),
    };

    let has_fm_name = fm.name.is_some();

    let (name, description, body) = if let Some(fm_name) = fm.name {
        let desc = fm.description.unwrap_or_default();
        let body = after_fm.trim().to_string();
        (fm_name, desc, body)
    } else if let Some((h_name, h_desc, h_body)) = parse_heading_and_body(after_fm) {
        let desc = fm.description.unwrap_or(h_desc);
        (h_name, desc, h_body)
    } else {
        let name = name_from_filename(path)?;
        let desc = fm.description.unwrap_or_default();
        let body = after_fm.trim().to_string();
        (name, desc, body)
    };

    let id_uuid = uuid::Uuid::from(skill_id);
    let id_prefix = id_uuid.to_string()[..8].to_string();
    let slug = slugify(&name);
    let created_at = fm.created.unwrap_or_default();
    let updated_at = fm.updated.unwrap_or_default();
    let rendered = render_skill_markdown(
        id_uuid,
        &name,
        &description,
        &body,
        &created_at,
        &updated_at,
    );

    let file = SyncedFile {
        doc_id: id_uuid,
        doc_type: DocType::Skill,
        workspace_id: None,
        workspace_name,
        slug: slug.clone(),
        id_prefix,
        created_at,
        updated_at,
        title: name.clone(),
        body: description,
        tags: Vec::new(),
        original_path: None,
        rendered,
    };

    let needs_rewrite = !has_id || !has_fm_name || file.relative_path() != path;

    Some(ParsedFile {
        file,
        needs_rewrite,
    })
}

fn parse_skill_without_frontmatter(
    content: &str,
    workspace_name: Option<String>,
    path: &str,
) -> Option<ParsedFile> {
    let (name, description, body) = if let Some(parsed) = parse_heading_and_body(content) {
        parsed
    } else {
        let name = name_from_filename(path)?;
        (name, String::new(), content.trim().to_string())
    };

    let skill_id = SkillId::new();
    let id_uuid = uuid::Uuid::from(skill_id);
    let id_prefix = id_uuid.to_string()[..8].to_string();
    let slug = slugify(&name);
    let rendered = render_skill_markdown(id_uuid, &name, &description, &body, "", "");

    Some(ParsedFile {
        file: SyncedFile {
            doc_id: id_uuid,
            doc_type: DocType::Skill,
            workspace_id: None,
            workspace_name,
            slug,
            id_prefix,
            created_at: String::new(),
            updated_at: String::new(),
            title: name,
            body: description,
            tags: Vec::new(),
            original_path: None,
            rendered,
        },
        needs_rewrite: true,
    })
}

fn parse_heading_and_body(content: &str) -> Option<(String, String, String)> {
    let content = content.trim_start_matches('\n');

    let name_line = content.lines().next()?;
    let name = name_line.strip_prefix("# ")?.trim().to_string();
    if name.is_empty() {
        return None;
    }

    let after_name = &content[name_line.len()..].trim_start_matches('\n');

    let (description, body) = if let Some((desc, bod)) = after_name.split_once("\n---\n") {
        (desc.trim().to_string(), bod.trim().to_string())
    } else {
        (after_name.trim().to_string(), String::new())
    };

    Some((name, description, body))
}

/// `runtime/workspaces/{ws}/skills/*.md` → `Some(ws)`;
/// `runtime/skills/*.md` → `None` (global skill).
pub fn workspace_name_from_skill_path(relative_path: &str) -> Option<String> {
    let parts: Vec<&str> = relative_path.split('/').collect();
    if parts.len() >= 5 && parts[0] == "runtime" && parts[1] == "workspaces" && parts[3] == "skills"
    {
        Some(parts[2].to_string())
    } else {
        None
    }
}

/// `runtime/skills/ci-check-019dc56a.md` → `"Ci Check"`.
fn name_from_filename(path: &str) -> Option<String> {
    let filename = path.rsplit('/').next()?;
    let stem = filename
        .strip_suffix(".md")
        .or_else(|| filename.strip_suffix(".yml"))
        .unwrap_or(filename);

    let base = if let Some((prefix, suffix)) = stem.rsplit_once('-') {
        if suffix.len() == 8 && suffix.chars().all(|c| c.is_ascii_hexdigit()) {
            prefix
        } else {
            stem
        }
    } else {
        stem
    };

    if base.is_empty() {
        return None;
    }

    let name = base
        .split('-')
        .filter(|s| !s.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(c) => {
                    let upper: String = c.to_uppercase().collect();
                    format!("{upper}{}", chars.as_str())
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ");

    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn synced(file: &UpstreamOp) -> &SyncedFile {
        match file {
            UpstreamOp::WriteFile(s) => s,
            _ => panic!("expected Synced variant"),
        }
    }

    #[test]
    fn parse_skill_markdown_roundtrip() {
        let skill_id = SkillId::new();
        let id_prefix = &skill_id.to_string()[..8];
        let original = UpstreamOp::for_skill(
            skill_id,
            Some(WorkspaceId::new()),
            Some("my-workspace"),
            "Deploy Script",
            "Deploys the app to production",
            "#!/bin/bash\necho deploy",
            "2025-01-01T00:00:00Z",
            "2025-06-01T00:00:00Z",
        );

        let content = original.content();
        let path = format!("runtime/workspaces/my-workspace/skills/deploy-script-{id_prefix}.md");
        let parsed = parse_skill_markdown(&content, &path).expect("should parse");
        assert!(!parsed.needs_rewrite);

        let s = &parsed.file;
        assert_eq!(s.doc_id, uuid::Uuid::from(skill_id));
        assert_eq!(s.doc_type, DocType::Skill);
        assert_eq!(s.workspace_name.as_deref(), Some("my-workspace"));
        assert_eq!(s.title, "Deploy Script");
        assert_eq!(s.body, "Deploys the app to production");
        assert_eq!(s.created_at, "2025-01-01T00:00:00Z");
        assert_eq!(s.updated_at, "2025-06-01T00:00:00Z");
        assert_eq!(s.original_path.as_deref(), Some(path.as_str()));
    }

    #[test]
    fn parse_skill_markdown_global() {
        let skill_id = SkillId::new();
        let id_prefix = &skill_id.to_string()[..8];
        let original = UpstreamOp::for_skill(
            skill_id,
            None,
            None,
            "Global Skill",
            "A global skill",
            "body content",
            "",
            "",
        );

        let content = original.content();
        let path = format!("runtime/skills/global-skill-{id_prefix}.md");
        let parsed = parse_skill_markdown(&content, &path).expect("should parse global skill");
        assert!(!parsed.needs_rewrite);

        let s = &parsed.file;
        assert_eq!(s.doc_id, uuid::Uuid::from(skill_id));
        assert_eq!(s.workspace_id, None);
        assert_eq!(s.workspace_name, None);
        assert_eq!(s.title, "Global Skill");
    }

    #[test]
    fn parse_skill_markdown_no_heading_falls_back_to_filename() {
        let path = "runtime/skills/test.md";
        let parsed = parse_skill_markdown("not markdown", path).expect("filename fallback");
        assert!(parsed.needs_rewrite);
        assert_eq!(parsed.file.title, "Test");
        assert_eq!(parsed.file.body, "");
    }

    #[test]
    fn parse_skill_markdown_returns_none_for_empty() {
        let path = "runtime/skills/.gitkeep";
        assert!(parse_skill_markdown("", path).is_none());
    }

    #[test]
    fn parse_skill_markdown_bad_uuid_generates_new_id() {
        let path = "runtime/skills/test.md";
        let parsed = parse_skill_markdown("---\nid: not-a-uuid\n---\n\n# Name\n", path).unwrap();
        assert!(parsed.needs_rewrite);
        assert_eq!(parsed.file.title, "Name");
    }

    #[test]
    fn parse_skill_hash_matches_original() {
        let skill_id = SkillId::new();
        let id_prefix = &skill_id.to_string()[..8];
        let original = UpstreamOp::for_skill(
            skill_id,
            None,
            None,
            "Test",
            "desc",
            "body",
            "2025-01-01T00:00:00Z",
            "2025-01-01T00:00:00Z",
        );
        let original_hash = original.file_hash();

        let content = original.content();
        let path = format!("runtime/skills/test-{id_prefix}.md");
        let parsed = parse_skill_markdown(&content, &path).unwrap();
        let parsed_hash = parsed.file.file_hash();

        assert_eq!(original_hash, parsed_hash);
    }

    #[test]
    fn parse_skill_markdown_without_frontmatter() {
        let content = "# My Cool Skill\n\nDoes something useful\n\n---\n\nThe body template";
        let path = "runtime/skills/my-cool-skill.md";
        let parsed = parse_skill_markdown(content, path).expect("should parse");
        assert!(parsed.needs_rewrite);
        let s = &parsed.file;
        assert_eq!(s.title, "My Cool Skill");
        assert_eq!(s.body, "Does something useful");
        assert_eq!(s.original_path.as_deref(), Some(path));
    }

    #[test]
    fn parse_skill_markdown_without_frontmatter_no_body() {
        let content = "# Simple Skill\n\nJust a description, no body";
        let path = "runtime/skills/simple-skill.md";
        let parsed = parse_skill_markdown(content, path).expect("should parse");
        assert!(parsed.needs_rewrite);
        let s = &parsed.file;
        assert_eq!(s.title, "Simple Skill");
        assert_eq!(s.body, "Just a description, no body");
    }

    #[test]
    fn parse_skill_markdown_frontmatter_without_id() {
        let content = "---\ncreated: 2025-01-01\nupdated: 2025-06-01\n---\n\n# My Skill\n\nDescription\n\n---\n\nBody";
        let path = "runtime/workspaces/team/skills/my-skill.md";
        let parsed = parse_skill_markdown(content, path).expect("should parse");
        assert!(parsed.needs_rewrite);
        let s = &parsed.file;
        assert_eq!(s.title, "My Skill");
        assert_eq!(s.workspace_name.as_deref(), Some("team"));
        assert_eq!(s.original_path.as_deref(), Some(path));
    }

    #[test]
    fn parse_skill_markdown_empty_frontmatter_generates_id() {
        let content = "---\n---\n\n# Name\n\nDesc";
        let path = "runtime/skills/name.md";
        let parsed = parse_skill_markdown(content, path).expect("should parse");
        assert!(parsed.needs_rewrite);
        assert_eq!(parsed.file.title, "Name");
    }

    /// Legacy format (id present, heading-based name) triggers needs_rewrite
    /// so it gets migrated to canonical frontmatter format.
    #[test]
    fn parse_skill_markdown_legacy_format_needs_rewrite() {
        let content = "---\nid: 019dc56a-502f-7ce3-9623-877d6b3a1c5c\ncreated: 2025-01-01\nupdated: 2025-06-01\n---\n\n# CI Check\n\nInvestigate CI\n\n---\n\nBody here";
        let path = "runtime/skills/ci-check-019dc56a.md";
        let parsed = parse_skill_markdown(content, path).expect("should parse");
        assert!(parsed.needs_rewrite, "legacy heading format needs rewrite");
        let s = &parsed.file;
        assert_eq!(s.title, "CI Check");
        assert_eq!(s.body, "Investigate CI");
    }

    #[test]
    fn parse_skill_markdown_frontmatter_name_desc() {
        let content = "---\nid: 019dc56a-502f-7ce3-9623-877d6b3a1c5c\nname: \"CI Check\"\ndescription: \"Investigate CI status\"\ncreated: 2025-01-01\nupdated: 2025-06-01\n---\n\nDo the thing.\n";
        let path = "runtime/skills/ci-check-019dc56a.md";
        let parsed = parse_skill_markdown(content, path).expect("should parse");
        assert!(
            !parsed.needs_rewrite,
            "canonical format should not need rewrite"
        );
        let s = &parsed.file;
        assert_eq!(s.title, "CI Check");
        assert_eq!(s.body, "Investigate CI status");
    }

    #[test]
    fn parse_skill_markdown_frontmatter_only_filename_for_body() {
        let content =
            "---\nid: 019dc56a-502f-7ce3-9623-877d6b3a1c5c\n---\n\nJust raw content, no heading";
        let path = "runtime/skills/my-tool-019dc56a.md";
        let parsed = parse_skill_markdown(content, path).expect("should parse");
        assert!(
            parsed.needs_rewrite,
            "no name in frontmatter triggers rewrite"
        );
        assert_eq!(parsed.file.title, "My Tool");
    }

    #[test]
    fn name_from_filename_strips_id_prefix() {
        assert_eq!(
            name_from_filename("runtime/skills/ci-check-019dc56a.md"),
            Some("Ci Check".to_string())
        );
    }

    #[test]
    fn name_from_filename_no_id_prefix() {
        assert_eq!(
            name_from_filename("runtime/skills/my-cool-skill.md"),
            Some("My Cool Skill".to_string())
        );
    }

    #[test]
    fn workspace_name_from_skill_path_workspace_scoped() {
        let path = "runtime/workspaces/my-ws/skills/deploy-script-abc12345.md";
        assert_eq!(
            workspace_name_from_skill_path(path),
            Some("my-ws".to_string())
        );
    }

    #[test]
    fn workspace_name_from_skill_path_global() {
        let path = "runtime/skills/deploy-script-abc12345.md";
        assert_eq!(workspace_name_from_skill_path(path), None);
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

    #[test]
    fn workflow_yaml_roundtrip_global() {
        let id = WorkflowDefinitionId::new();
        let original = UpstreamOp::for_workflow(
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
        let original = UpstreamOp::for_workflow(
            id,
            None,
            None,
            "alert-response",
            None,
            WorkflowTrigger::Manual,
            sample_steps(),
            sample_sandboxes(),
            "2026-04-27T00:00:00Z",
            "2026-04-27T00:00:00Z",
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
        let original = UpstreamOp::for_workflow(
            id,
            None,
            None,
            "alert-response",
            None,
            WorkflowTrigger::Manual,
            sample_steps(),
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
            "2026-04-29T00:00:00Z",
            "2026-04-29T00:00:00Z",
        );
        let content = original.content();
        assert!(content.contains("type: scratch"));
        assert!(content.contains("type: repo"));
        assert!(content.contains("type: preexisting"));
        assert!(content.contains("config:"));
        // Round-trip back through the parser to confirm.
        let path = synced(&original).relative_path();
        let parsed = parse_workflow_yaml(&content, &path).expect("parses");
        assert_eq!(parsed.sandboxes.len(), 3);
    }

    #[test]
    fn workflow_yaml_roundtrip_preserves_preexisting_sandbox() {
        let id = WorkflowDefinitionId::new();
        let original = UpstreamOp::for_workflow(
            id,
            None,
            None,
            "uses-existing",
            None,
            WorkflowTrigger::Manual,
            sample_steps(),
            vec![WorkflowSandboxDecl::Preexisting {
                name: "investigation".to_string(),
            }],
            "2026-04-29T00:00:00Z",
            "2026-04-29T00:00:00Z",
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
