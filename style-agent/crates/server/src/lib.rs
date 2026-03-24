use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router, ServerHandler,
};
use style_agent_core::{
    request_log::RequestLogEntry,
    search::SearchEngine,
    store::{AntiPatternResult, SearchResult},
};

#[derive(Debug, Clone)]
pub struct StyleAgentServer {
    search_engine: Arc<SearchEngine>,
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SearchCodeParams {
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

    /// Filter results to a specific architectural layer
    #[schemars(
        description = "Filter results to a specific architectural layer. Values: domain, application, infrastructure, interface"
    )]
    layer: Option<String>,

    /// Filter results to chunks that use a specific pattern
    #[schemars(
        description = "Filter results to chunks that use a specific pattern. Values: repository, events, authorization, publisher, config"
    )]
    uses: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ReviewCodeParams {
    /// The code to check for known anti-patterns
    #[schemars(description = "The code to check for known anti-patterns from PR reviews")]
    code: String,

    /// Optional context describing what the code does or the task at hand
    #[schemars(
        description = "Optional context describing what the code does (used for semantic matching against review comments)"
    )]
    context: Option<String>,

    /// Maximum number of anti-pattern warnings to return (default: 5)
    #[schemars(description = "Maximum number of anti-pattern warnings to return (default: 5)")]
    limit: Option<u64>,
}

#[tool_router]
impl StyleAgentServer {
    pub fn new(search_engine: Arc<SearchEngine>) -> Self {
        Self {
            search_engine,
            tool_router: Self::tool_router(),
        }
    }

    /// Search indexed code repositories for patterns, conventions, and style examples matching a query.
    #[tool(
        description = "Search indexed codebases for code patterns matching a query.\n\nUsage tips:\n- Pass a code snippet as the query (e.g. the pattern you are about to write) — code-as-query gives much better results than natural language\n- Always pass a `label` filter for precise results\n- Adopt the style, naming, and structure from the returned examples — don't guess conventions, search first\n\nAvailable labels: entity, entity_event, entity_command, entity_query, entity_hydration, error, service, service_method, repository, domain_primitives, value_object, type_conversion, config, test, api, job, event_handler, authorization, published_event, new_entity, none (unlabeled chunks)"
    )]
    async fn search_code(
        &self,
        Parameters(params): Parameters<SearchCodeParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let start = Instant::now();
        let limit = params.limit.unwrap_or(10);
        let repo_filter = params.repo.as_deref();
        let language_filter = params.language.as_deref();
        let label_filter = params.label.as_deref();
        let layer_filter = params.layer.as_deref();
        let uses_filter = params.uses.as_deref();

        let filters = serde_json::json!({
            "label": params.label,
            "language": params.language,
            "layer": params.layer,
            "repo": params.repo,
            "uses": params.uses,
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
                layer_filter,
                uses_filter,
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

    /// Check code against known anti-patterns mined from PR reviews.
    #[tool(
        description = "Check code against known anti-patterns mined from PR reviews. Returns warnings when submitted code matches patterns that reviewers have previously flagged, along with the reviewer's comment and suggested fix (if available)."
    )]
    async fn review_code(
        &self,
        Parameters(params): Parameters<ReviewCodeParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let start = Instant::now();
        let limit = params.limit.unwrap_or(5);

        let filters = serde_json::json!({
            "context": params.context.is_some(),
            "limit": limit,
        });

        // Truncate the code for the log query field (first 500 chars)
        let query_for_log: String = params.code.chars().take(500).collect();

        // Check if we have any anti-patterns at all
        let count = self.search_engine.anti_pattern_count().unwrap_or(0);

        if count == 0 {
            let latency_ms = start.elapsed().as_millis() as i64;
            fire_and_forget_log(
                &self.search_engine,
                "review_code",
                &query_for_log,
                &filters,
                0,
                None,
                latency_ms,
                None,
            );
            return Ok(CallToolResult::success(vec![Content::text(
                "No anti-patterns indexed yet. Run `style-agent mine-reviews` to extract patterns from PR reviews.",
            )]));
        }

        // Fetch extra candidates to allow for dedup and filtering
        let fetch_limit = limit * 3;

        // 1. Code-to-code search (primary)
        let code_results = match self
            .search_engine
            .search_anti_patterns_by_code(&params.code, fetch_limit)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                let latency_ms = start.elapsed().as_millis() as i64;
                fire_and_forget_log(
                    &self.search_engine,
                    "review_code",
                    &query_for_log,
                    &filters,
                    0,
                    None,
                    latency_ms,
                    Some(&e.to_string()),
                );
                return Err(rmcp::ErrorData::internal_error(
                    format!("Anti-pattern code search failed: {e}"),
                    None,
                ));
            }
        };

        // 2. Semantic context search (if context provided)
        let context_results = if let Some(ref context) = params.context {
            match self
                .search_engine
                .search_anti_patterns(context, fetch_limit)
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    let latency_ms = start.elapsed().as_millis() as i64;
                    fire_and_forget_log(
                        &self.search_engine,
                        "review_code",
                        &query_for_log,
                        &filters,
                        0,
                        None,
                        latency_ms,
                        Some(&e.to_string()),
                    );
                    return Err(rmcp::ErrorData::internal_error(
                        format!("Anti-pattern context search failed: {e}"),
                        None,
                    ));
                }
            }
        } else {
            Vec::new()
        };

        // 3. Merge, deduplicate by pattern_id, keep best similarity
        let merged = merge_anti_pattern_results(code_results, context_results, limit);

        let latency_ms = start.elapsed().as_millis() as i64;
        let result_count = merged.len() as i64;
        let top_score = merged.first().map(|r| r.similarity as f64);

        fire_and_forget_log(
            &self.search_engine,
            "review_code",
            &query_for_log,
            &filters,
            result_count,
            top_score,
            latency_ms,
            None,
        );

        if merged.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No known anti-patterns detected.",
            )]));
        }

        let formatted = format_anti_pattern_results(&merged);
        Ok(CallToolResult::success(vec![Content::text(formatted)]))
    }
}

/// Generate an ISO 8601 UTC timestamp string.
fn iso8601_now() -> String {
    use std::time::SystemTime;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    // Simple formatting: seconds since epoch → date-time via chrono-free calculation
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    // Days since 1970-01-01
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

    // Build label and layer distribution summaries
    let mut label_counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut layer_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for r in results {
        let label = r.labels.first().map(|s| s.as_str()).unwrap_or("unlabeled");
        *label_counts.entry(label).or_default() += 1;
        if let Some(ref layer) = r.layer {
            *layer_counts.entry(layer.as_str()).or_default() += 1;
        }
    }

    if !label_counts.is_empty() {
        let labels_summary: Vec<String> = label_counts
            .iter()
            .map(|(k, v)| format!("{k} ({v})"))
            .collect();
        output.push_str(&format!("Labels: {}\n", labels_summary.join(", ")));
    }
    if !layer_counts.is_empty() {
        let layers_summary: Vec<String> = layer_counts
            .iter()
            .map(|(k, v)| format!("{k} ({v})"))
            .collect();
        output.push_str(&format!("Layers: {}\n", layers_summary.join(", ")));
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
            output.push_str(&format!("**Label**: {}", result.labels.join(", ")));
            if let Some(ref layer) = result.layer {
                output.push_str(&format!(" | **Layer**: {layer}"));
            }
            if !result.uses.is_empty() {
                output.push_str(&format!(" | **Uses**: {}", result.uses.join(", ")));
            }
            output.push('\n');
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

/// Minimum similarity threshold for anti-pattern matches.
const ANTI_PATTERN_SIMILARITY_THRESHOLD: f32 = 0.5;

/// Merge code-search and context-search anti-pattern results.
/// Deduplicates by pattern_id (keeping the highest similarity), filters below
/// threshold, and returns the top `limit` results.
fn merge_anti_pattern_results(
    code_results: Vec<AntiPatternResult>,
    context_results: Vec<AntiPatternResult>,
    limit: u64,
) -> Vec<AntiPatternResult> {
    let mut best: std::collections::HashMap<String, AntiPatternResult> =
        std::collections::HashMap::new();

    for result in code_results.into_iter().chain(context_results) {
        if result.similarity < ANTI_PATTERN_SIMILARITY_THRESHOLD {
            continue;
        }
        best.entry(result.pattern_id.clone())
            .and_modify(|existing| {
                if result.similarity > existing.similarity {
                    *existing = result.clone();
                }
            })
            .or_insert(result);
    }

    let mut merged: Vec<AntiPatternResult> = best.into_values().collect();
    merged.sort_by(|a, b| {
        b.similarity
            .partial_cmp(&a.similarity)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    merged.truncate(limit as usize);
    merged
}

/// Format anti-pattern results into a user-facing warning string.
fn format_anti_pattern_results(results: &[AntiPatternResult]) -> String {
    let mut output = format!(
        "Found {} potential anti-pattern{} matching your code:\n",
        results.len(),
        if results.len() == 1 { "" } else { "s" }
    );

    for (i, r) in results.iter().enumerate() {
        output.push_str("\n---\n");
        output.push_str(&format!(
            "⚠️  Anti-pattern #{} (similarity: {:.2})\n",
            i + 1,
            r.similarity
        ));
        output.push_str(&format!(
            "PR: {}#{} — Reviewer: {}\n\n",
            r.repo, r.pr_number, r.reviewer
        ));

        output.push_str(&format!("**Issue**: {}\n", r.review_comment));

        if !r.before_code.is_empty() {
            output.push_str("\n**Problematic code** (what to avoid):\n```\n");
            output.push_str(&r.before_code);
            if !r.before_code.ends_with('\n') {
                output.push('\n');
            }
            output.push_str("```\n");
        }

        if r.has_fix && !r.after_code.is_empty() {
            output.push_str("\n**Correct code** (what to do instead):\n```\n");
            output.push_str(&r.after_code);
            if !r.after_code.ends_with('\n') {
                output.push('\n');
            }
            output.push_str("```\n");
        }
    }

    output
}
#[tool_handler]
impl ServerHandler for StyleAgentServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "style-agent",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Style Agent — searches indexed code repositories for patterns and conventions",
            )
    }
}

/// Default threshold below which a top_score is considered "low quality".
const DEFAULT_LOW_SCORE_THRESHOLD: f64 = 0.3;

/// Handler for `GET /stats` — returns aggregated request log analytics as JSON.
async fn stats_handler(
    axum::extract::State(engine): axum::extract::State<Arc<SearchEngine>>,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;

    match tokio::task::spawn_blocking(move || engine.query_stats(DEFAULT_LOW_SCORE_THRESHOLD)).await
    {
        Ok(Ok(stats)) => axum::Json(stats).into_response(),
        Ok(Err(e)) => {
            tracing::error!(error = %e, "Failed to query stats");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Stats error: {e}"),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "Stats task panicked");
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response()
        }
    }
}

/// Start the HTTP MCP server from a `CoreConfig` and block until shutdown.
pub async fn run_server_with_config(
    config: &style_agent_core::CoreConfig,
    bind_addr: &str,
) -> anyhow::Result<()> {
    use style_agent_core::embedder::Embedder;
    use style_agent_core::store::VectorStore;

    let embedder = Embedder::new()?;
    let store = VectorStore::new(&config.db_path)?;
    store.ensure_collection()?;
    store.ensure_anti_pattern_tables()?;
    let search_engine = Arc::new(SearchEngine::new(embedder, store));

    run_server(search_engine, bind_addr).await
}

/// Start the HTTP MCP server and block until shutdown.
pub async fn run_server(search_engine: Arc<SearchEngine>, bind_addr: &str) -> anyhow::Result<()> {
    use rmcp::transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    };

    let config = StreamableHttpServerConfig {
        stateful_mode: false,
        json_response: true,
        ..Default::default()
    };

    let stats_engine = Arc::clone(&search_engine);

    let service = StreamableHttpService::new(
        move || Ok(StyleAgentServer::new(Arc::clone(&search_engine))),
        LocalSessionManager::default().into(),
        config,
    );

    let router = axum::Router::new()
        .route("/stats", axum::routing::get(stats_handler))
        .with_state(stats_engine)
        .nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    let url = format!("http://{bind_addr}/mcp");

    tracing::info!(%url, "Style Agent MCP server listening");
    println!("Style Agent MCP server listening at {url}");

    axum::serve(listener, router).await?;
    Ok(())
}
