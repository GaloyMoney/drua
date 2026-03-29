//! ONNX-based SetFit classifier for architectural role prediction.
//!
//! Loads the exported ONNX model (sentence-transformer body), tokenizer, and
//! classification head weights to predict code chunk labels entirely in Rust.
//!
//! Inference pipeline:
//!   tokenize → ONNX embed → mean pool → linear head → softmax → argmax

use std::collections::HashMap;
use std::path::Path;

use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use serde::Deserialize;
use tokenizers::Tokenizer;

use crate::error::CoreError;

/// A single prediction result with label and confidence score.
#[derive(Debug, Clone)]
pub struct Prediction {
    /// The predicted label (e.g. "entity_command").
    pub label: String,
    /// Softmax probability for the predicted class.
    pub confidence: f32,
    /// All class scores sorted by confidence (descending).
    pub scores: Vec<(String, f32)>,
}

/// Deserialized classification head weights from `head_weights.json`.
#[derive(Deserialize)]
struct HeadWeights {
    /// Shape: (n_classes, n_features=768)
    weights: Vec<Vec<f32>>,
    /// Shape: (n_classes,)
    bias: Vec<f32>,
    /// Maps class index (as string) → label name
    label_mapping: HashMap<String, String>,
}

/// Deserialized model config from `config.json`.
#[derive(Deserialize)]
struct ModelConfig {
    /// Expected embedding dimension (e.g. 768).
    #[serde(default = "default_embedding_dim")]
    embedding_dim: usize,
}

fn default_embedding_dim() -> usize {
    768
}

/// ONNX-based SetFit classifier.
///
/// Holds the ONNX runtime session, tokenizer, and classification head
/// for running inference on code chunks.
pub struct Classifier {
    session: Session,
    tokenizer: Tokenizer,
    /// Head weights matrix: shape (n_classes, embedding_dim)
    weights: Vec<Vec<f32>>,
    /// Head bias vector: shape (n_classes,)
    bias: Vec<f32>,
    /// Ordered label list: index → label name
    labels: Vec<String>,
    /// Whether the ONNX model expects token_type_ids input
    needs_token_type_ids: bool,
}

impl std::fmt::Debug for Classifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Classifier")
            .field("labels", &self.labels)
            .field("n_classes", &self.labels.len())
            .field("embedding_dim", &self.weights.first().map(|w| w.len()))
            .finish()
    }
}

impl Classifier {
    /// Load classifier from the ONNX model directory.
    ///
    /// Expects the directory to contain:
    /// - `model.onnx` — quantized sentence-transformer
    /// - `tokenizer.json` — HuggingFace tokenizer config
    /// - `head_weights.json` — linear classification head
    /// - `config.json` — model metadata (optional)
    #[tracing::instrument(name = "classifier.load", skip_all, fields(dir = %dir.as_ref().display()))]
    pub fn load(dir: impl AsRef<Path>) -> Result<Self, CoreError> {
        let dir = dir.as_ref();
        tracing::info!("Loading ONNX classifier from {}", dir.display());

        // Load ONNX session
        let model_path = dir.join("model.onnx");
        let session = Session::builder()
            .map_err(|e| CoreError::Classifier(format!("Failed to create session builder: {e}")))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| CoreError::Classifier(format!("Failed to set optimization level: {e}")))?
            .with_intra_threads(4)
            .map_err(|e| CoreError::Classifier(format!("Failed to set thread count: {e}")))?
            .commit_from_file(&model_path)
            .map_err(|e| {
                CoreError::Classifier(format!(
                    "Failed to load ONNX model from {}: {e}",
                    model_path.display()
                ))
            })?;

        // Check if model expects token_type_ids
        let needs_token_type_ids = session
            .inputs()
            .iter()
            .any(|input| input.name() == "token_type_ids");

        tracing::info!(
            inputs = ?session.inputs().iter().map(|i| i.name().to_string()).collect::<Vec<_>>(),
            outputs = ?session.outputs().iter().map(|o| o.name().to_string()).collect::<Vec<_>>(),
            needs_token_type_ids,
            "ONNX model loaded"
        );

        // Load tokenizer
        let tokenizer_path = dir.join("tokenizer.json");
        let tokenizer = Tokenizer::from_file(&tokenizer_path).map_err(|e| {
            CoreError::Classifier(format!(
                "Failed to load tokenizer from {}: {e}",
                tokenizer_path.display()
            ))
        })?;

        // Load head weights
        let head_path = dir.join("head_weights.json");
        let head_data: HeadWeights = {
            let content = std::fs::read_to_string(&head_path).map_err(|e| {
                CoreError::Classifier(format!(
                    "Failed to read head weights from {}: {e}",
                    head_path.display()
                ))
            })?;
            serde_json::from_str(&content)?
        };

        // Load optional config for validation
        let config_path = dir.join("config.json");
        if config_path.exists() {
            let content = std::fs::read_to_string(&config_path)
                .map_err(|e| CoreError::Classifier(format!("Failed to read config: {e}")))?;
            let config: ModelConfig = serde_json::from_str(&content)?;

            // Validate embedding dimension matches head weights
            if let Some(first_row) = head_data.weights.first() {
                if first_row.len() != config.embedding_dim {
                    return Err(CoreError::Classifier(format!(
                        "Embedding dimension mismatch: config says {} but head weights have {}",
                        config.embedding_dim,
                        first_row.len()
                    )));
                }
            }
        }

        // Build ordered label list from label_mapping
        let n_classes = head_data.weights.len();
        let mut labels = vec![String::new(); n_classes];
        for (idx_str, label) in &head_data.label_mapping {
            let idx: usize = idx_str.parse().map_err(|e| {
                CoreError::Classifier(format!("Invalid label index '{idx_str}': {e}"))
            })?;
            if idx >= n_classes {
                return Err(CoreError::Classifier(format!(
                    "Label index {idx} out of range (n_classes={n_classes})"
                )));
            }
            labels[idx] = label.clone();
        }

        // Ensure no empty labels
        for (i, label) in labels.iter().enumerate() {
            if label.is_empty() {
                return Err(CoreError::Classifier(format!(
                    "Missing label mapping for class index {i}"
                )));
            }
        }

        tracing::info!(
            n_classes,
            labels = ?labels,
            "Classification head loaded"
        );

        Ok(Self {
            session,
            tokenizer,
            weights: head_data.weights,
            bias: head_data.bias,
            labels,
            needs_token_type_ids,
        })
    }

    /// Classify a single code chunk, returning the predicted label and confidence.
    #[tracing::instrument(name = "classifier.classify", skip_all)]
    pub fn classify(&mut self, text: &str) -> Result<Prediction, CoreError> {
        let embedding = self.embed(text)?;
        Ok(self.predict(&embedding))
    }

    /// Classify multiple code chunks in sequence.
    #[tracing::instrument(name = "classifier.classify_batch", skip_all, fields(n = texts.len()))]
    pub fn classify_batch(&mut self, texts: &[&str]) -> Result<Vec<Prediction>, CoreError> {
        texts.iter().map(|text| self.classify(text)).collect()
    }

    /// Return the list of known labels this classifier can predict.
    pub fn labels(&self) -> &[String] {
        &self.labels
    }

    /// Run the ONNX model to get a sentence embedding for the input text.
    fn embed(&mut self, text: &str) -> Result<Vec<f32>, CoreError> {
        // Tokenize
        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| CoreError::Classifier(format!("Tokenization failed: {e}")))?;

        let input_ids: Vec<i64> = encoding.get_ids().iter().map(|&id| id as i64).collect();
        let attention_mask: Vec<i64> = encoding
            .get_attention_mask()
            .iter()
            .map(|&m| m as i64)
            .collect();
        let seq_len = input_ids.len();

        // Create tensors for ONNX input
        let ids_shape = vec![1i64, seq_len as i64];
        let input_ids_tensor = ort::value::Tensor::from_array((ids_shape.clone(), input_ids))
            .map_err(|e| {
                CoreError::Classifier(format!("Failed to create input_ids tensor: {e}"))
            })?;
        let mask_tensor =
            ort::value::Tensor::from_array((ids_shape.clone(), attention_mask.clone())).map_err(
                |e| CoreError::Classifier(format!("Failed to create attention_mask tensor: {e}")),
            )?;

        // Run ONNX inference
        let outputs = if self.needs_token_type_ids {
            let token_type_ids = vec![0i64; seq_len];
            let tti_tensor =
                ort::value::Tensor::from_array((ids_shape, token_type_ids)).map_err(|e| {
                    CoreError::Classifier(format!("Failed to create token_type_ids tensor: {e}"))
                })?;
            self.session
                .run(ort::inputs! {
                    "input_ids" => input_ids_tensor,
                    "attention_mask" => mask_tensor,
                    "token_type_ids" => tti_tensor
                })
                .map_err(|e| CoreError::Classifier(format!("ONNX inference failed: {e}")))?
        } else {
            self.session
                .run(ort::inputs! {
                    "input_ids" => input_ids_tensor,
                    "attention_mask" => mask_tensor
                })
                .map_err(|e| CoreError::Classifier(format!("ONNX inference failed: {e}")))?
        };

        // Extract token embeddings: shape (1, seq_len, hidden_dim)
        let (shape, data) = outputs["last_hidden_state"]
            .try_extract_tensor::<f32>()
            .map_err(|e| CoreError::Classifier(format!("Failed to extract embeddings: {e}")))?;

        // shape is [1, seq_len, hidden_dim]
        let hidden_dim = shape[2] as usize;

        // Mean pooling: attention-mask-aware average of token embeddings
        let mut pooled = vec![0.0f32; hidden_dim];
        let mut mask_sum = 0.0f32;

        for (i, &mask) in attention_mask.iter().enumerate().take(seq_len) {
            let mask_val = mask as f32;
            if mask_val > 0.0 {
                let offset = i * hidden_dim;
                for j in 0..hidden_dim {
                    pooled[j] += data[offset + j] * mask_val;
                }
                mask_sum += mask_val;
            }
        }

        if mask_sum > 0.0 {
            for val in pooled.iter_mut() {
                *val /= mask_sum;
            }
        }

        Ok(pooled)
    }

    /// Apply the classification head: logits = embedding @ weights^T + bias, then softmax.
    fn predict(&self, embedding: &[f32]) -> Prediction {
        let n_classes = self.weights.len();

        // Compute logits: for each class, dot(embedding, weights[class]) + bias[class]
        let mut logits = Vec::with_capacity(n_classes);
        for (class_weights, &class_bias) in self.weights.iter().zip(self.bias.iter()) {
            let dot: f32 = embedding
                .iter()
                .zip(class_weights.iter())
                .map(|(e, w)| e * w)
                .sum();
            logits.push(dot + class_bias);
        }

        // Softmax
        let probabilities = softmax(&logits);

        // Build sorted scores
        let mut scores: Vec<(String, f32)> = self
            .labels
            .iter()
            .zip(probabilities.iter())
            .map(|(label, &prob)| (label.clone(), prob))
            .collect();
        scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        let best_label = scores[0].0.clone();
        let best_confidence = scores[0].1;

        Prediction {
            label: best_label,
            confidence: best_confidence,
            scores,
        }
    }
}

/// Numerically stable softmax over a slice of logits.
fn softmax(logits: &[f32]) -> Vec<f32> {
    let max_logit = logits.iter().copied().fold(f32::NEG_INFINITY, f32::max);

    let exp_values: Vec<f32> = logits.iter().map(|&x| (x - max_logit).exp()).collect();
    let sum: f32 = exp_values.iter().sum();

    exp_values.iter().map(|&e| e / sum).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_softmax_basic() {
        let logits = vec![1.0, 2.0, 3.0];
        let probs = softmax(&logits);

        // Sum should be ~1.0
        let sum: f32 = probs.iter().sum();
        assert!((sum - 1.0).abs() < 1e-6);

        // Highest logit should have highest probability
        assert!(probs[2] > probs[1]);
        assert!(probs[1] > probs[0]);
    }

    #[test]
    fn test_softmax_numerical_stability() {
        // Large logits should not overflow
        let logits = vec![1000.0, 1001.0, 1002.0];
        let probs = softmax(&logits);

        let sum: f32 = probs.iter().sum();
        assert!((sum - 1.0).abs() < 1e-6);
        assert!(probs[2] > probs[1]);
    }

    #[test]
    fn test_softmax_uniform() {
        let logits = vec![0.0, 0.0, 0.0];
        let probs = softmax(&logits);

        for p in &probs {
            assert!((*p - 1.0 / 3.0).abs() < 1e-6);
        }
    }

    /// Helper to run the classification head logic without needing a full Session.
    fn predict_from_head(
        weights: &[Vec<f32>],
        bias: &[f32],
        labels: &[String],
        embedding: &[f32],
    ) -> Prediction {
        let n_classes = weights.len();
        let mut logits = Vec::with_capacity(n_classes);
        for (class_weights, &class_bias) in weights.iter().zip(bias.iter()) {
            let dot: f32 = embedding
                .iter()
                .zip(class_weights.iter())
                .map(|(e, w)| e * w)
                .sum();
            logits.push(dot + class_bias);
        }
        let probabilities = softmax(&logits);
        let mut scores: Vec<(String, f32)> = labels
            .iter()
            .zip(probabilities.iter())
            .map(|(label, &prob)| (label.clone(), prob))
            .collect();
        scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let best_label = scores[0].0.clone();
        let best_confidence = scores[0].1;
        Prediction {
            label: best_label,
            confidence: best_confidence,
            scores,
        }
    }

    #[test]
    fn test_predict_with_known_weights() {
        let weights = vec![
            vec![1.0, 0.0], // class 0
            vec![0.0, 1.0], // class 1
        ];
        let bias = vec![0.0, 0.0];
        let labels = vec!["alpha".to_string(), "beta".to_string()];

        // Embedding that strongly favors class 0 (alpha)
        let pred = predict_from_head(&weights, &bias, &labels, &[5.0, 0.0]);
        assert_eq!(pred.label, "alpha");
        assert!(pred.confidence > 0.99);

        // Embedding that strongly favors class 1 (beta)
        let pred = predict_from_head(&weights, &bias, &labels, &[0.0, 5.0]);
        assert_eq!(pred.label, "beta");
        assert!(pred.confidence > 0.99);
    }

    #[test]
    fn test_predict_with_bias() {
        let weights = vec![vec![1.0, 0.0], vec![1.0, 0.0]];
        let bias = vec![0.0, 10.0]; // Strong bias toward class 1
        let labels = vec!["a".to_string(), "b".to_string()];

        let pred = predict_from_head(&weights, &bias, &labels, &[0.0, 0.0]);
        assert_eq!(pred.label, "b");
    }
}
