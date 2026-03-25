use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A stored memory — observation, decision, research finding, or knowledge.
///
/// Memories decay over time unless pinned or persistent.
/// Persistent memories are exempt from decay.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub id: String,
    pub title: String,
    pub content: String,
    pub tags: Vec<String>,
    pub project: Option<String>,
    pub source_task: Option<String>,
    pub source_type: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_accessed: Option<DateTime<Utc>>,
    pub access_count: i64,
    pub pinned: bool,
    /// Persistent memories are exempt from decay.
    pub persistent: bool,
}

/// Input for creating a new memory.
#[derive(Debug, Clone)]
pub struct NewMemory {
    pub title: String,
    pub content: String,
    pub tags: Vec<String>,
    pub project: Option<String>,
    pub source_task: Option<String>,
    pub source_type: String,
    /// When true, the memory is exempt from decay.
    pub persistent: bool,
}

/// A search result with relevance scoring and decay information.
#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    pub id: String,
    pub title: String,
    pub content: String,
    pub tags: Vec<String>,
    pub project: Option<String>,
    pub score: f64,
    /// Decay factor (1.0 = fully fresh, 0.0 = fully decayed).
    /// Persistent or pinned memories always have 1.0.
    pub decay_factor: f64,
    pub pinned: bool,
    pub persistent: bool,
}
