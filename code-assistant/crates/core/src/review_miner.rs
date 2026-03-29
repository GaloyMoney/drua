use std::path::Path;
use std::sync::Mutex;

use base64::Engine;
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, USER_AGENT};
use rusqlite::Connection;
use serde::Deserialize;
use sqlite_vec::sqlite3_vec_init;
use zerocopy::AsBytes;

use crate::embedder::Embedder;
use crate::store::VECTOR_DIM;

pub const ANTI_PATTERNS_COLLECTION: &str = "anti_patterns";

const GITHUB_API_BASE: &str = "https://api.github.com";

/// Bot accounts to always skip when filtering review comments.
const BOT_USERS: &[&str] = &[
    "coderabbitai",
    "cursor",
    "copilot",
    "github-actions",
    "dependabot",
    "renovate",
];

/// A review comment extracted from a GitHub PR, ready for storage.
#[derive(Debug, Clone)]
pub struct ReviewAntiPattern {
    pub id: String,
    pub repo: String,
    pub pr_number: u64,
    pub reviewer: String,
    pub review_comment: String,
    pub file_path: String,
    pub before_code: Option<String>,
    pub after_code: Option<String>,
    pub diff_hunk: String,
    pub original_line: Option<u64>,
    pub created_at: String,
}

/// Raw GitHub PR review comment from the REST API.
#[derive(Debug, Deserialize)]
struct GhReviewComment {
    #[expect(dead_code)]
    id: u64,
    user: GhUser,
    body: Option<String>,
    path: String,
    original_commit_id: Option<String>,
    commit_id: Option<String>,
    diff_hunk: Option<String>,
    original_line: Option<u64>,
    #[serde(default)]
    pull_request_url: Option<String>,
    created_at: String,
}

#[derive(Debug, Deserialize)]
struct GhUser {
    login: String,
}

/// GitHub file content response (used for before/after extraction).
#[derive(Debug, Deserialize)]
struct GhFileContent {
    content: Option<String>,
    encoding: Option<String>,
}

/// Manages GitHub API access and anti-pattern storage (SQLite-backed).
pub struct ReviewMiner {
    client: reqwest::Client,
    conn: Mutex<Connection>,
    embedder: Embedder,
}

/// Summary stats returned after mining.
#[derive(Debug, Default)]
pub struct MiningStats {
    pub total_fetched: usize,
    pub filtered: usize,
    pub with_fix: usize,
    pub stored: usize,
}

impl ReviewMiner {
    pub fn new(github_token: &str, db_path: &Path, embedder: Embedder) -> anyhow::Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {github_token}"))?,
        );
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/vnd.github+json"),
        );
        headers.insert(
            USER_AGENT,
            HeaderValue::from_static("code-assistant-review-miner"),
        );
        headers.insert(
            "X-GitHub-Api-Version",
            HeaderValue::from_static("2022-11-28"),
        );

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()?;

        // Ensure parent directory exists
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        unsafe {
            rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute::<
                *const (),
                unsafe extern "C" fn(
                    *mut rusqlite::ffi::sqlite3,
                    *mut *mut i8,
                    *const rusqlite::ffi::sqlite3_api_routines,
                ) -> i32,
            >(
                sqlite3_vec_init as *const ()
            )));
        }

        let conn = Connection::open(db_path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;

        Ok(Self {
            client,
            conn: Mutex::new(conn),
            embedder,
        })
    }

    /// Ensure the anti_patterns tables exist.
    pub fn ensure_collection(&self) -> anyhow::Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;

        conn.execute_batch(&format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS anti_patterns_vec USING vec0(
                pattern_id TEXT PRIMARY KEY,
                embedding float[{VECTOR_DIM}] distance_metric=cosine
            );"
        ))?;

        conn.execute_batch(&format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS anti_patterns_code_vec USING vec0(
                pattern_id TEXT PRIMARY KEY,
                embedding float[{VECTOR_DIM}] distance_metric=cosine
            );"
        ))?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS anti_patterns_meta (
                pattern_id TEXT PRIMARY KEY,
                repo TEXT NOT NULL,
                pr_number INTEGER,
                reviewer TEXT,
                review_comment TEXT,
                file_path TEXT,
                before_code TEXT,
                after_code TEXT,
                diff_hunk TEXT,
                has_fix TEXT,
                created_at TEXT
            );",
        )?;

        tracing::info!("Ensured anti_patterns tables");
        Ok(())
    }

    /// Fetch all review comments for a repo, paginating through the API.
    async fn fetch_comments(&self, repo: &str) -> anyhow::Result<Vec<GhReviewComment>> {
        let mut all_comments = Vec::new();
        let mut page = 1u32;

        loop {
            let url = format!(
                "{GITHUB_API_BASE}/repos/{repo}/pulls/comments?per_page=100&page={page}&sort=created&direction=asc"
            );

            tracing::info!(repo, page, "Fetching review comments");

            let resp = self.client.get(&url).send().await?;

            // Check rate limit headers
            if let Some(remaining) = resp
                .headers()
                .get("x-ratelimit-remaining")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u32>().ok())
            {
                if remaining < 50 {
                    tracing::warn!(remaining, "GitHub rate limit low, adding delay");
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
                if remaining == 0 {
                    anyhow::bail!(
                        "GitHub API rate limit exhausted. Wait for reset before retrying."
                    );
                }
            }

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("GitHub API error ({status}): {body}");
            }

            let comments: Vec<GhReviewComment> = resp.json().await?;
            let count = comments.len();
            all_comments.extend(comments);

            if count < 100 {
                break;
            }
            page += 1;
        }

        Ok(all_comments)
    }

    /// Filter comments to only those from specified reviewers, excluding bots.
    fn filter_comments(
        comments: Vec<GhReviewComment>,
        reviewers: &[String],
    ) -> Vec<GhReviewComment> {
        comments
            .into_iter()
            .filter(|c| {
                let login = c.user.login.to_lowercase();
                // Skip bots
                if BOT_USERS.iter().any(|b| login.contains(b)) {
                    return false;
                }
                // Filter to target reviewers
                reviewers.iter().any(|r| r.to_lowercase() == login)
            })
            .filter(|c| {
                // Skip empty comments
                c.body.as_ref().is_some_and(|b| !b.trim().is_empty())
            })
            .collect()
    }

    /// Extract PR number from pull_request_url field.
    fn extract_pr_number(comment: &GhReviewComment) -> u64 {
        comment
            .pull_request_url
            .as_ref()
            .and_then(|url| url.rsplit('/').next())
            .and_then(|n| n.parse().ok())
            .unwrap_or(0)
    }

    /// Fetch raw file content at a specific commit SHA.
    async fn fetch_file_at_ref(
        &self,
        repo: &str,
        path: &str,
        git_ref: &str,
    ) -> anyhow::Result<Option<String>> {
        let url = format!("{GITHUB_API_BASE}/repos/{repo}/contents/{path}?ref={git_ref}");

        let resp = self.client.get(&url).send().await?;

        if !resp.status().is_success() {
            // File may not exist at this ref (renamed, deleted, etc.)
            return Ok(None);
        }

        let content: GhFileContent = resp.json().await?;

        match (content.content, content.encoding.as_deref()) {
            (Some(encoded), Some("base64")) => {
                // GitHub returns base64 with embedded newlines
                let cleaned: String = encoded.chars().filter(|c| !c.is_whitespace()).collect();
                let decoded = base64::engine::general_purpose::STANDARD.decode(cleaned)?;
                Ok(Some(String::from_utf8_lossy(&decoded).into_owned()))
            }
            (Some(raw), _) => Ok(Some(raw)),
            _ => Ok(None),
        }
    }

    /// Extract a code section around the commented line (~20 lines context).
    fn extract_section(full_content: &str, target_line: u64, context_lines: u64) -> String {
        let lines: Vec<&str> = full_content.lines().collect();
        let target = target_line.saturating_sub(1) as usize; // 0-indexed
        let start = target.saturating_sub(context_lines as usize);
        let end = (target + context_lines as usize + 1).min(lines.len());

        lines[start..end].join("\n")
    }

    /// Attempt to extract before/after code pairs for a comment.
    async fn extract_before_after(
        &self,
        repo: &str,
        comment: &GhReviewComment,
    ) -> (Option<String>, Option<String>) {
        let (original_sha, commit_sha) = match (
            comment.original_commit_id.as_deref(),
            comment.commit_id.as_deref(),
        ) {
            (Some(orig), Some(commit)) => (orig, commit),
            _ => return (None, None),
        };

        // If same commit, no fix was applied — only extract "before"
        let has_fix = original_sha != commit_sha;
        let line = comment.original_line.unwrap_or(1);
        let context = 10; // lines of context around the target

        let before = self
            .fetch_file_at_ref(repo, &comment.path, original_sha)
            .await
            .ok()
            .flatten()
            .map(|content| Self::extract_section(&content, line, context));

        let after = if has_fix {
            self.fetch_file_at_ref(repo, &comment.path, commit_sha)
                .await
                .ok()
                .flatten()
                .map(|content| Self::extract_section(&content, line, context))
        } else {
            None
        };

        (before, after)
    }

    /// Mine review comments from a single repo and store anti-patterns.
    pub async fn mine_repo(&self, repo: &str, reviewers: &[String]) -> anyhow::Result<MiningStats> {
        let mut stats = MiningStats::default();

        println!("\n--- Mining {repo} ---");

        // Fetch all comments
        let comments = self.fetch_comments(repo).await?;
        stats.total_fetched = comments.len();
        println!("  Fetched {} review comments", stats.total_fetched);

        // Filter to target reviewers
        let filtered = Self::filter_comments(comments, reviewers);
        stats.filtered = filtered.len();
        println!("  Filtered to {} reviewer comments", stats.filtered);

        if filtered.is_empty() {
            return Ok(stats);
        }

        // Process comments in batches
        let batch_size = 10;
        let mut anti_patterns = Vec::new();

        for (i, comment) in filtered.iter().enumerate() {
            let pr_number = Self::extract_pr_number(comment);

            let has_fix = comment.original_commit_id.as_deref() != comment.commit_id.as_deref();

            // Only fetch before/after for comments where a fix was made
            let (before_code, after_code) = if has_fix {
                self.extract_before_after(repo, comment).await
            } else {
                (None, None)
            };

            if before_code.is_some() || after_code.is_some() {
                stats.with_fix += 1;
            }

            let pattern = ReviewAntiPattern {
                id: uuid::Uuid::new_v4().to_string(),
                repo: repo.to_string(),
                pr_number,
                reviewer: comment.user.login.clone(),
                review_comment: comment.body.clone().unwrap_or_default(),
                file_path: comment.path.clone(),
                before_code,
                after_code,
                diff_hunk: comment.diff_hunk.clone().unwrap_or_default(),
                original_line: comment.original_line,
                created_at: comment.created_at.clone(),
            };

            anti_patterns.push(pattern);

            // Progress
            if (i + 1) % 50 == 0 {
                println!(
                    "  Processed {}/{} comments ({} with fixes)",
                    i + 1,
                    stats.filtered,
                    stats.with_fix
                );
            }

            // Small delay between file fetches to avoid rate limiting
            if has_fix && (i + 1) % batch_size == 0 {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }

        // Embed and store
        println!(
            "  Embedding and storing {} anti-patterns...",
            anti_patterns.len()
        );

        let embed_batch_size = 4;
        for batch_start in (0..anti_patterns.len()).step_by(embed_batch_size) {
            let batch_end = (batch_start + embed_batch_size).min(anti_patterns.len());
            let batch = &anti_patterns[batch_start..batch_end];

            let embed_texts: Vec<String> = batch
                .iter()
                .map(|p| build_embed_text(&p.review_comment, p.before_code.as_deref()))
                .collect();

            let embeddings = self.embedder.embed_document_batch(embed_texts).await?;

            // Also embed before_code alone for code-to-code matching
            let code_only_items: Vec<(usize, String)> = batch
                .iter()
                .enumerate()
                .filter_map(|(i, p)| {
                    p.before_code
                        .as_ref()
                        .filter(|c| !c.trim().is_empty())
                        .map(|c| (i, c.clone()))
                })
                .collect();

            let code_only_embeddings = if !code_only_items.is_empty() {
                let code_texts: Vec<String> =
                    code_only_items.iter().map(|(_, c)| c.clone()).collect();
                Some(self.embedder.embed_document_batch(code_texts).await?)
            } else {
                None
            };

            let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
            let tx = conn.unchecked_transaction()?;

            for (pattern, embedding) in batch.iter().zip(embeddings) {
                let has_fix = pattern.before_code.is_some()
                    && pattern.after_code.is_some()
                    && pattern.before_code != pattern.after_code;

                // Insert into vec0 table (review comment + code embedding)
                tx.execute(
                    "DELETE FROM anti_patterns_vec WHERE pattern_id = ?1",
                    [&pattern.id],
                )?;
                tx.execute(
                    "INSERT INTO anti_patterns_vec(pattern_id, embedding) VALUES (?1, ?2)",
                    rusqlite::params![pattern.id, embedding.as_bytes()],
                )?;

                // Insert metadata
                tx.execute(
                    "INSERT OR REPLACE INTO anti_patterns_meta
                        (pattern_id, repo, pr_number, reviewer, review_comment,
                         file_path, before_code, after_code, diff_hunk, has_fix, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                    rusqlite::params![
                        pattern.id,
                        pattern.repo,
                        pattern.pr_number as i64,
                        pattern.reviewer,
                        pattern.review_comment,
                        pattern.file_path,
                        pattern.before_code.as_deref().unwrap_or(""),
                        pattern.after_code.as_deref().unwrap_or(""),
                        pattern.diff_hunk,
                        has_fix.to_string(),
                        pattern.created_at,
                    ],
                )?;
            }

            // Insert code-only embeddings
            if let Some(code_embeddings) = &code_only_embeddings {
                for ((idx, _), code_embedding) in code_only_items.iter().zip(code_embeddings.iter())
                {
                    let pattern = &batch[*idx];
                    tx.execute(
                        "DELETE FROM anti_patterns_code_vec WHERE pattern_id = ?1",
                        [&pattern.id],
                    )?;
                    tx.execute(
                        "INSERT INTO anti_patterns_code_vec(pattern_id, embedding) VALUES (?1, ?2)",
                        rusqlite::params![pattern.id, code_embedding.as_bytes()],
                    )?;
                }
            }

            tx.commit()?;
            stats.stored += batch.len();

            if batch_end % 50 == 0 || batch_end == anti_patterns.len() {
                tracing::info!(
                    progress = batch_end,
                    total = anti_patterns.len(),
                    "Stored anti-patterns"
                );
            }
        }

        println!(
            "  Done: {} stored ({} with before/after pairs)",
            stats.stored, stats.with_fix
        );

        Ok(stats)
    }

    /// Get the current count in the anti_patterns table.
    pub fn collection_count(&self) -> anyhow::Result<u64> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM anti_patterns_meta", [], |row| {
                row.get(0)
            })
            .unwrap_or(0);
        Ok(count as u64)
    }
}

/// Build text for embedding: review comment + before code (if available).
fn build_embed_text(review_comment: &str, before_code: Option<&str>) -> String {
    let mut text = String::new();
    text.push_str("Review comment: ");
    text.push_str(review_comment);
    if let Some(code) = before_code {
        text.push_str("\n\nCode:\n");
        text.push_str(code);
    }
    text
}

/// Get GitHub token from `gh auth token` CLI command.
pub fn get_github_token() -> anyhow::Result<String> {
    let output = std::process::Command::new("gh")
        .args(["auth", "token"])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to get GitHub token via `gh auth token`: {stderr}");
    }

    let token = String::from_utf8(output.stdout)?.trim().to_string();
    if token.is_empty() {
        anyhow::bail!("GitHub token is empty. Run `gh auth login` first.");
    }
    Ok(token)
}
