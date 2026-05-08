//! `StringClassifier` — typed extraction at string leaves.
//!
//! The walker is the structural spine: it descends a `Value` and, at
//! every string leaf, asks the registered `StringClassifier` chain
//! whether anything wants to type the bytes. The first classifier
//! whose `classify` returns `Some(Value)` wins; the walker substitutes
//! that value for the original string in `kept`.
//!
//! When no classifier matches, the walker falls back to its existing
//! byte-elision behaviour (head + tail of the string, in place,
//! keeping the `Value::String` type). That fallback IS the generic
//! string classifier — it doesn't need to be a separate trait impl.
//!
//! Convention for the substitute value: an object with a `_typed`
//! discriminator field and the typed summary inline.
//!
//! ```ignore
//! { "_typed": "nix_build",
//!   "summary": { "derivations_attempted": 4, "failures": [...] } }
//! ```
//!
//! Same shape any future content classifier should follow (Cargo,
//! Nextest, cpp-compile, ...). Agents recognise typed regions by
//! probing `obj["_typed"]`.

pub trait StringClassifier: Send + Sync + 'static {
    fn name(&self) -> &str;

    /// Inspect `text` and decide whether to claim it. `Some(Value)` is
    /// the typed substitute the walker embeds at the string's
    /// position; `None` falls through to the next classifier in the
    /// chain. Implementations conventionally tag with a `_typed`
    /// discriminator (see module docs).
    fn classify(&self, text: &str) -> Option<serde_json::Value>;
}

/// Holds the registered chain. First match wins.
pub struct StringClassifierChain {
    classifiers: Vec<Box<dyn StringClassifier>>,
}

impl StringClassifierChain {
    pub fn new() -> Self {
        Self {
            classifiers: Vec::new(),
        }
    }

    pub fn register(mut self, classifier: impl StringClassifier) -> Self {
        self.classifiers.push(Box::new(classifier));
        self
    }

    /// Walk the chain; first `Some(Value)` wins. `None` when no
    /// classifier matched — the walker's byte-elision fallback runs.
    pub fn classify(&self, text: &str) -> Option<serde_json::Value> {
        for c in &self.classifiers {
            if let Some(v) = c.classify(text) {
                return Some(v);
            }
        }
        None
    }
}

impl Default for StringClassifierChain {
    fn default() -> Self {
        Self::new()
    }
}
