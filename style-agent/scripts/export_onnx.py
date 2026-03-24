#!/usr/bin/env python3
"""Phase 2: Export trained SetFit model to ONNX for Rust inference.

Produces four artifacts in models/onnx/:
1. model.onnx — the fine-tuned sentence-transformer body (int8 quantized)
2. tokenizer.json — tokenizer config for the `tokenizers` Rust crate
3. head_weights.json — logistic regression weights for pure-Rust inference
4. config.json — model metadata

The Rust inference pipeline will:
  tokenize (tokenizers crate) → embed (ort crate) → mean pool → matmul + argmax

Uses torch.onnx.export directly (not optimum.exporters) since the nixpkgs
optimum package doesn't include the exporters subpackage.

Usage:
    nix develop .#training -c python3 scripts/export_onnx.py
"""

import json
import os
import shutil
import sys
from pathlib import Path

# Force CPU — nomic-bert at full 2048-token context exhausts MPS memory on macOS.
# Must be set before torch is imported.
os.environ["CUDA_VISIBLE_DEVICES"] = ""
os.environ["NO_TORCH_MPS"] = "1"

import joblib
import numpy as np
import onnxruntime as ort
import torch

# Ensure MPS is not used even if available
torch.backends.mps.is_available = lambda: False

from onnxruntime.quantization import QuantType, quantize_dynamic
from sentence_transformers import SentenceTransformer

MODEL_DIR = Path("models/setfit-label-classifier")
ONNX_DIR = Path("models/onnx")
ST_DIR = MODEL_DIR / "sentence-transformer"


def export_sentence_transformer(st_model: SentenceTransformer) -> Path:
    """Export the sentence-transformer body to ONNX via torch.onnx.export."""
    print("Exporting sentence-transformer to ONNX...")

    raw_onnx = ONNX_DIR / "model_raw.onnx"

    # Get the underlying transformer model (first module in the ST pipeline)
    transformer = st_model[0]
    hf_model = transformer.auto_model
    tokenizer = st_model.tokenizer

    hf_model.eval()

    # Create dummy input
    dummy_text = "fn main() {}"
    encoded = tokenizer(
        dummy_text,
        padding="max_length",
        truncation=True,
        max_length=128,
        return_tensors="pt",
    )

    input_ids = encoded["input_ids"]
    attention_mask = encoded["attention_mask"]

    # Build dynamic axes for variable sequence length and batch size
    dynamic_axes = {
        "input_ids": {0: "batch_size", 1: "sequence_length"},
        "attention_mask": {0: "batch_size", 1: "sequence_length"},
        "last_hidden_state": {0: "batch_size", 1: "sequence_length"},
    }

    input_names = ["input_ids", "attention_mask"]
    inputs = (input_ids, attention_mask)

    # Check if model uses token_type_ids
    if "token_type_ids" in encoded:
        token_type_ids = encoded["token_type_ids"]
        input_names.append("token_type_ids")
        inputs = (input_ids, attention_mask, token_type_ids)
        dynamic_axes["token_type_ids"] = {0: "batch_size", 1: "sequence_length"}

    # Export
    with torch.no_grad():
        torch.onnx.export(
            hf_model,
            inputs,
            str(raw_onnx),
            input_names=input_names,
            output_names=["last_hidden_state"],
            dynamic_axes=dynamic_axes,
            opset_version=18,
            do_constant_folding=True,
        )

    size_mb = raw_onnx.stat().st_size / (1024 * 1024)
    print(f"  Exported raw model: {size_mb:.1f} MB")
    return raw_onnx


def quantize_model(src: Path) -> Path:
    """Apply int8 dynamic quantization to reduce model size."""
    print("Quantizing model to int8...")
    dst = ONNX_DIR / "model.onnx"

    quantize_dynamic(
        model_input=src,
        model_output=dst,
        weight_type=QuantType.QInt8,
    )

    original_size = src.stat().st_size / (1024 * 1024)
    quantized_size = dst.stat().st_size / (1024 * 1024)
    print(f"  Original:  {original_size:.1f} MB")
    print(f"  Quantized: {quantized_size:.1f} MB")
    print(f"  Reduction: {(1 - quantized_size / original_size) * 100:.0f}%")

    # Remove the raw model
    src.unlink()

    return dst


def copy_tokenizer():
    """Copy tokenizer.json for the Rust tokenizers crate."""
    print("Copying tokenizer...")

    candidates = [
        ST_DIR / "tokenizer.json",
    ]

    src = None
    for c in candidates:
        if c.exists():
            src = c
            break

    if src is None:
        print("ERROR: tokenizer.json not found in model directory", file=sys.stderr)
        print(f"  Searched: {[str(c) for c in candidates]}")
        sys.exit(1)

    dst = ONNX_DIR / "tokenizer.json"
    shutil.copy2(src, dst)
    print(f"  Copied {src} → {dst}")


def export_head_weights():
    """Extract logistic regression weights to JSON for Rust inference."""
    print("Extracting classification head weights...")

    head_path = MODEL_DIR / "head.joblib"
    if not head_path.exists():
        print(f"ERROR: {head_path} not found", file=sys.stderr)
        sys.exit(1)

    head = joblib.load(head_path)

    label_mapping_path = MODEL_DIR / "label_mapping.json"
    with open(label_mapping_path) as f:
        label_mapping = json.load(f)

    # Extract weights: shape (n_classes, n_features) and bias: shape (n_classes,)
    weights = head.coef_.tolist()
    bias = head.intercept_.tolist()

    head_data = {
        "weights": weights,
        "bias": bias,
        "label_mapping": label_mapping,
    }

    dst = ONNX_DIR / "head_weights.json"
    with open(dst, "w") as f:
        json.dump(head_data, f)

    n_classes = len(weights)
    n_features = len(weights[0]) if weights else 0
    print(f"  Head shape: ({n_classes} classes, {n_features} features)")
    print(f"  Labels: {list(label_mapping.values())}")
    print(f"  Saved to {dst}")

    return head, label_mapping


def save_config():
    """Save model metadata config."""
    print("Saving config...")

    meta_path = MODEL_DIR / "training_metadata.json"
    metadata = {}
    if meta_path.exists():
        with open(meta_path) as f:
            metadata = json.load(f)

    config = {
        "model_type": "setfit-onnx",
        "base_model": metadata.get("base_model", "nomic-ai/nomic-embed-text-v1.5"),
        "embedding_dim": 768,
        "quantization": "int8",
        "pooling": "mean",
        "num_classes": metadata.get("num_classes"),
        "classes": metadata.get("classes"),
        "accuracy": metadata.get("accuracy"),
        "inference_steps": [
            "1. Tokenize input with tokenizer.json (tokenizers crate)",
            "2. Run ONNX model (ort crate) to get token embeddings",
            "3. Mean-pool token embeddings (attention mask aware)",
            "4. Multiply by head weights matrix + add bias",
            "5. Argmax to get predicted class index",
            "6. Map index to label via label_mapping",
        ],
    }

    dst = ONNX_DIR / "config.json"
    with open(dst, "w") as f:
        json.dump(config, f, indent=2)
    print(f"  Saved to {dst}")


def verify_onnx(head, label_mapping):
    """Verify ONNX output matches PyTorch model output."""
    print("\nVerifying ONNX model...")

    st_model = SentenceTransformer(str(ST_DIR), trust_remote_code=True, device="cpu")

    sample_text = "pub struct CustomerError { ... }"
    print(f"  Sample input: '{sample_text}'")

    # PyTorch embedding
    pt_embedding = st_model.encode([sample_text])[0]

    # ONNX embedding
    onnx_model_path = str(ONNX_DIR / "model.onnx")
    session = ort.InferenceSession(onnx_model_path)

    tokenizer = st_model.tokenizer
    encoded = tokenizer(
        sample_text,
        padding=True,
        truncation=True,
        max_length=512,
        return_tensors="np",
    )

    input_names = [inp.name for inp in session.get_inputs()]
    feeds = {}
    for name in input_names:
        if name == "input_ids":
            feeds[name] = encoded["input_ids"]
        elif name == "attention_mask":
            feeds[name] = encoded["attention_mask"]
        elif name == "token_type_ids":
            if "token_type_ids" in encoded:
                feeds[name] = encoded["token_type_ids"]
            else:
                feeds[name] = np.zeros_like(encoded["input_ids"])

    outputs = session.run(None, feeds)

    # Mean pooling (same as sentence-transformers default)
    token_embeddings = outputs[0]  # (1, seq_len, hidden_dim)
    attention_mask = encoded["attention_mask"]
    mask_expanded = np.expand_dims(attention_mask, -1)  # (1, seq_len, 1)
    sum_embeddings = np.sum(token_embeddings * mask_expanded, axis=1)
    sum_mask = np.sum(mask_expanded, axis=1)
    onnx_embedding = (sum_embeddings / sum_mask)[0]

    # Compare embeddings
    cosine_sim = np.dot(pt_embedding, onnx_embedding) / (
        np.linalg.norm(pt_embedding) * np.linalg.norm(onnx_embedding)
    )
    print(f"  Cosine similarity (PyTorch vs ONNX): {cosine_sim:.6f}")

    if cosine_sim < 0.95:
        print("  WARNING: Cosine similarity is low — embeddings diverge significantly")
    else:
        print("  OK: Embeddings match closely")

    # Compare predictions
    pt_pred = head.predict(pt_embedding.reshape(1, -1))[0]
    onnx_pred = head.predict(onnx_embedding.reshape(1, -1))[0]
    print(f"  PyTorch prediction: {pt_pred}")
    print(f"  ONNX prediction:    {onnx_pred}")
    if pt_pred == onnx_pred:
        print("  OK: Predictions match")
    else:
        print("  WARNING: Predictions differ (may be due to quantization)")


def main():
    if not ST_DIR.exists():
        print(f"ERROR: Trained model not found at {ST_DIR}", file=sys.stderr)
        print("  Run `make train` first.", file=sys.stderr)
        sys.exit(1)

    ONNX_DIR.mkdir(parents=True, exist_ok=True)

    # Step 1: Export sentence-transformer to ONNX
    st_model = SentenceTransformer(str(ST_DIR), trust_remote_code=True, device="cpu")
    raw_onnx = export_sentence_transformer(st_model)

    # Step 2: Quantize
    quantize_model(raw_onnx)

    # Step 3: Copy tokenizer
    copy_tokenizer()

    # Step 4: Export head weights
    head, label_mapping = export_head_weights()

    # Step 5: Save config
    save_config()

    # Step 6: Verify
    verify_onnx(head, label_mapping)

    print("\n" + "=" * 70)
    print("ONNX EXPORT COMPLETE")
    print("=" * 70)
    print(f"\nArtifacts in {ONNX_DIR}/:")
    for p in sorted(ONNX_DIR.iterdir()):
        size = p.stat().st_size
        if size > 1024 * 1024:
            print(f"  {p.name:30s} {size / (1024 * 1024):.1f} MB")
        elif size > 1024:
            print(f"  {p.name:30s} {size / 1024:.1f} KB")
        else:
            print(f"  {p.name:30s} {size} B")
    print("\nDone!")


if __name__ == "__main__":
    main()
