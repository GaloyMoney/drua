use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use rmcp::model::{CallToolResult, Content};
use rmcp::schemars;
use style_agent_core::request_log::RequestLogEntry;
use style_agent_core::search::SearchEngine;
use style_agent_core::store::SearchResult;

use crate::config::StyleAgentConfig;

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SearchCodeParams {
    /// The search query — use a code snippet for best results, not natural language
    #[schemars(
        description = "The search query. Pass a code snippet (e.g. the pattern you are about to write) for best results — code-as-query gives much better similarity matches than natural language"
    )]
    query: String,

    /// Maximum number of results to return (default: 5)
    #[schemars(description = "Maximum number of results to return (default: 5)")]
    limit: Option<u64>,

    /// Filter results to a specific repository
    #[schemars(description = "Filter results to a specific repository name")]
    repo: Option<String>,

    /// Filter results to a specific language (e.g. 'rust', 'bats', 'bash')
    #[schemars(
        description = "Filter results to a specific language (e.g. 'rust', 'bats', 'bash')"
    )]
    language: Option<String>,

    /// Filter results to a specific primary label
    #[schemars(
        description = "Filter results to a specific primary label. Values: entity, entity_command, entity_query, entity_hydration, entity_event, published_event, new_entity, service_method, service, repository, error, authorization, value_object, domain_primitives, api, job, event_handler, type_conversion, test, config, none (unlabeled chunks)"
    )]
    label: Option<String>,
}

/// Facade for style-agent endpoints, to be held by the MCP gateway.
#[derive(Debug, Clone)]
pub struct StyleAgentEndpoints {
    pub(crate) search_engine: Arc<SearchEngine>,
}

impl StyleAgentEndpoints {
    /// Execute a `search_code` query.
    pub async fn search_code(
        &self,
        params: SearchCodeParams,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let start = Instant::now();
        let limit = params.limit.unwrap_or(10);
        let repo_filter = params.repo.as_deref();
        let language_filter = params.language.as_deref();
        let label_filter = params.label.as_deref();

        let filters = serde_json::json!({
            "label": params.label,
            "language": params.language,
            "repo": params.repo,
            "limit": limit,
        });

        let outcome = self
            .search_engine
            .search(
                &params.query,
                limit,
                repo_filter,
                language_filter,
                label_filter,
                None,
                None,
            )
            .await;

        let latency_ms = start.elapsed().as_millis() as i64;

        match outcome {
            Ok(results) => {
                let result_count = results.len() as i64;
                let top_score = results.first().map(|r| r.score as f64);

                fire_and_forget_log(
                    &self.search_engine,
                    "search_code",
                    &params.query,
                    &filters,
                    result_count,
                    top_score,
                    latency_ms,
                    None,
                );

                if results.is_empty() {
                    return Ok(CallToolResult::success(vec![Content::text(
                        "No matching code patterns found.",
                    )]));
                }

                let formatted = format_search_results(&results);
                Ok(CallToolResult::success(vec![Content::text(formatted)]))
            }
            Err(e) => {
                fire_and_forget_log(
                    &self.search_engine,
                    "search_code",
                    &params.query,
                    &filters,
                    0,
                    None,
                    latency_ms,
                    Some(&e.to_string()),
                );
                Err(rmcp::ErrorData::internal_error(
                    format!("Search failed: {e}"),
                    None,
                ))
            }
        }
    }
}

/// Initialise the style-agent endpoints from config.
///
/// Returns `Ok(None)` when `db_path` is empty — style-agent is disabled and
/// the server starts normally without it.  Returns `Ok(Some(...))` on success.
/// Returns `Err(...)` when `db_path` is set but initialisation fails — the
/// caller should treat this as a hard error.
pub fn init_endpoints(config: &StyleAgentConfig) -> anyhow::Result<Option<StyleAgentEndpoints>> {
    use style_agent_core::embedder::Embedder;
    use style_agent_core::store::VectorStore;

    if config.db_path.is_empty() {
        tracing::info!("Style-agent disabled (db_path is empty)");
        return Ok(None);
    }

    tracing::info!(db_path = %config.db_path, "Initialising style-agent");

    let store = VectorStore::new(std::path::Path::new(&config.db_path))?;
    store.ensure_collection()?;
    store.ensure_anti_pattern_tables()?;

    let embedder = Embedder::new()?;
    let search_engine = Arc::new(SearchEngine::new(embedder, store));

    tracing::info!("Style-agent search engine ready");
    Ok(Some(StyleAgentEndpoints { search_engine }))
}

/// Generate an ISO 8601 UTC timestamp string.
fn iso8601_now() -> String {
    use std::time::SystemTime;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    let (year, month, day) = days_to_date(days);

    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
}

/// Convert days since Unix epoch to (year, month, day).
fn days_to_date(days: u64) -> (u64, u64, u64) {
    // Algorithm from https://howardhinnant.github.io/date_algorithms.html
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Fire-and-forget: spawn a blocking task to INSERT the log entry.
#[allow(clippy::too_many_arguments)]
fn fire_and_forget_log(
    engine: &Arc<SearchEngine>,
    tool: &str,
    query: &str,
    filters: &serde_json::Value,
    result_count: i64,
    top_score: Option<f64>,
    latency_ms: i64,
    error: Option<&str>,
) {
    let engine = Arc::clone(engine);
    let entry = RequestLogEntry {
        ts: iso8601_now(),
        tool: tool.to_string(),
        query: query.to_string(),
        filters: Some(filters.to_string()),
        result_count,
        top_score,
        latency_ms,
        error: error.map(|s| s.to_string()),
    };

    tokio::task::spawn_blocking(move || {
        if let Err(e) = engine.log_request(&entry) {
            tracing::warn!(error = %e, "Failed to log request");
        }
    });
}

fn format_search_results(results: &[SearchResult]) -> String {
    let mut output = format!("Found {} matching code patterns:\n", results.len());

    // Build label distribution summary
    let mut label_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for r in results {
        let label = r.labels.first().map(|s| s.as_str()).unwrap_or("unlabeled");
        *label_counts.entry(label).or_default() += 1;
    }

    if !label_counts.is_empty() {
        let labels_summary: Vec<String> = label_counts
            .iter()
            .map(|(k, v)| format!("{k} ({v})"))
            .collect();
        output.push_str(&format!("Labels: {}\n", labels_summary.join(", ")));
    }
    output.push('\n');

    for (i, result) in results.iter().enumerate() {
        output.push_str(&format!(
            "### Result {} — `{}` ({})\n",
            i + 1,
            result.file_path,
            result.repo,
        ));

        if let Some(ref name) = result.entity_name {
            output.push_str(&format!("**Entity**: `{name}`\n"));
        }

        if !result.labels.is_empty() {
            output.push_str(&format!("**Label**: {}\n", result.labels.join(", ")));
        }

        output.push_str(&format!(
            "**Type**: `{}` | **Lines**: {}-{} | **Score**: {:.3}\n\n",
            result.chunk_type, result.line_start, result.line_end, result.score,
        ));

        let lang = if result.language.is_empty() {
            "rust"
        } else {
            &result.language
        };
        output.push_str(&format!("```{lang}\n"));
        output.push_str(&result.content);
        if !result.content.ends_with('\n') {
            output.push('\n');
        }
        output.push_str("```\n\n");
    }

    output
}
