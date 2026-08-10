//! Anti-pattern triage state and persistence.
//!
//! Curates raw inline review entries (e.g. `@@` markers) into a clean
//! anti-pattern catalog with categorized examples. Pure I/O + state — both
//! the TUI and any future automation drive these helpers.

use std::collections::HashSet;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::review_session::now_iso8601;

pub const SOURCE_INLINE_REVIEW: &str = "inline-review";

/// One raw entry from `~/.claude/data/inline-reviews.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InlineReview {
    pub file: String,
    pub line: u32,
    pub comment: String,
    #[serde(default)]
    pub context: String,
    #[serde(default)]
    pub repo: String,
    #[serde(default)]
    pub timestamp: String,
}

/// Curated anti-pattern entry persisted to `anti-patterns.toml`.
///
/// `category` is optional and intentionally left unset at triage time —
/// taxonomy is better designed in one pass over the full catalog later.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Pattern {
    pub slug: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    pub canonical_comment: String,
    #[serde(default = "default_active")]
    pub active: bool,
}

fn default_active() -> bool {
    true
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PatternFile {
    #[serde(default, rename = "pattern")]
    patterns: Vec<Pattern>,
}

/// Linked code example for a pattern, appended to `anti-pattern-examples.jsonl`.
///
/// A single snippet often violates multiple anti-patterns simultaneously, so
/// the schema supports multi-attach: `primary_slug` is required, plus an
/// optional `additional_slugs` array.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Example {
    pub primary_slug: String,
    #[serde(default)]
    pub additional_slugs: Vec<String>,
    pub code: String,
    pub source: String,
    /// Stable hash of the originating entry — matches `Discarded.source_id`.
    /// Optional for backward compatibility with externally-imported examples.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    pub original_comment: String,
    pub file: String,
    pub repo: String,
    pub triaged_at: String,
}

/// A discarded entry — appended to `anti-pattern-discarded.jsonl` and used to
/// dedup on subsequent triage sessions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Discarded {
    pub source: String,
    pub source_id: String,
    pub reason: String,
    pub triaged_at: String,
}

/// Heuristic pre-classification used to sort entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeuristicGuess {
    StyleLikely,
    LogicLikely,
    Unsure,
}

impl HeuristicGuess {
    pub fn label(self) -> &'static str {
        match self {
            Self::StyleLikely => "style-likely",
            Self::LogicLikely => "logic-likely",
            Self::Unsure => "unsure",
        }
    }

    fn sort_rank(self) -> u8 {
        match self {
            Self::Unsure => 0,
            Self::StyleLikely => 1,
            Self::LogicLikely => 2,
        }
    }
}

impl fmt::Display for HeuristicGuess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

const STYLE_KEYWORDS: &[&str] = &[
    "use ",
    "prefer",
    "avoid",
    "always",
    "never",
    "should",
    "convention",
    "anti-pattern",
    "instead",
    "rather than",
];

const LOGIC_KEYWORDS: &[&str] = &[
    "off-by-one",
    "null",
    "race",
    "wrong",
    "bug",
    "missing",
    "crash",
    "panic",
    "deadlock",
];

pub fn classify_comment(comment: &str) -> HeuristicGuess {
    let lower = comment.to_lowercase();
    let style = STYLE_KEYWORDS.iter().any(|kw| lower.contains(kw));
    let logic = LOGIC_KEYWORDS.iter().any(|kw| lower.contains(kw));
    match (style, logic) {
        (true, false) => HeuristicGuess::StyleLikely,
        (false, true) => HeuristicGuess::LogicLikely,
        _ => HeuristicGuess::Unsure,
    }
}

pub fn source_id_inline(file: &str, line: u32, comment: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(file.as_bytes());
    hasher.update([0u8]);
    hasher.update(line.to_string().as_bytes());
    hasher.update([0u8]);
    hasher.update(comment.as_bytes());
    let result = hasher.finalize();
    let mut s = String::with_capacity(64);
    use fmt::Write;
    for byte in result.iter() {
        let _ = write!(s, "{byte:02x}");
    }
    s
}

pub fn load_inline_reviews(path: &Path) -> anyhow::Result<Vec<InlineReview>> {
    load_jsonl::<InlineReview>(path)
}

/// Load the curated catalog. Returns empty if the file does not exist.
pub fn load_patterns(path: &Path) -> anyhow::Result<Vec<Pattern>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let parsed: PatternFile = toml::from_str(&text)?;
    Ok(parsed.patterns)
}

pub fn save_patterns(path: &Path, patterns: &[Pattern]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = PatternFile {
        patterns: patterns.to_vec(),
    };
    let text = toml::to_string_pretty(&file)?;
    std::fs::write(path, text)?;
    Ok(())
}

pub fn append_example(path: &Path, example: &Example) -> anyhow::Result<()> {
    append_jsonl(path, example)
}

pub fn append_discarded(path: &Path, discarded: &Discarded) -> anyhow::Result<()> {
    append_jsonl(path, discarded)
}

fn append_jsonl<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    let line = serde_json::to_string(value)?;
    writeln!(file, "{line}")?;
    Ok(())
}

/// Source-id set + counts loaded in a single pass over the JSONL files.
pub struct TriagedHistory {
    pub source_ids: HashSet<String>,
    pub example_count: usize,
    pub discarded_count: usize,
}

/// Load all `source_id`s already triaged (examples + discarded), returning
/// counts so callers don't have to re-read the files.
pub fn load_triaged_history(
    examples_path: &Path,
    discarded_path: &Path,
) -> anyhow::Result<TriagedHistory> {
    let examples = load_examples(examples_path)?;
    let discarded = load_discarded(discarded_path)?;
    let mut ids = HashSet::with_capacity(examples.len() + discarded.len());
    for example in &examples {
        if let Some(id) = &example.source_id {
            ids.insert(id.clone());
        }
    }
    for d in &discarded {
        ids.insert(d.source_id.clone());
    }
    Ok(TriagedHistory {
        source_ids: ids,
        example_count: examples.len(),
        discarded_count: discarded.len(),
    })
}

pub fn load_examples(path: &Path) -> anyhow::Result<Vec<Example>> {
    load_jsonl(path)
}

pub fn load_discarded(path: &Path) -> anyhow::Result<Vec<Discarded>> {
    load_jsonl(path)
}

fn load_jsonl<T: for<'de> Deserialize<'de>>(path: &Path) -> anyhow::Result<Vec<T>> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let reader = BufReader::new(file);
    let mut out = Vec::new();
    for (line_num, line) in reader.lines().enumerate() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<T>(trimmed) {
            Ok(v) => out.push(v),
            Err(e) => {
                tracing::warn!(
                    line = line_num + 1,
                    path = %path.display(),
                    error = %e,
                    "Skipping malformed JSONL line"
                );
            }
        }
    }
    Ok(out)
}

/// Sort entries: unsure first, style next, logic last. Stable for ties.
pub fn sort_by_heuristic(entries: &mut [TriageEntry]) {
    entries.sort_by_key(|e| e.guess.sort_rank());
}

/// One entry queued for triage — raw review + heuristic classification.
#[derive(Debug, Clone)]
pub struct TriageEntry {
    pub review: InlineReview,
    pub source_id: String,
    pub guess: HeuristicGuess,
}

impl TriageEntry {
    pub fn from_inline(review: InlineReview) -> Self {
        let source_id = source_id_inline(&review.file, review.line, &review.comment);
        let guess = classify_comment(&review.comment);
        Self {
            review,
            source_id,
            guess,
        }
    }
}

/// Build the queue: load inline reviews, drop already-triaged ones, sort.
pub fn build_queue(
    inline_path: &Path,
    examples_path: &Path,
    discarded_path: &Path,
) -> anyhow::Result<QueueLoadStats> {
    let reviews = load_inline_reviews(inline_path)?;
    let total = reviews.len();
    let history = load_triaged_history(examples_path, discarded_path)?;

    let mut entries: Vec<TriageEntry> = reviews
        .into_iter()
        .map(TriageEntry::from_inline)
        .filter(|e| !history.source_ids.contains(&e.source_id))
        .collect();
    sort_by_heuristic(&mut entries);

    Ok(QueueLoadStats {
        entries,
        total,
        already_triaged: history.source_ids.len(),
        discarded_count: history.discarded_count,
        example_count: history.example_count,
    })
}

#[derive(Debug)]
pub struct QueueLoadStats {
    pub entries: Vec<TriageEntry>,
    pub total: usize,
    pub already_triaged: usize,
    pub discarded_count: usize,
    pub example_count: usize,
}

/// Default repo-relative paths for the persistence files.
pub fn default_catalog_path(labels_dir: &Path) -> PathBuf {
    labels_dir.join("anti-patterns.toml")
}

pub fn default_examples_path(labels_dir: &Path) -> PathBuf {
    labels_dir.join("anti-pattern-examples.jsonl")
}

pub fn default_discarded_path(labels_dir: &Path) -> PathBuf {
    labels_dir.join("anti-pattern-discarded.jsonl")
}

/// Produce a discard record stamped with the current ISO 8601 time.
pub fn make_discarded(source: &str, source_id: &str, reason: &str) -> Discarded {
    Discarded {
        source: source.to_string(),
        source_id: source_id.to_string(),
        reason: reason.to_string(),
        triaged_at: now_iso8601(),
    }
}

/// Inputs needed to build an `Example`. Avoids a wide positional API.
pub struct NewExample<'a> {
    pub primary_slug: &'a str,
    pub additional_slugs: Vec<String>,
    pub code: &'a str,
    pub source: &'a str,
    pub source_id: Option<String>,
    pub original_comment: &'a str,
    pub file: &'a str,
    pub repo: &'a str,
}

/// Produce an example record stamped with the current ISO 8601 time.
pub fn make_example(input: NewExample<'_>) -> Example {
    Example {
        primary_slug: input.primary_slug.to_string(),
        additional_slugs: input.additional_slugs,
        code: input.code.to_string(),
        source: input.source.to_string(),
        source_id: input.source_id,
        original_comment: input.original_comment.to_string(),
        file: input.file.to_string(),
        repo: input.repo.to_string(),
        triaged_at: now_iso8601(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_style_keywords() {
        assert_eq!(
            classify_comment("Use ? with From impl instead of map_err."),
            HeuristicGuess::StyleLikely
        );
        assert_eq!(
            classify_comment("Prefer the builder pattern here."),
            HeuristicGuess::StyleLikely
        );
    }

    #[test]
    fn classify_logic_keywords() {
        assert_eq!(
            classify_comment("This is an off-by-one error."),
            HeuristicGuess::LogicLikely
        );
        assert_eq!(
            classify_comment("Will panic if input is empty."),
            HeuristicGuess::LogicLikely
        );
    }

    #[test]
    fn classify_unsure() {
        assert_eq!(
            classify_comment("Why is this here?"),
            HeuristicGuess::Unsure
        );
    }

    #[test]
    fn classify_both_falls_back_to_unsure() {
        // Style keyword AND logic keyword present — ambiguous.
        let g = classify_comment("Avoid this — it will panic.");
        assert_eq!(g, HeuristicGuess::Unsure);
    }

    #[test]
    fn source_id_is_deterministic() {
        let a = source_id_inline("src/lib.rs", 10, "use ?");
        let b = source_id_inline("src/lib.rs", 10, "use ?");
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn source_id_changes_with_inputs() {
        let a = source_id_inline("src/lib.rs", 10, "use ?");
        let b = source_id_inline("src/lib.rs", 11, "use ?");
        let c = source_id_inline("src/main.rs", 10, "use ?");
        let d = source_id_inline("src/lib.rs", 10, "use From");
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d);
    }

    #[test]
    fn pattern_round_trip_toml() {
        let dir = std::env::temp_dir().join(format!("triage_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("anti-patterns.toml");

        let patterns = vec![
            Pattern {
                slug: "manual_error_mapping".into(),
                category: None,
                canonical_comment: "Use ? with From impl instead of map_err.".into(),
                active: true,
            },
            Pattern {
                slug: "raw_uuid".into(),
                category: Some("domain-primitives".into()),
                canonical_comment: "Use a strongly-typed id, not raw Uuid.".into(),
                active: true,
            },
        ];
        save_patterns(&path, &patterns).unwrap();

        // A pattern with no category omits the field entirely; only the
        // categorized pattern emits one `category =` line.
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("category =").count(), 1);

        let loaded = load_patterns(&path).unwrap();
        assert_eq!(loaded, patterns);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn example_and_discard_jsonl_round_trip() {
        let dir = std::env::temp_dir().join(format!("triage_jsonl_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ex_path = dir.join("ex.jsonl");
        let dc_path = dir.join("dc.jsonl");

        let example = make_example(NewExample {
            primary_slug: "manual_error_mapping",
            additional_slugs: vec!["missing_instrument".into()],
            code: "fn foo() {}",
            source: SOURCE_INLINE_REVIEW,
            source_id: Some("abc123".into()),
            original_comment: "use ?",
            file: "src/lib.rs",
            repo: "lana-bank",
        });
        let discarded = make_discarded(SOURCE_INLINE_REVIEW, "abc123", "logic");

        append_example(&ex_path, &example).unwrap();
        append_discarded(&dc_path, &discarded).unwrap();

        let exs = load_examples(&ex_path).unwrap();
        let dcs = load_discarded(&dc_path).unwrap();
        assert_eq!(exs.len(), 1);
        assert_eq!(dcs.len(), 1);
        assert_eq!(exs[0].primary_slug, "manual_error_mapping");
        assert_eq!(
            exs[0].additional_slugs,
            vec!["missing_instrument".to_string()]
        );
        assert_eq!(dcs[0].source_id, "abc123");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sort_orders_unsure_first() {
        let mut entries = vec![
            TriageEntry::from_inline(InlineReview {
                file: "f1".into(),
                line: 1,
                comment: "this will panic".into(),
                context: String::new(),
                repo: String::new(),
                timestamp: String::new(),
            }),
            TriageEntry::from_inline(InlineReview {
                file: "f2".into(),
                line: 2,
                comment: "?".into(),
                context: String::new(),
                repo: String::new(),
                timestamp: String::new(),
            }),
            TriageEntry::from_inline(InlineReview {
                file: "f3".into(),
                line: 3,
                comment: "use ? here".into(),
                context: String::new(),
                repo: String::new(),
                timestamp: String::new(),
            }),
        ];
        sort_by_heuristic(&mut entries);
        assert_eq!(entries[0].guess, HeuristicGuess::Unsure);
        assert_eq!(entries[1].guess, HeuristicGuess::StyleLikely);
        assert_eq!(entries[2].guess, HeuristicGuess::LogicLikely);
    }
}
