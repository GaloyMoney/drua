mod config;
mod error;
pub mod logs;
mod request_logger;

pub use config::CodeAssistantConfig;
pub use error::CodeAssistantError;
pub(crate) use request_logger::RequestLogger;

// Re-exports from code-assistant-core
pub use code_assistant_core::request_log::RequestLogEntry;
pub use code_assistant_core::search::SearchEngine;
pub use code_assistant_core::store::{LabelOriginCounts, SearchResult, KNOWN_PRIMARY_LABELS};

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use logs::CodeAssistantLogs;

/// Parameters for a code search query.
#[derive(Debug, serde::Deserialize)]
pub struct SearchCodeParams {
    /// The search query — use a code snippet for best results, not natural language
    pub query: String,
    /// Maximum number of results to return (default: 5)
    pub limit: Option<u64>,
    /// Filter results to a specific repository
    pub repo: Option<String>,
    /// Filter results to a specific language (e.g. 'rust', 'bats', 'bash')
    pub language: Option<String>,
    /// Filter results to a specific primary label
    pub label: Option<String>,
}

/// Code assistant service — holds the search engine and logger.
#[derive(Clone)]
pub struct CodeAssistant {
    search_engine: Arc<SearchEngine>,
    logs: Arc<CodeAssistantLogs>,
}

impl std::fmt::Debug for CodeAssistant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodeAssistant")
            .field("search_engine", &self.search_engine)
            .finish_non_exhaustive()
    }
}

/// Initialise the code assistant from config.
///
/// Returns `Ok(None)` when `db_path` is empty (code assistant disabled).
/// Accepts a shared embedder to avoid loading the ONNX model twice.
pub fn init(
    pool: &sqlx::PgPool,
    config: &CodeAssistantConfig,
    embedder: std::sync::Arc<code_assistant_core::embedder::Embedder>,
) -> Result<Option<CodeAssistant>, CodeAssistantError> {
    use code_assistant_core::store::VectorStore;

    if config.db_path.is_empty() {
        tracing::info!("Code assistant disabled (db_path is empty)");
        return Ok(None);
    }

    let db = std::path::Path::new(&config.db_path);
    if !db.exists() {
        tracing::warn!(
            db_path = %config.db_path,
            "Code assistant database not found — run 'nix run .#prep-code-assistant' to bootstrap"
        );
        return Ok(None);
    }

    tracing::info!(db_path = %config.db_path, "Code assistant database found");

    let store = VectorStore::new(db).map_err(|e| CodeAssistantError::Init(e.to_string()))?;
    store
        .ensure_collection()
        .map_err(|e| CodeAssistantError::Init(e.to_string()))?;
    store
        .ensure_anti_pattern_tables()
        .map_err(|e| CodeAssistantError::Init(e.to_string()))?;

    let search_engine = Arc::new(SearchEngine::new((*embedder).clone(), store));
    let logs = Arc::new(CodeAssistantLogs::new(pool));

    tracing::info!("Code assistant search engine ready");
    Ok(Some(CodeAssistant {
        search_engine,
        logs,
    }))
}

impl CodeAssistant {
    pub fn logs(&self) -> &Arc<CodeAssistantLogs> {
        &self.logs
    }

    /// Count labelled chunks grouped by their origin (human / model / heuristic).
    pub fn label_origin_counts(&self) -> Result<LabelOriginCounts, CodeAssistantError> {
        self.search_engine
            .label_origin_counts()
            .map_err(|e| CodeAssistantError::Search(e.to_string()))
    }

    /// Run a search and return the raw results (for the web dashboard).
    pub async fn search_raw(
        &self,
        query: &str,
        limit: u64,
        label: Option<&str>,
    ) -> Result<Vec<SearchResult>, CodeAssistantError> {
        let results = self
            .search_engine
            .search(query, limit, None, None, label, None, None)
            .await
            .map_err(|e| CodeAssistantError::Search(e.to_string()))?;
        Ok(results)
    }

    /// Execute a `search_code` query, returning formatted text.
    pub async fn search(&self, params: SearchCodeParams) -> Result<String, CodeAssistantError> {
        // Validate label filter before querying
        if let Some(ref label) = params.label {
            if label != "none" && !KNOWN_PRIMARY_LABELS.contains(&label.as_str()) {
                let valid: Vec<&str> = KNOWN_PRIMARY_LABELS
                    .iter()
                    .copied()
                    .chain(std::iter::once("none"))
                    .collect();
                return Ok(format!(
                    "Invalid label filter '{label}'. Valid labels: {}",
                    valid.join(", ")
                ));
            }
        }

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
                    &(self.logs.clone() as Arc<dyn RequestLogger>),
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
                    return Ok("No matching code patterns found.".to_string());
                }

                Ok(format_search_results(&results))
            }
            Err(e) => {
                fire_and_forget_log(
                    &(self.logs.clone() as Arc<dyn RequestLogger>),
                    "search_code",
                    &params.query,
                    &filters,
                    0,
                    None,
                    latency_ms,
                    Some(&e.to_string()),
                    None,
                );
                Err(CodeAssistantError::Search(format!("Search failed: {e}")))
            }
        }
    }
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
            "### Result {} \u{2014} `{}` ({})\n",
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
