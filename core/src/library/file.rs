use sha1::{Digest, Sha1};

use crate::primitives::{NoteId, SkillId, WorkflowDefinitionId, WorkspaceId};
use crate::workflow::{WorkflowStepDef, WorkflowTrigger};

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GitFileHash(String);

impl GitFileHash {
    pub(super) fn from_sha1(hex: String) -> Self {
        Self(hex)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeFile {
    Note {
        doc_id: NoteId,
        workspace_id: WorkspaceId,
        workspace_name: String,
        title: String,
        body: String,
        tags: Vec<String>,
        created_at: String,
        updated_at: String,
        slug: String,
        id_prefix: String,
    },
    Skill {
        doc_id: SkillId,
        workspace_id: Option<WorkspaceId>,
        workspace_name: Option<String>,
        name: String,
        description: String,
        body: String,
        created_at: String,
        updated_at: String,
        slug: String,
        id_prefix: String,
        /// Original on-disk path before canonicalisation. The `WriteToRuntime`
        /// job removes this path if it differs from canonical `relative_path()`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        original_path: Option<String>,
    },
    /// YAML body. Webhook secret is **not** serialized — it stays
    /// DB-only so secrets don't leak into git.
    Workflow {
        doc_id: WorkflowDefinitionId,
        workspace_id: Option<WorkspaceId>,
        workspace_name: Option<String>,
        name: String,
        description: Option<String>,
        trigger: WorkflowTrigger,
        steps: Vec<WorkflowStepDef>,
        created_at: String,
        updated_at: String,
        slug: String,
        id_prefix: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        original_path: Option<String>,
    },
    GitKeep {
        workspace_name: String,
        subdir: String,
    },
    /// Job runner removes the entire `runtime/workspaces/{workspace_name}/`
    /// directory from the library repo and pushes.
    WorkspaceCleanup { workspace_name: String },
}

impl RuntimeFile {
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
        RuntimeFile::Note {
            doc_id: note_id,
            workspace_id,
            workspace_name: workspace_name.to_string(),
            title: title.to_string(),
            body: body.to_string(),
            tags: tags.to_vec(),
            created_at: created_at.to_string(),
            updated_at: updated_at.to_string(),
            slug: slugify(title),
            id_prefix: note_id.to_string()[..8].to_string(),
        }
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
        RuntimeFile::Skill {
            doc_id: skill_id,
            workspace_id,
            workspace_name: workspace_name.map(|s| s.to_string()),
            name: name.to_string(),
            description: description.to_string(),
            body: body.to_string(),
            created_at: created_at.to_string(),
            updated_at: updated_at.to_string(),
            slug: slugify(name),
            id_prefix: skill_id.to_string()[..8].to_string(),
            original_path,
        }
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
        created_at: &str,
        updated_at: &str,
        original_path: Option<String>,
    ) -> Self {
        RuntimeFile::Workflow {
            doc_id: workflow_id,
            workspace_id,
            workspace_name: workspace_name.map(|s| s.to_string()),
            name: name.to_string(),
            description: description.map(|s| s.to_string()),
            trigger,
            steps,
            created_at: created_at.to_string(),
            updated_at: updated_at.to_string(),
            slug: slugify(name),
            id_prefix: workflow_id.to_string()[..8].to_string(),
            original_path,
        }
    }

    pub fn searchable_fields(&self) -> Option<SearchableFields> {
        match self {
            RuntimeFile::Note {
                doc_id,
                workspace_id,
                title,
                body,
                tags,
                ..
            } => Some(SearchableFields {
                doc_id: uuid::Uuid::from(*doc_id),
                doc_type: DocType::Note,
                workspace_id: uuid::Uuid::from(*workspace_id),
                title: title.clone(),
                body: body.clone(),
                tags: tags.clone(),
            }),
            RuntimeFile::Skill {
                doc_id,
                workspace_id,
                name,
                description,
                ..
            } => Some(SearchableFields {
                doc_id: uuid::Uuid::from(*doc_id),
                doc_type: DocType::Skill,
                workspace_id: workspace_id
                    .map(uuid::Uuid::from)
                    .unwrap_or(uuid::Uuid::nil()),
                title: name.clone(),
                body: description.clone(),
                tags: Vec::new(),
            }),
            RuntimeFile::Workflow {
                doc_id,
                workspace_id,
                name,
                description,
                ..
            } => Some(SearchableFields {
                doc_id: uuid::Uuid::from(*doc_id),
                doc_type: DocType::Workflow,
                workspace_id: workspace_id
                    .map(uuid::Uuid::from)
                    .unwrap_or(uuid::Uuid::nil()),
                title: name.clone(),
                body: description.clone().unwrap_or_default(),
                tags: Vec::new(),
            }),
            RuntimeFile::GitKeep { .. } | RuntimeFile::WorkspaceCleanup { .. } => None,
        }
    }

    pub(super) fn relative_path(&self) -> String {
        match self {
            RuntimeFile::Note {
                workspace_name,
                slug,
                id_prefix,
                ..
            } => format!(
                "runtime/workspaces/{}/notes/{}-{}.md",
                workspace_name, slug, id_prefix
            ),
            RuntimeFile::Skill {
                workspace_name,
                slug,
                id_prefix,
                ..
            } => match workspace_name {
                Some(ws) => format!("runtime/workspaces/{}/skills/{}-{}.md", ws, slug, id_prefix),
                None => format!("runtime/skills/{}-{}.md", slug, id_prefix),
            },
            RuntimeFile::Workflow {
                workspace_name,
                slug,
                id_prefix,
                ..
            } => match workspace_name {
                Some(ws) => format!(
                    "runtime/workspaces/{}/workflows/{}-{}.yml",
                    ws, slug, id_prefix
                ),
                None => format!("runtime/workflows/{}-{}.yml", slug, id_prefix),
            },
            RuntimeFile::GitKeep {
                workspace_name,
                subdir,
            } => {
                format!("runtime/workspaces/{workspace_name}/{subdir}/.gitkeep")
            }
            RuntimeFile::WorkspaceCleanup { workspace_name } => {
                format!("runtime/workspaces/{workspace_name}")
            }
        }
    }

    pub(crate) fn content(&self) -> String {
        match self {
            RuntimeFile::Note {
                doc_id,
                title,
                body,
                tags,
                created_at,
                updated_at,
                ..
            } => {
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
            RuntimeFile::Skill {
                doc_id,
                name,
                description,
                body,
                created_at,
                updated_at,
                ..
            } => {
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
            RuntimeFile::Workflow {
                doc_id,
                name,
                description,
                trigger,
                steps,
                created_at,
                updated_at,
                ..
            } => render_workflow_yaml(
                *doc_id,
                name,
                description.as_deref(),
                trigger,
                steps,
                created_at,
                updated_at,
            ),
            RuntimeFile::GitKeep { .. } | RuntimeFile::WorkspaceCleanup { .. } => String::new(),
        }
    }

    pub(super) fn commit_message(&self) -> String {
        match self {
            RuntimeFile::Note {
                slug, id_prefix, ..
            } => format!("note: {}-{}", slug, id_prefix),
            RuntimeFile::Skill {
                slug, id_prefix, ..
            } => format!("skill: {}-{}", slug, id_prefix),
            RuntimeFile::Workflow {
                slug, id_prefix, ..
            } => format!("workflow: {}-{}", slug, id_prefix),
            RuntimeFile::GitKeep {
                workspace_name,
                subdir,
            } => {
                format!("workspace: scaffold {workspace_name}/{subdir}")
            }
            RuntimeFile::WorkspaceCleanup { workspace_name } => {
                format!("workspace: delete {workspace_name}")
            }
        }
    }

    pub(crate) fn original_path(&self) -> Option<&str> {
        match self {
            RuntimeFile::Skill { original_path, .. } => original_path.as_deref(),
            RuntimeFile::Workflow { original_path, .. } => original_path.as_deref(),
            _ => None,
        }
    }

    pub(crate) fn set_original_path(&mut self, path: String) {
        match self {
            RuntimeFile::Skill { original_path, .. } => *original_path = Some(path),
            RuntimeFile::Workflow { original_path, .. } => *original_path = Some(path),
            _ => {}
        }
    }

    /// Identical to `git hash-object`.
    pub fn file_hash(&self) -> GitFileHash {
        let content = self.content();
        let header = format!("blob {}\0", content.len());
        let mut hasher = Sha1::new();
        hasher.update(header.as_bytes());
        hasher.update(content.as_bytes());
        GitFileHash(format!("{:x}", hasher.finalize()))
    }
}

pub struct ParsedSkillFile {
    pub file: RuntimeFile,
    /// File lacks proper frontmatter (or `id:`) and should be rewritten
    /// with canonical headers after entity creation.
    pub needs_rewrite: bool,
}

/// Handles three formats:
/// 1. Full frontmatter (canonical) — `needs_rewrite = false`.
/// 2. Frontmatter without `id:` — generates a new `SkillId`, `needs_rewrite = true`.
/// 3. No frontmatter (human-authored) — generates a new `SkillId`, `needs_rewrite = true`.
///
/// Returns `None` only if the content has no recognisable `# heading`.
pub fn parse_skill_markdown(content: &str, path: &str) -> Option<ParsedSkillFile> {
    let workspace_name = workspace_name_from_skill_path(path);
    let content = content.trim();
    if content.is_empty() {
        return None;
    }

    let mut parsed = if content.starts_with("---") {
        parse_with_frontmatter(content, workspace_name, path)?
    } else {
        parse_without_frontmatter(content, workspace_name, path)?
    };

    parsed.file.set_original_path(path.to_string());

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
fn parse_with_frontmatter(
    content: &str,
    workspace_name: Option<String>,
    path: &str,
) -> Option<ParsedSkillFile> {
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
        // Legacy: name from heading, description from first paragraph.
        let desc = fm.description.unwrap_or(h_desc);
        (h_name, desc, h_body)
    } else {
        let name = name_from_filename(path)?;
        let desc = fm.description.unwrap_or_default();
        let body = after_fm.trim().to_string();
        (name, desc, body)
    };

    let slug = slugify(&name);
    let id_prefix = skill_id.to_string()[..8].to_string();

    let file = RuntimeFile::Skill {
        doc_id: skill_id,
        workspace_id: None,
        workspace_name,
        name,
        description,
        body,
        created_at: fm.created.unwrap_or_default(),
        updated_at: fm.updated.unwrap_or_default(),
        slug,
        id_prefix,
        original_path: None,
    };

    // Canonical only when id + name in frontmatter AND path matches.
    let needs_rewrite = !has_id || !has_fm_name || file.relative_path() != path;

    Some(ParsedSkillFile {
        file,
        needs_rewrite,
    })
}

/// Name priority: `# Heading` → filename.
fn parse_without_frontmatter(
    content: &str,
    workspace_name: Option<String>,
    path: &str,
) -> Option<ParsedSkillFile> {
    let (name, description, body) = if let Some(parsed) = parse_heading_and_body(content) {
        parsed
    } else {
        let name = name_from_filename(path)?;
        (name, String::new(), content.trim().to_string())
    };

    let skill_id = SkillId::new();
    let slug = slugify(&name);
    let id_prefix = skill_id.to_string()[..8].to_string();

    Some(ParsedSkillFile {
        file: RuntimeFile::Skill {
            doc_id: skill_id,
            workspace_id: None,
            workspace_name,
            name,
            description,
            body,
            created_at: String::new(),
            updated_at: String::new(),
            slug,
            id_prefix,
            original_path: None,
        },
        needs_rewrite: true,
    })
}

/// Expects `# Name` heading, optional description, optional `---` body separator.
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

/// `runtime/skills/ci-check-019dc56a.md` → `"Ci Check"`. Strips `.md` and
/// trailing `-{8-hex}` id prefix, then title-cases hyphen-separated words.
fn name_from_filename(path: &str) -> Option<String> {
    let filename = path.rsplit('/').next()?;
    let stem = filename.strip_suffix(".md").unwrap_or(filename);

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
    /// Files lacking `id:` get a fresh `WorkflowDefinitionId` on import
    /// and the file is rewritten with the canonical id afterwards.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<uuid::Uuid>,
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    trigger: WorkflowTriggerYaml,
    steps: Vec<WorkflowStepYaml>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    created: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    updated: String,
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
                timeout_seconds,
            } => WorkflowStepYaml::AgentStep {
                name: name.clone(),
                skill: skill.clone(),
                sandbox: sandbox.clone(),
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
                timeout_seconds,
            } => WorkflowStepDef::AgentStep {
                name,
                skill,
                sandbox,
                timeout_seconds,
            },
        }
    }
}

fn render_workflow_yaml(
    doc_id: WorkflowDefinitionId,
    name: &str,
    description: Option<&str>,
    trigger: &WorkflowTrigger,
    steps: &[WorkflowStepDef],
    created_at: &str,
    updated_at: &str,
) -> String {
    let yaml = WorkflowYaml {
        id: Some(doc_id.into()),
        name: name.to_string(),
        description: description.map(|s| s.to_string()),
        trigger: WorkflowTriggerYaml::from_runtime(trigger),
        steps: steps.iter().map(WorkflowStepYaml::from_runtime).collect(),
        created: created_at.to_string(),
        updated: updated_at.to_string(),
    };
    serde_yaml::to_string(&yaml).unwrap_or_else(|e| format!("# yaml render error: {e}\n"))
}

pub struct ParsedWorkflowFile {
    pub file: RuntimeFile,
    /// Set when the sync job should rewrite the file with canonical
    /// headers (missing `id:`, non-canonical path, stale format).
    pub needs_rewrite: bool,
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
        // Empty secret signals "mint on create / preserve on update"
        // — the upsert path handles it (see `Workflows::create`).
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

    let slug = slugify(&name);
    let id_prefix = workflow_id.to_string()[..8].to_string();

    let file = RuntimeFile::Workflow {
        doc_id: workflow_id,
        workspace_id: None,
        workspace_name,
        name,
        description: yaml.description,
        trigger,
        steps,
        created_at: yaml.created,
        updated_at: yaml.updated,
        slug,
        id_prefix,
        original_path: Some(path.to_string()),
    };

    let needs_rewrite = !has_id || file.relative_path() != path;

    Some(ParsedWorkflowFile {
        file,
        needs_rewrite,
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

fn slugify(title: &str) -> String {
    title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_skill_markdown_roundtrip() {
        let skill_id = SkillId::new();
        let id_prefix = &skill_id.to_string()[..8];
        let original = RuntimeFile::for_skill(
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

        match parsed.file {
            RuntimeFile::Skill {
                doc_id,
                workspace_name,
                name,
                description,
                body,
                created_at,
                updated_at,
                original_path,
                ..
            } => {
                assert_eq!(doc_id, skill_id);
                assert_eq!(workspace_name.as_deref(), Some("my-workspace"));
                assert_eq!(name, "Deploy Script");
                assert_eq!(description, "Deploys the app to production");
                assert_eq!(body, "#!/bin/bash\necho deploy");
                assert_eq!(created_at, "2025-01-01T00:00:00Z");
                assert_eq!(updated_at, "2025-06-01T00:00:00Z");
                assert_eq!(original_path.as_deref(), Some(path.as_str()));
            }
            _ => panic!("expected Skill variant"),
        }
    }

    #[test]
    fn parse_skill_markdown_global() {
        let skill_id = SkillId::new();
        let id_prefix = &skill_id.to_string()[..8];
        let original = RuntimeFile::for_skill(
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

        match parsed.file {
            RuntimeFile::Skill {
                doc_id,
                workspace_id,
                workspace_name,
                name,
                ..
            } => {
                assert_eq!(doc_id, skill_id);
                assert_eq!(workspace_id, None);
                assert_eq!(workspace_name, None);
                assert_eq!(name, "Global Skill");
            }
            _ => panic!("expected Skill variant"),
        }
    }

    #[test]
    fn parse_skill_markdown_no_heading_falls_back_to_filename() {
        let path = "runtime/skills/test.md";
        let parsed = parse_skill_markdown("not markdown", path).expect("filename fallback");
        assert!(parsed.needs_rewrite);
        match &parsed.file {
            RuntimeFile::Skill { name, body, .. } => {
                assert_eq!(name, "Test");
                assert_eq!(body, "not markdown");
            }
            _ => panic!("expected Skill variant"),
        }
    }

    #[test]
    fn parse_skill_markdown_returns_none_for_empty() {
        let path = "runtime/skills/.gitkeep";
        assert!(parse_skill_markdown("", path).is_none());
    }

    #[test]
    fn parse_skill_markdown_bad_uuid_generates_new_id() {
        let path = "runtime/skills/test.md";
        // Invalid UUID in frontmatter falls back to generating a new SkillId
        let parsed = parse_skill_markdown("---\nid: not-a-uuid\n---\n\n# Name\n", path).unwrap();
        assert!(parsed.needs_rewrite);
        match &parsed.file {
            RuntimeFile::Skill { name, .. } => assert_eq!(name, "Name"),
            _ => panic!("expected Skill variant"),
        }
    }

    #[test]
    fn parse_skill_hash_matches_original() {
        let skill_id = SkillId::new();
        let id_prefix = &skill_id.to_string()[..8];
        let original = RuntimeFile::for_skill(
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

        match &parsed.file {
            RuntimeFile::Skill {
                name,
                description,
                body,
                original_path,
                ..
            } => {
                assert_eq!(name, "My Cool Skill");
                assert_eq!(description, "Does something useful");
                assert_eq!(body, "The body template");
                assert_eq!(original_path.as_deref(), Some(path));
            }
            _ => panic!("expected Skill variant"),
        }
    }

    #[test]
    fn parse_skill_markdown_without_frontmatter_no_body() {
        let content = "# Simple Skill\n\nJust a description, no body";
        let path = "runtime/skills/simple-skill.md";
        let parsed = parse_skill_markdown(content, path).expect("should parse");
        assert!(parsed.needs_rewrite);

        match &parsed.file {
            RuntimeFile::Skill {
                name,
                description,
                body,
                ..
            } => {
                assert_eq!(name, "Simple Skill");
                assert_eq!(description, "Just a description, no body");
                assert!(body.is_empty());
            }
            _ => panic!("expected Skill variant"),
        }
    }

    #[test]
    fn parse_skill_markdown_frontmatter_without_id() {
        let content = "---\ncreated: 2025-01-01\nupdated: 2025-06-01\n---\n\n# My Skill\n\nDescription\n\n---\n\nBody";
        let path = "runtime/workspaces/team/skills/my-skill.md";
        let parsed = parse_skill_markdown(content, path).expect("should parse");
        assert!(parsed.needs_rewrite);

        match &parsed.file {
            RuntimeFile::Skill {
                name,
                workspace_name,
                original_path,
                ..
            } => {
                assert_eq!(name, "My Skill");
                assert_eq!(workspace_name.as_deref(), Some("team"));
                assert_eq!(original_path.as_deref(), Some(path));
            }
            _ => panic!("expected Skill variant"),
        }
    }

    #[test]
    fn parse_skill_markdown_empty_frontmatter_generates_id() {
        let content = "---\n---\n\n# Name\n\nDesc";
        let path = "runtime/skills/name.md";
        let parsed = parse_skill_markdown(content, path).expect("should parse");
        assert!(parsed.needs_rewrite);

        match &parsed.file {
            RuntimeFile::Skill { name, .. } => {
                assert_eq!(name, "Name");
            }
            _ => panic!("expected Skill variant"),
        }
    }

    /// Legacy format (id present, heading-based name) triggers needs_rewrite
    /// so it gets migrated to canonical frontmatter format.
    #[test]
    fn parse_skill_markdown_legacy_format_needs_rewrite() {
        let content = "---\nid: 019dc56a-502f-7ce3-9623-877d6b3a1c5c\ncreated: 2025-01-01\nupdated: 2025-06-01\n---\n\n# CI Check\n\nInvestigate CI\n\n---\n\nBody here";
        let path = "runtime/skills/ci-check-019dc56a.md";
        let parsed = parse_skill_markdown(content, path).expect("should parse");
        assert!(parsed.needs_rewrite, "legacy heading format needs rewrite");

        match &parsed.file {
            RuntimeFile::Skill {
                name,
                description,
                body,
                ..
            } => {
                assert_eq!(name, "CI Check");
                assert_eq!(description, "Investigate CI");
                assert_eq!(body, "Body here");
            }
            _ => panic!("expected Skill variant"),
        }
    }

    /// Frontmatter with name+description but no heading in body.
    #[test]
    fn parse_skill_markdown_frontmatter_name_desc() {
        let content = "---\nid: 019dc56a-502f-7ce3-9623-877d6b3a1c5c\nname: \"CI Check\"\ndescription: \"Investigate CI status\"\ncreated: 2025-01-01\nupdated: 2025-06-01\n---\n\nDo the thing.\n";
        let path = "runtime/skills/ci-check-019dc56a.md";
        let parsed = parse_skill_markdown(content, path).expect("should parse");
        assert!(
            !parsed.needs_rewrite,
            "canonical format should not need rewrite"
        );

        match &parsed.file {
            RuntimeFile::Skill {
                name,
                description,
                body,
                ..
            } => {
                assert_eq!(name, "CI Check");
                assert_eq!(description, "Investigate CI status");
                assert_eq!(body, "Do the thing.");
            }
            _ => panic!("expected Skill variant"),
        }
    }

    /// Frontmatter with only name, no heading — body is everything after frontmatter.
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

        match &parsed.file {
            RuntimeFile::Skill { name, body, .. } => {
                assert_eq!(name, "My Tool");
                assert_eq!(body, "Just raw content, no heading");
            }
            _ => panic!("expected Skill variant"),
        }
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
            sandbox: None,
            timeout_seconds: Some(120),
        }]
    }

    #[test]
    fn workflow_yaml_roundtrip_global() {
        let id = WorkflowDefinitionId::new();
        let original = RuntimeFile::for_workflow(
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
            "2026-04-27T00:00:00Z",
            "2026-04-27T00:00:00Z",
        );

        let content = original.content();
        assert!(!content.contains("whsec_should-not-be-serialized"));

        let path = original.relative_path();
        let parsed = parse_workflow_yaml(&content, &path).expect("parses");
        assert!(!parsed.needs_rewrite);

        match parsed.file {
            RuntimeFile::Workflow {
                doc_id,
                workspace_id,
                workspace_name,
                name,
                description,
                trigger,
                steps,
                ..
            } => {
                assert_eq!(doc_id, id);
                assert_eq!(workspace_id, None);
                assert_eq!(workspace_name, None);
                assert_eq!(name, "alert-response");
                assert_eq!(description.as_deref(), Some("Investigate Honeycomb alerts"));
                match trigger {
                    WorkflowTrigger::Webhook { provider, secret } => {
                        assert_eq!(provider.as_deref(), Some("honeycomb"));
                        assert_eq!(secret, "");
                    }
                    _ => panic!("expected webhook trigger"),
                }
                assert_eq!(steps.len(), 1);
            }
            _ => panic!("expected workflow"),
        }
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
        assert!(parsed.needs_rewrite);
        match parsed.file {
            RuntimeFile::Workflow { name, trigger, .. } => {
                assert_eq!(name, "simple-flow");
                assert!(matches!(trigger, WorkflowTrigger::Manual));
            }
            _ => panic!("expected workflow"),
        }
    }

    #[test]
    fn workflow_yaml_returns_none_for_empty() {
        assert!(parse_workflow_yaml("", "runtime/workflows/x.yml").is_none());
    }
}
