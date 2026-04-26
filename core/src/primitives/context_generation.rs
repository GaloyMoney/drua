use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Cross-cutting "something changed" signal for workspace context (notes/skills).
/// Bumped on every Notes/Skills mutation; read by Agents to detect cache staleness.
/// HA-safe via PG LISTEN/NOTIFY: the App spawns a listener that bumps this on
/// `context_changed` notifications from other instances.
#[derive(Clone)]
pub struct ContextGeneration(Arc<AtomicU64>);

impl ContextGeneration {
    pub fn new() -> Self {
        Self(Arc::new(AtomicU64::new(0)))
    }

    pub fn bump(&self) {
        self.0.fetch_add(1, Ordering::Release);
    }

    pub fn current(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }
}

impl Default for ContextGeneration {
    fn default() -> Self {
        Self::new()
    }
}
