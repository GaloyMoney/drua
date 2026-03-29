use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use code_assistant_core::request_log::RequestLogEntry;
use code_assistant_core::search::SearchEngine;
use code_assistant_core::store::SearchResult;
use rmcp::model::{CallToolResult, Content};
use rmcp::schemars;

use crate::config::CodeAssistantConfig;
use crate::request_logger::{NoopRequestLogger, RequestLogger};

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

/// Facade for code-assistant endpoints, to be held by the MCP gateway.
#[derive(Clone)]
pub struct CodeAssistantEndpoints {
    pub(crate) search_engine: Arc<SearchEngine>,
    pub(crate) logger: Arc<dyn RequestLogger>,
}

impl std::fmt::Debug for CodeAssistantEndpoints {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodeAssistantEndpoints")
            .field("search_engine", &self.search_engine)
            .finish_non_exhaustive()
    }
}

impl CodeAssistantEndpoints {
    /// Create endpoints with a custom [`RequestLogger`] implementation.
    pub fn with_logger(search_engine: Arc<SearchEngine>, logger: Arc<dyn RequestLogger>) -> Self {
        Self {
            search_engine,
            logger,
        }
    }

    /// Run a search and return the raw results (for the web dashboard).
    pub async fn search_raw(
        &self,
        query: &str,
        limit: u64,
        label: Option<&str>,
    ) -> Result<Vec<SearchResult>, anyhow::Error> {
        let results = self
            .search_engine
            .search(query, limit, None, None, label, None, None)
            .await?;
        Ok(results)
    }

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
                let results_json = build_results_json(&results);

                fire_and_forget_log(
                    &self.logger,
                    "search_code",
                    &params.query,
                    &filters,
                    result_count,
                    top_score,
                    latency_ms,
                    None,
                    Some(&results_json),
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
                    &self.logger,
                    "search_code",
                    &params.query,
                    &filters,
                    0,
                    None,
                    latency_ms,
                    Some(&e.to_string()),
                    None,
                );
                Err(rmcp::ErrorData::internal_error(
                    format!("Search failed: {e}"),
                    None,
                ))
            }
        }
    }
}

/// Initialise the code-assistant endpoints from config (standalone SQLite logger).
///
/// Returns `Ok(None)` when `db_path` is empty — code assistant is disabled and
/// the server starts normally without it.  Returns `Ok(Some(...))` on success.
/// Returns `Err(...)` when `db_path` is set but initialisation fails — the
/// caller should treat this as a hard error.
pub fn init_endpoints(
    config: &CodeAssistantConfig,
) -> anyhow::Result<Option<CodeAssistantEndpoints>> {
    let logger: Arc<dyn RequestLogger> = Arc::new(NoopRequestLogger);
    init_endpoints_inner(config, logger)
}

/// Initialise endpoints with an external logger.
///
/// Returns `Ok(None)` when `db_path` is empty (code assistant disabled).
pub fn init_endpoints_with_logger(
    config: &CodeAssistantConfig,
    logger: Arc<dyn RequestLogger>,
) -> anyhow::Result<Option<CodeAssistantEndpoints>> {
    init_endpoints_inner(config, logger)
}

fn init_endpoints_inner(
    config: &CodeAssistantConfig,
    logger: Arc<dyn RequestLogger>,
) -> anyhow::Result<Option<CodeAssistantEndpoints>> {
    use code_assistant_core::embedder::Embedder;
    use code_assistant_core::store::VectorStore;

    if config.db_path.is_empty() {
        tracing::info!("Code assistant disabled (db_path is empty)");
        return Ok(None);
    }

    let db = std::path::Path::new(&config.db_path);
    if db.exists() {
        tracing::info!(db_path = %config.db_path, "Code assistant database found");
    } else {
        tracing::warn!(
            db_path = %config.db_path,
            "Code assistant database not found — run 'nix run .#prep-code-assistant' to bootstrap"
        );
        return Ok(None);
    }

    let store = VectorStore::new(db)?;
    store.ensure_collection()?;
    store.ensure_anti_pattern_tables()?;

    let embedder = Embedder::new()?;
    let search_engine = Arc::new(SearchEngine::new(embedder, store));

    tracing::info!("Code assistant search engine ready");
    Ok(Some(CodeAssistantEndpoints {
        search_engine,
        logger,
    }))
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

/// Build a compact JSON summary of returned search results for logging.
fn build_results_json(results: &[SearchResult]) -> String {
    let summaries: Vec<serde_json::Value> = results
        .iter()
        .map(|r| {
            serde_json::json!({
                "file": r.file_path,
                "repo": r.repo,
                "score": format!("{:.3}", r.score),
                "labels": r.labels,
                "lines": format!("{}-{}", r.line_start, r.line_end),
                "content": r.content,
            })
        })
        .collect();
    serde_json::to_string(&summaries).unwrap_or_default()
}

/// Fire-and-forget: spawn an async task to INSERT the log entry.
#[allow(clippy::too_many_arguments)]
fn fire_and_forget_log(
    logger: &Arc<dyn RequestLogger>,
    tool: &str,
    query: &str,
    filters: &serde_json::Value,
    result_count: i64,
    top_score: Option<f64>,
    latency_ms: i64,
    error: Option<&str>,
    results_json: Option<&str>,
) {
    let logger = Arc::clone(logger);
    let entry = RequestLogEntry {
        ts: iso8601_now(),
        tool: tool.to_string(),
        query: query.to_string(),
        filters: Some(filters.to_string()),
        result_count,
        top_score,
        latency_ms,
        error: error.map(|s| s.to_string()),
        results_json: results_json.map(|s| s.to_string()),
    };

    tokio::spawn(async move {
        if let Err(e) = logger.log_request(&entry).await {
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
