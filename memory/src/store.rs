use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rusqlite::Connection;
use sqlite_vec::sqlite3_vec_init;

use crate::error::MemoryError;
use crate::memory::Memory;

/// Embedding dimensions for nomic-embed-text-v1.5.
const VECTOR_DIM: usize = 768;

/// FTS keyword search result before merging.
#[derive(Debug)]
pub(crate) struct FtsResult {
    pub(crate) id: String,
    // Rank is returned by FTS5 but we use positional RRF scoring instead.
    #[allow(dead_code)]
    pub(crate) rank: f64,
}

/// Vector similarity search result before merging.
#[derive(Debug)]
pub(crate) struct VecResult {
    pub(crate) id: String,
    // Distance is returned by sqlite-vec but we use positional RRF scoring instead.
    #[allow(dead_code)]
    pub(crate) distance: f64,
}

/// SQLite-backed memory store with FTS5 and sqlite-vec for hybrid search.
pub(crate) struct MemoryStore {
    conn: Mutex<Connection>,
}

impl std::fmt::Debug for MemoryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryStore").finish_non_exhaustive()
    }
}

impl MemoryStore {
    /// Open (or create) the SQLite database at the given path with sqlite-vec loaded.
    pub(crate) fn new(db_path: &Path) -> Result<Self, MemoryError> {
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
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;

        let store = Self {
            conn: Mutex::new(conn),
        };
        store.ensure_tables()?;

        Ok(store)
    }

    /// Create tables if they don't already exist.
    fn ensure_tables(&self) -> Result<(), MemoryError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| MemoryError::Other(e.to_string()))?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS memories (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                content TEXT NOT NULL,
                tags TEXT NOT NULL DEFAULT '[]',
                project TEXT,
                source_task TEXT,
                source_type TEXT NOT NULL DEFAULT 'agent',
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                last_accessed TEXT,
                access_count INTEGER NOT NULL DEFAULT 0,
                pinned INTEGER NOT NULL DEFAULT 0,
                persistent INTEGER NOT NULL DEFAULT 0
            );",
        )?;

        // FTS5 content table for keyword search.
        conn.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(
                memory_id UNINDEXED,
                title,
                content,
                tags_text,
                tokenize='porter unicode61'
            );",
        )?;

        // sqlite-vec virtual table for vector similarity.
        conn.execute_batch(&format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS memory_vectors USING vec0(
                memory_id TEXT PRIMARY KEY,
                embedding float[{VECTOR_DIM}] distance_metric=cosine
            );"
        ))?;

        Ok(())
    }

    // ── Insert ───────────────────────────────────────────────────────

    /// Insert a new memory into the store.
    pub(crate) fn insert(&self, memory: &Memory) -> Result<(), MemoryError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| MemoryError::Other(e.to_string()))?;
        let tags_json = serde_json::to_string(&memory.tags)?;

        conn.execute(
            "INSERT OR REPLACE INTO memories
                (id, title, content, tags, project, source_task, source_type,
                 created_at, updated_at, last_accessed, access_count, pinned, persistent)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            rusqlite::params![
                memory.id,
                memory.title,
                memory.content,
                tags_json,
                memory.project,
                memory.source_task,
                memory.source_type,
                memory.created_at.to_rfc3339(),
                memory.updated_at.to_rfc3339(),
                memory.last_accessed.map(|dt| dt.to_rfc3339()),
                memory.access_count,
                memory.pinned as i32,
                memory.persistent as i32,
            ],
        )?;

        Ok(())
    }

    /// Insert or update an FTS5 entry for a memory.
    pub(crate) fn upsert_fts(
        &self,
        memory_id: &str,
        title: &str,
        content: &str,
        tags_text: &str,
    ) -> Result<(), MemoryError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| MemoryError::Other(e.to_string()))?;

        // FTS5 doesn't support UPDATE; delete then re-insert.
        conn.execute(
            "DELETE FROM memory_fts WHERE memory_id = ?1",
            rusqlite::params![memory_id],
        )?;
        conn.execute(
            "INSERT INTO memory_fts(memory_id, title, content, tags_text) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![memory_id, title, content, tags_text],
        )?;

        Ok(())
    }

    /// Store a vector embedding for a memory.
    pub(crate) fn upsert_embedding(
        &self,
        memory_id: &str,
        embedding: &[f32],
    ) -> Result<(), MemoryError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| MemoryError::Other(e.to_string()))?;

        // vec0 doesn't support UPDATE; delete then re-insert.
        conn.execute(
            "DELETE FROM memory_vectors WHERE memory_id = ?1",
            rusqlite::params![memory_id],
        )?;
        conn.execute(
            "INSERT INTO memory_vectors(memory_id, embedding) VALUES (?1, ?2)",
            rusqlite::params![memory_id, embedding_to_bytes(embedding)],
        )?;

        Ok(())
    }

    // ── Query ────────────────────────────────────────────────────────

    /// Find a memory by exact ID.
    pub(crate) fn find_by_id(&self, id: &str) -> Result<Memory, MemoryError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| MemoryError::Other(e.to_string()))?;
        let mut stmt = conn.prepare(
            "SELECT id, title, content, tags, project, source_task, source_type,
                    created_at, updated_at, last_accessed, access_count, pinned, persistent
             FROM memories WHERE id = ?1",
        )?;

        let memory = stmt
            .query_row(rusqlite::params![id], |row| Ok(row_to_memory(row)))
            .map_err(|_| MemoryError::NotFound(format!("memory '{id}'")))?;

        Ok(memory)
    }

    /// List memories with optional filters, ordered by created_at DESC.
    pub(crate) fn list(
        &self,
        project: Option<&str>,
        persistent: Option<bool>,
        limit: usize,
    ) -> Result<Vec<Memory>, MemoryError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| MemoryError::Other(e.to_string()))?;

        let mut sql = String::from(
            "SELECT id, title, content, tags, project, source_task, source_type,
                    created_at, updated_at, last_accessed, access_count, pinned, persistent
             FROM memories WHERE 1=1",
        );
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        if let Some(proj) = project {
            sql.push_str(&format!(" AND project = ?{}", params.len() + 1));
            params.push(Box::new(proj.to_string()));
        }
        if let Some(p) = persistent {
            sql.push_str(&format!(" AND persistent = ?{}", params.len() + 1));
            params.push(Box::new(p as i32));
        }
        sql.push_str(&format!(
            " ORDER BY created_at DESC LIMIT ?{}",
            params.len() + 1
        ));
        params.push(Box::new(limit as i64));

        let mut stmt = conn.prepare(&sql)?;
        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();
        let rows = stmt
            .query_map(param_refs.as_slice(), |row| Ok(row_to_memory(row)))?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(rows)
    }

    /// Record access for a batch of memory IDs.
    pub(crate) fn record_access(&self, ids: &[String]) -> Result<(), MemoryError> {
        if ids.is_empty() {
            return Ok(());
        }
        let conn = self
            .conn
            .lock()
            .map_err(|e| MemoryError::Other(e.to_string()))?;
        let now = Utc::now().to_rfc3339();

        for id in ids {
            conn.execute(
                "UPDATE memories SET last_accessed = ?1, access_count = access_count + 1 WHERE id = ?2",
                rusqlite::params![now, id],
            )?;
        }

        Ok(())
    }

    // ── Search ───────────────────────────────────────────────────────

    /// Full-text keyword search, returning ranked results.
    pub(crate) fn search_fts(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<FtsResult>, MemoryError> {
        let escaped = escape_fts5_query(query);
        if escaped.is_empty() {
            return Ok(Vec::new());
        }

        let conn = self
            .conn
            .lock()
            .map_err(|e| MemoryError::Other(e.to_string()))?;
        let mut stmt = conn.prepare(
            "SELECT memory_id, rank
             FROM memory_fts
             WHERE memory_fts MATCH ?1
             ORDER BY rank
             LIMIT ?2",
        )?;

        let rows = stmt
            .query_map(rusqlite::params![escaped, limit as i64], |row| {
                Ok(FtsResult {
                    id: row.get(0)?,
                    rank: row.get(1)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(rows)
    }

    /// Vector similarity search (KNN), returning ranked results.
    pub(crate) fn search_vector(
        &self,
        query_embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<VecResult>, MemoryError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| MemoryError::Other(e.to_string()))?;
        let mut stmt = conn.prepare(
            "SELECT memory_id, distance
             FROM memory_vectors
             WHERE embedding MATCH ?1 AND k = ?2",
        )?;

        let rows = stmt
            .query_map(
                rusqlite::params![embedding_to_bytes(query_embedding), limit as i64],
                |row| {
                    Ok(VecResult {
                        id: row.get(0)?,
                        distance: row.get(1)?,
                    })
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(rows)
    }

    /// Check whether any embeddings exist.
    pub(crate) fn has_embeddings(&self) -> Result<bool, MemoryError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| MemoryError::Other(e.to_string()))?;
        let count: i64 =
            conn.query_row("SELECT COUNT(*) FROM memory_vectors", [], |row| row.get(0))?;
        Ok(count > 0)
    }
}

// ── Helpers ──────────────────────────────────────────────────────────

/// Convert f32 embedding slice to bytes for sqlite-vec.
fn embedding_to_bytes(embedding: &[f32]) -> Vec<u8> {
    embedding.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Escape a user query for safe use in FTS5 MATCH expressions.
///
/// Wraps each token in double quotes to prevent FTS5 column-filter
/// interpretation (e.g. `word:term`).
fn escape_fts5_query(query: &str) -> String {
    query
        .split_whitespace()
        .map(|term| format!("\"{}\"", term.replace('"', "")))
        .collect::<Vec<_>>()
        .join(" ")
}

fn row_to_memory(row: &rusqlite::Row<'_>) -> Memory {
    let tags_json: String = row.get_unwrap(3);
    let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();
    let created_str: String = row.get_unwrap(7);
    let updated_str: String = row.get_unwrap(8);
    let last_accessed_str: Option<String> = row.get_unwrap(9);
    let pinned_int: i32 = row.get_unwrap(11);
    let persistent_int: i32 = row.get_unwrap(12);

    Memory {
        id: row.get_unwrap(0),
        title: row.get_unwrap(1),
        content: row.get_unwrap(2),
        tags,
        project: row.get_unwrap(4),
        source_task: row.get_unwrap(5),
        source_type: row.get_unwrap(6),
        created_at: parse_dt(&created_str),
        updated_at: parse_dt(&updated_str),
        last_accessed: last_accessed_str.as_deref().map(parse_dt),
        access_count: row.get_unwrap(10),
        pinned: pinned_int != 0,
        persistent: persistent_int != 0,
    }
}

fn parse_dt(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_fts5_plain_terms() {
        assert_eq!(escape_fts5_query("hello world"), "\"hello\" \"world\"");
    }

    #[test]
    fn escape_fts5_column_like_term() {
        assert_eq!(
            escape_fts5_query("style-agent:value"),
            "\"style-agent:value\""
        );
    }

    #[test]
    fn escape_fts5_empty_query() {
        assert_eq!(escape_fts5_query(""), "");
        assert_eq!(escape_fts5_query("   "), "");
    }
}
