//! Tiny MCP server that serves on-disk fixtures as tools, one tool per file.
//!
//! Each `*.json` file under the fixtures dir becomes a tool whose name is the
//! file stem. Calling the tool returns the fixture's pre-canned `CallToolResult`.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, ErrorCode, ErrorData, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use rmcp::{RoleServer, ServerHandler};
use serde::Deserialize;
use tracing::instrument;

#[derive(Deserialize, Clone, Debug)]
pub struct Fixture {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub as_used_by: Vec<String>,
    pub upstream: Option<UpstreamEnvelope>,
    pub compose: Option<ComposeEnvelope>,
    /// Carried through verbatim for harnesses that need it (e.g. the
    /// `concourse-shaped-but-wrong-tool` fixture). Ignored by the stub.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub masquerade_tool_name: Option<String>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct UpstreamEnvelope {
    #[serde(default)]
    pub is_error: bool,
    pub content: Vec<ContentPart>,
    #[serde(default)]
    pub structured_content: Option<serde_json::Value>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct ComposeEnvelope {
    pub script: String,
    #[serde(default)]
    pub inner_stub_upstream: Option<UpstreamEnvelope>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text {
        text: String,
    },
    Image {
        #[serde(rename = "mimeType")]
        mime_type: String,
        data: String,
    },
}

#[derive(Clone, Debug)]
pub struct FakeUpstream {
    fixtures: Arc<BTreeMap<String, Fixture>>,
}

impl FakeUpstream {
    pub fn load(dir: &Path) -> Result<Self> {
        let mut fixtures = BTreeMap::new();
        let read = std::fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))?;
        for entry in read {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let body = std::fs::read_to_string(&path)
                .with_context(|| format!("read {}", path.display()))?;
            let fixture: Fixture =
                serde_json::from_str(&body).with_context(|| format!("parse {}", path.display()))?;
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .ok_or_else(|| anyhow::anyhow!("missing file stem: {}", path.display()))?
                .to_string();
            fixtures.insert(stem, fixture);
        }
        if fixtures.is_empty() {
            anyhow::bail!("no .json fixtures found under {}", dir.display());
        }
        Ok(Self {
            fixtures: Arc::new(fixtures),
        })
    }

    pub fn fixture_count(&self) -> usize {
        self.fixtures.len()
    }

    pub fn tool_names(&self) -> impl Iterator<Item = &str> {
        self.fixtures.keys().map(|s| s.as_str())
    }

    pub fn into_service(self) -> StreamableHttpService<Self, LocalSessionManager> {
        let mut config = StreamableHttpServerConfig::default().disable_allowed_hosts();
        config.stateful_mode = false;
        config.json_response = true;
        StreamableHttpService::new(
            move || Ok(self.clone()),
            LocalSessionManager::default().into(),
            config,
        )
    }
}

fn empty_object_schema() -> serde_json::Map<String, serde_json::Value> {
    let mut m = serde_json::Map::new();
    m.insert("type".into(), serde_json::Value::String("object".into()));
    m.insert(
        "properties".into(),
        serde_json::Value::Object(serde_json::Map::new()),
    );
    m.insert(
        "additionalProperties".into(),
        serde_json::Value::Bool(false),
    );
    m
}

fn fixture_to_tool(name: &str, fix: &Fixture) -> Tool {
    let mut t = Tool::default();
    t.name = name.to_string().into();
    let desc = if fix.description.is_empty() {
        format!("Fake upstream fixture {name}")
    } else {
        fix.description.clone()
    };
    t.description = Some(desc.into());
    t.input_schema = Arc::new(empty_object_schema());
    t
}

fn envelope_to_result(env: UpstreamEnvelope) -> CallToolResult {
    let content: Vec<Content> = env
        .content
        .into_iter()
        .map(|p| match p {
            ContentPart::Text { text } => Content::text(text),
            ContentPart::Image { mime_type, data } => Content::image(data, mime_type),
        })
        .collect();
    let mut result = CallToolResult::success(content);
    if env.is_error {
        result.is_error = Some(true);
    }
    result.structured_content = env.structured_content;
    result
}

impl ServerHandler for FakeUpstream {
    fn get_info(&self) -> ServerInfo {
        let names: Vec<&str> = self.fixtures.keys().map(|s| s.as_str()).collect();
        let instructions = format!(
            "Fake MCP upstream — {} canned fixtures served from disk.\nFixtures: {}",
            names.len(),
            names.join(", ")
        );
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(instructions)
    }

    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tools = self
            .fixtures
            .iter()
            .map(|(name, fix)| fixture_to_tool(name, fix))
            .collect();
        Ok(ListToolsResult {
            tools,
            next_cursor: None,
            meta: None,
        })
    }

    #[instrument(name = "fake_mcp_upstream.call_tool", skip(self, _ctx), fields(tool = %request.name))]
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let name: &str = request.name.as_ref();
        let fix = self.fixtures.get(name).ok_or_else(|| {
            ErrorData::new(
                ErrorCode::METHOD_NOT_FOUND,
                format!("unknown fixture tool: {name}"),
                None::<serde_json::Value>,
            )
        })?;
        // Compose-script fixtures (no `upstream`) return the script body as a
        // single text part so probes still get a deterministic response.
        let env = if let Some(u) = &fix.upstream {
            u.clone()
        } else if let Some(c) = &fix.compose {
            UpstreamEnvelope {
                is_error: false,
                content: vec![ContentPart::Text {
                    text: c.script.clone(),
                }],
                structured_content: None,
            }
        } else {
            return Err(ErrorData::new(
                ErrorCode::INTERNAL_ERROR,
                format!("fixture {name} has neither `upstream` nor `compose`"),
                None::<serde_json::Value>,
            ));
        };
        Ok(envelope_to_result(env))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestDir(std::path::PathBuf);

    impl TestDir {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("fake-mcp-upstream-test-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn write(&self, name: &str, body: &str) {
            std::fs::write(self.0.join(name), body).unwrap();
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const OBJ_SMALL: &str = r#"{
        "name":"obj-small","description":"small JSON object","as_used_by":[],
        "upstream":{"is_error":false,"content":[{"type":"text","text":"{\"ok\":true}"}],"structured_content":null}
    }"#;

    const IS_ERROR: &str = r#"{
        "name":"is-error","description":"error","as_used_by":[],
        "upstream":{"is_error":true,"content":[{"type":"text","text":"boom"}],"structured_content":null}
    }"#;

    const MIXED: &str = r#"{
        "name":"mixed","description":"text+image+text","as_used_by":[],
        "upstream":{"is_error":false,"content":[
          {"type":"text","text":"a"},
          {"type":"image","mimeType":"image/png","data":"AAAA"},
          {"type":"text","text":"b"}
        ],"structured_content":null}
    }"#;

    const COMPOSE_ONLY: &str = r#"{
        "name":"co","description":"compose","as_used_by":[],
        "compose":{"script":"throw new Error(\"x\");","inner_stub_upstream":null}
    }"#;

    const WITH_STRUCT: &str = r#"{
        "name":"ws","description":"with struct","as_used_by":[],
        "upstream":{"is_error":false,"content":[{"type":"text","text":"hi"}],"structured_content":{"k":1}}
    }"#;

    #[test]
    fn loads_every_json_in_dir() {
        let dir = TestDir::new();
        dir.write("obj-small.json", OBJ_SMALL);
        dir.write("is-error.json", IS_ERROR);
        dir.write("not-a-fixture.txt", "ignored");
        let up = FakeUpstream::load(dir.path()).expect("load");
        assert_eq!(up.fixture_count(), 2);
        let names: Vec<&str> = up.tool_names().collect();
        assert!(names.contains(&"obj-small"));
        assert!(names.contains(&"is-error"));
    }

    #[test]
    fn empty_dir_errors() {
        let dir = TestDir::new();
        let err = FakeUpstream::load(dir.path()).unwrap_err();
        assert!(err.to_string().contains("no .json fixtures"));
    }

    #[test]
    fn upstream_round_trips_to_call_tool_result() {
        let dir = TestDir::new();
        dir.write("obj-small.json", OBJ_SMALL);
        let up = FakeUpstream::load(dir.path()).unwrap();
        let env = up
            .fixtures
            .get("obj-small")
            .unwrap()
            .upstream
            .clone()
            .unwrap();
        let result = envelope_to_result(env);
        assert_eq!(result.is_error, Some(false));
        assert_eq!(result.content.len(), 1);
    }

    #[test]
    fn is_error_marks_error_flag() {
        let dir = TestDir::new();
        dir.write("is-error.json", IS_ERROR);
        let up = FakeUpstream::load(dir.path()).unwrap();
        let env = up
            .fixtures
            .get("is-error")
            .unwrap()
            .upstream
            .clone()
            .unwrap();
        let result = envelope_to_result(env);
        assert_eq!(result.is_error, Some(true));
    }

    #[test]
    fn structured_content_passes_through() {
        let dir = TestDir::new();
        dir.write("ws.json", WITH_STRUCT);
        let up = FakeUpstream::load(dir.path()).unwrap();
        let env = up.fixtures.get("ws").unwrap().upstream.clone().unwrap();
        let result = envelope_to_result(env);
        assert_eq!(result.structured_content, Some(serde_json::json!({"k": 1})));
    }

    #[test]
    fn mixed_content_parts_includes_image() {
        let dir = TestDir::new();
        dir.write("mixed.json", MIXED);
        let up = FakeUpstream::load(dir.path()).unwrap();
        let env = up.fixtures.get("mixed").unwrap().upstream.clone().unwrap();
        assert_eq!(env.content.len(), 3);
        assert!(matches!(env.content[1], ContentPart::Image { .. }));
    }

    #[test]
    fn compose_only_fixture_falls_back_to_script_body() {
        let dir = TestDir::new();
        dir.write("co.json", COMPOSE_ONLY);
        let up = FakeUpstream::load(dir.path()).unwrap();
        let fix = up.fixtures.get("co").unwrap();
        assert!(fix.upstream.is_none());
        assert!(fix.compose.as_ref().unwrap().script.contains("throw"));
    }

    #[test]
    fn list_tools_returns_one_per_fixture() {
        let dir = TestDir::new();
        dir.write("a.json", OBJ_SMALL);
        dir.write("b.json", IS_ERROR);
        dir.write("c.json", COMPOSE_ONLY);
        let up = FakeUpstream::load(dir.path()).unwrap();
        let mut tool_names: Vec<String> = up
            .fixtures
            .iter()
            .map(|(name, fix)| fixture_to_tool(name, fix).name.to_string())
            .collect();
        tool_names.sort();
        assert_eq!(tool_names, vec!["a", "b", "c"]);
    }
}
