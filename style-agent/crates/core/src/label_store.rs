use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Stable composite key identifying a chunk across re-indexes.
///
/// Derived from chunk metadata (not the chunk UUID, which changes
/// on re-index).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkKey {
    pub repo: String,
    pub file_path: String,
    pub chunk_type: String,
    pub entity_name: String,
}

impl PartialEq for ChunkKey {
    fn eq(&self, other: &Self) -> bool {
        self.repo == other.repo
            && self.file_path == other.file_path
            && self.chunk_type == other.chunk_type
            && self.entity_name == other.entity_name
    }
}

impl Eq for ChunkKey {}

impl Hash for ChunkKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.repo.hash(state);
        self.file_path.hash(state);
        self.chunk_type.hash(state);
        self.entity_name.hash(state);
    }
}

impl fmt::Display for ChunkKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}:{}:{}",
            self.repo, self.file_path, self.chunk_type, self.entity_name
        )
    }
}

/// A single label decision persisted as one JSONL line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LabelRecord {
    pub key: ChunkKey,
    /// SHA-256 hex digest of chunk content.
    pub content_hash: String,
    /// Actual code content (for training-data portability).
    pub content: String,
    /// Human-assigned labels.
    pub labels: Vec<String>,
    /// Labels the heuristic pre-filled before human review.
    pub heuristic_labels: Vec<String>,
    /// Source of the labels — typically `"human"`.
    pub label_source: String,
    /// ISO 8601 timestamp of when the review happened.
    pub reviewed_at: String,
}

/// Compute SHA-256 hex digest of content.
pub fn content_hash(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    let result = hasher.finalize();
    // Format as lowercase hex
    result.iter().fold(String::new(), |mut acc, byte| {
        use fmt::Write;
        let _ = write!(acc, "{byte:02x}");
        acc
    })
}

/// A record written to `audit-suspects.jsonl` when the ML classifier disagrees
/// with the human label.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditSuspect {
    pub key: ChunkKey,
    pub content: String,
    pub human_label: String,
    pub ml_label: String,
    pub ml_confidence: f32,
    pub confidence_tier: String,
}

/// Write a list of audit suspects to a JSONL file (overwrites).
pub fn write_audit_suspects(
    path: &std::path::Path,
    suspects: &[AuditSuspect],
) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::File::create(path)?;
    for s in suspects {
        let line = serde_json::to_string(s)?;
        writeln!(file, "{line}")?;
    }
    Ok(())
}

/// Load audit suspects from a JSONL file.
pub fn load_audit_suspects(path: &std::path::Path) -> anyhow::Result<Vec<AuditSuspect>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut suspects = Vec::new();
    for (line_num, line) in reader.lines().enumerate() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<AuditSuspect>(trimmed) {
            Ok(s) => suspects.push(s),
            Err(e) => {
                tracing::warn!(
                    line = line_num + 1,
                    error = %e,
                    "Skipping malformed audit suspect line"
                );
            }
        }
    }
    Ok(suspects)
}

/// JSONL-backed store for human label decisions.
///
/// This file is the **primary training dataset** — each record is a
/// self-contained (code, labels) pair. Records are never deleted.
/// Orphaned records (chunk key no longer in the database) remain valid
/// training data.
///
/// Append-only: every call to `append` adds one line. On load, the last
/// record for each `ChunkKey` wins (latest-write-wins dedup for replay
/// purposes; all records are preserved in the file).
pub struct LabelStore {
    path: PathBuf,
}

impl LabelStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Append a single record to the JSONL file.
    pub fn append(&self, record: &LabelRecord) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        let line = serde_json::to_string(record)?;
        writeln!(file, "{line}")?;
        Ok(())
    }

    /// Remove the last line from the JSONL file (undo support).
    pub fn pop_last(&self) -> anyhow::Result<()> {
        if !self.path.exists() {
            return Ok(());
        }
        let content = std::fs::read_to_string(&self.path)?;
        let lines: Vec<&str> = content.lines().collect();
        if lines.is_empty() {
            return Ok(());
        }
        let mut file = File::create(&self.path)?;
        for line in &lines[..lines.len() - 1] {
            writeln!(file, "{line}")?;
        }
        Ok(())
    }

    /// Load all records from the JSONL file. Latest entry wins for
    /// duplicate keys.
    pub fn load_all(&self) -> anyhow::Result<HashMap<ChunkKey, LabelRecord>> {
        let mut map = HashMap::new();

        if !self.path.exists() {
            return Ok(map);
        }

        let file = File::open(&self.path)?;
        let reader = BufReader::new(file);

        for (line_num, line) in reader.lines().enumerate() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str::<LabelRecord>(trimmed) {
                Ok(record) => {
                    map.insert(record.key.clone(), record);
                }
                Err(e) => {
                    tracing::warn!(
                        line = line_num + 1,
                        error = %e,
                        "Skipping malformed JSONL line"
                    );
                }
            }
        }

        Ok(map)
    }

    /// Quick check: which keys have been reviewed?
    pub fn reviewed_keys(&self) -> anyhow::Result<HashSet<ChunkKey>> {
        let all = self.load_all()?;
        Ok(all.into_keys().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_hash_deterministic() {
        let h1 = content_hash("fn main() {}");
        let h2 = content_hash("fn main() {}");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64); // SHA-256 = 64 hex chars
    }

    #[test]
    fn content_hash_differs_for_different_content() {
        let h1 = content_hash("fn main() {}");
        let h2 = content_hash("fn main() { println!(\"hello\"); }");
        assert_ne!(h1, h2);
    }

    #[test]
    fn chunk_key_equality() {
        let k1 = ChunkKey {
            repo: "my-repo".into(),
            file_path: "src/lib.rs".into(),
            chunk_type: "function".into(),
            entity_name: "do_thing".into(),
        };
        let k2 = ChunkKey {
            repo: "my-repo".into(),
            file_path: "src/lib.rs".into(),
            chunk_type: "function".into(),
            entity_name: "do_thing".into(),
        };
        assert_eq!(k1, k2);
    }

    #[test]
    fn round_trip_jsonl() {
        let dir = std::env::temp_dir().join(format!("label_store_test_{}", std::process::id()));
        let path = dir.join("labels.jsonl");
        let store = LabelStore::new(path.clone());

        let record = LabelRecord {
            key: ChunkKey {
                repo: "test-repo".into(),
                file_path: "src/main.rs".into(),
                chunk_type: "function".into(),
                entity_name: "main".into(),
            },
            content_hash: content_hash("fn main() {}"),
            content: "fn main() {}".into(),
            labels: vec!["good_example".into()],
            heuristic_labels: vec!["function".into()],
            label_source: "human".into(),
            reviewed_at: "2026-03-18T12:00:00Z".into(),
        };

        store.append(&record).unwrap();

        let loaded = store.load_all().unwrap();
        assert_eq!(loaded.len(), 1);

        let loaded_record = loaded.values().next().unwrap();
        assert_eq!(loaded_record.key, record.key);
        assert_eq!(loaded_record.content_hash, record.content_hash);
        assert_eq!(loaded_record.labels, record.labels);

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn latest_wins_dedup() {
        let dir = std::env::temp_dir().join(format!("label_store_dedup_{}", std::process::id()));
        let path = dir.join("labels.jsonl");
        let store = LabelStore::new(path.clone());

        let key = ChunkKey {
            repo: "test-repo".into(),
            file_path: "src/main.rs".into(),
            chunk_type: "function".into(),
            entity_name: "main".into(),
        };

        // First write
        store
            .append(&LabelRecord {
                key: key.clone(),
                content_hash: content_hash("v1"),
                content: "v1".into(),
                labels: vec!["old_label".into()],
                heuristic_labels: vec![],
                label_source: "human".into(),
                reviewed_at: "2026-03-18T12:00:00Z".into(),
            })
            .unwrap();

        // Second write (same key, new labels)
        store
            .append(&LabelRecord {
                key: key.clone(),
                content_hash: content_hash("v2"),
                content: "v2".into(),
                labels: vec!["new_label".into()],
                heuristic_labels: vec![],
                label_source: "human".into(),
                reviewed_at: "2026-03-18T13:00:00Z".into(),
            })
            .unwrap();

        let loaded = store.load_all().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[&key].labels, vec!["new_label".to_string()]);

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }
}
