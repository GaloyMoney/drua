//! Compare a library's legacy and migrated preflight in QuickJS with local, read-only tool fixtures.
use js_engine::{JsEngine, ScriptSource, ScriptSourceProvider, SourceIdentity, ToolDispatcher};
use serde_json::{json, Value};
use std::{path::PathBuf, sync::Arc, time::Duration};

struct Fixture {
    root: PathBuf,
}
impl Fixture {
    fn text(&self, path: &str) -> Result<String, String> {
        let path = path.strip_prefix("space:").ok_or("invalid_path")?;
        if path
            .split('/')
            .any(|s| s.is_empty() || s == "." || s == "..")
            || path.contains('\\')
        {
            return Err("invalid_path".into());
        }
        std::fs::read_to_string(self.root.join("spaces").join(path)).map_err(|e| e.to_string())
    }
}
#[async_trait::async_trait]
impl ScriptSourceProvider for Fixture {
    async fn authorize(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
    async fn read(&self, path: &str, max: usize) -> Result<ScriptSource, String> {
        let text = self.text(path)?;
        if text.len() > max {
            return Err("size_limit".into());
        }
        Ok(ScriptSource {
            identity: SourceIdentity {
                path: path.into(),
                revision: "local-fixture".into(),
                blob_oid: String::new(),
                sha256: String::new(),
                byte_length: text.len(),
            },
            text,
        })
    }
}
#[async_trait::async_trait]
impl ToolDispatcher for Fixture {
    async fn call_tool(&self, name: &str, args: Value) -> Result<Value, String> {
        match name {
            "Read" => {
                let text = self.text(args["path"].as_str().ok_or("missing path")?)?;
                let mut lines: Vec<_> = text.split('\n').collect();
                if lines.last() == Some(&"") {
                    lines.pop();
                }
                let content = lines
                    .iter()
                    .enumerate()
                    .map(|(i, line)| {
                        format!("{:6}\t{}", i + 1, line.strip_suffix('\r').unwrap_or(line))
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(json!({"content":content}))
            }
            "library_search" => {
                let slug = args["space_slugs"][0].as_str().ok_or("missing slug")?;
                let hits: Vec<_> = args["paths"].as_array().ok_or("missing paths")?.iter().map(|path| {
                    let path = path.as_str().unwrap();
                    json!({"id":format!("space:{slug}/{path}"), "type":"space_file", "space_slug":slug, "relative_path":path})
                }).collect();
                Ok(json!({"hits":hits}))
            }
            "library_get_files" => {
                let files = args["ids"].as_array().ok_or("missing ids")?.iter().map(|id| {
                    let path = id.as_str().unwrap();
                    let (slug,relative) = path.strip_prefix("space:").unwrap().split_once('/').unwrap();
                    Ok(json!({"id":path, "type":"space_file", "space_slug":slug, "relative_path":relative, "body":self.text(path)?}))
                }).collect::<Result<Vec<_>,String>>()?;
                Ok(json!({"files":files}))
            }
            _ => Err(format!("fixture rejects tool {name}")),
        }
    }
}

#[tokio::main]
async fn main() {
    let roots: Vec<_> = std::env::args().skip(1).collect();
    assert_eq!(
        roots.len(),
        2,
        "usage: script_probe LEGACY_LIBRARY MIGRATED_LIBRARY"
    );
    let mut results = Vec::new();
    let encoding = tiktoken::get_encoding("cl100k_base").unwrap();
    for (name, root) in ["legacy", "loadScript"].into_iter().zip(roots) {
        let fixture = Arc::new(Fixture { root: root.into() });
        let script = fixture
            .text("space:library-curation/checks/compose-probe.js")
            .unwrap();
        let mut times = Vec::new();
        let mut last = None;
        let mut sources = Value::Null;
        for _ in 0..20 {
            let audit = Arc::default();
            let result = JsEngine::new()
                .execute_with_sources(
                    &script,
                    fixture.clone(),
                    Duration::from_secs(5),
                    Some(fixture.clone()),
                    Arc::clone(&audit),
                )
                .await
                .unwrap();
            times.push(result.execution_time.as_secs_f64() * 1000.0);
            sources = serde_json::to_value(&*audit.lock().unwrap()).unwrap();
            last = Some(result);
        }
        times.sort_by(f64::total_cmp);
        let last = last.unwrap();
        let input = serde_json::to_string(&json!({"script":script})).unwrap();
        eprintln!(
            "{name}: {} input tokens (cl100k_base)",
            encoding.encode(&input).len()
        );
        results.push(json!({"mode":name, "compose_input_bytes":serde_json::to_vec(&json!({"script":script})).unwrap().len(), "script_bytes":script.len(), "result_bytes":serde_json::to_vec(&last.value).unwrap().len(), "tool_calls":last.tool_calls_made, "client_round_trips":1, "median_local_ms":times[10], "result":last.value, "sources":sources["sources"]}));
    }
    assert_eq!(results[0]["result"], results[1]["result"]);
    println!("{}", serde_json::to_string_pretty(&results).unwrap());
}
