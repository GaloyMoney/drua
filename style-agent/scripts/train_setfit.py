#!/usr/bin/env python3
"""Phase 1: SetFit-style training for code label classification.

Implements the SetFit approach (contrastive fine-tuning + logistic head)
using sentence-transformers and sklearn directly, since the `setfit`
package is not available in nixpkgs.

The SetFit approach:
1. Generate contrastive pairs (positive = same label, negative = different label)
2. Fine-tune a sentence-transformer with contrastive loss
3. Train a logistic regression head on the resulting embeddings

Usage:
    nix develop .#training -c python3 scripts/train_setfit.py
"""

import json
import random
import sys
from collections import Counter
from itertools import combinations
from pathlib import Path

import os

# Force CPU — nomic-bert at full 2048-token context exhausts MPS memory on macOS.
# Must be set before torch is imported.
os.environ["CUDA_VISIBLE_DEVICES"] = ""
os.environ["NO_TORCH_MPS"] = "1"

import numpy as np
import torch
# Ensure MPS is not used even if available
torch.backends.mps.is_available = lambda: False
from sentence_transformers import InputExample, SentenceTransformer, losses
from sklearn.linear_model import LogisticRegression
from sklearn.metrics import classification_report, confusion_matrix
from sklearn.model_selection import train_test_split
from torch.utils.data import DataLoader

LABELS_PATH = Path("labels/labels.jsonl")
MODEL_DIR = Path("models/setfit-label-classifier")
BASE_MODEL = "nomic-ai/nomic-embed-text-v1.5"
MIN_EXAMPLES = 14
NUM_ITERATIONS = 20  # contrastive pairs per class
BATCH_SIZE = 16
NUM_EPOCHS = 1
RANDOM_SEED = 42


def load_human_labels(path: Path) -> list[dict]:
    """Load records from JSONL, keeping only human-reviewed entries (deduped)."""
    seen_keys = {}
    with open(path) as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            record = json.loads(line)
            key = (
                record["key"]["repo"],
                record["key"]["file_path"],
                record["key"]["chunk_type"],
                record["key"]["entity_name"],
            )
            seen_keys[key] = record

    return [
        r
        for r in seen_keys.values()
        if r.get("label_source") == "human" and r.get("labels")
    ]


def generate_contrastive_pairs(
    texts: list[str],
    labels: list[str],
    num_iterations: int,
    seed: int,
) -> list[InputExample]:
    """Generate positive and negative pairs for contrastive learning.

    For each class, samples `num_iterations` positive pairs (same class)
    and `num_iterations` negative pairs (different class).
    """
    rng = random.Random(seed)

    # Group texts by label
    label_to_texts: dict[str, list[str]] = {}
    for text, label in zip(texts, labels):
        label_to_texts.setdefault(label, []).append(text)

    examples = []
    all_labels = sorted(label_to_texts.keys())

    for label in all_labels:
        class_texts = label_to_texts[label]
        other_texts = [t for l, ts in label_to_texts.items() if l != label for t in ts]

        # Positive pairs (label=1.0)
        if len(class_texts) >= 2:
            all_pos_pairs = list(combinations(class_texts, 2))
            rng.shuffle(all_pos_pairs)
            for t1, t2 in all_pos_pairs[:num_iterations]:
                examples.append(InputExample(texts=[t1, t2], label=1.0))
        else:
            # If only 1 example, pair with itself (rare edge case)
            examples.append(
                InputExample(texts=[class_texts[0], class_texts[0]], label=1.0)
            )

        # Negative pairs (label=0.0)
        for _ in range(num_iterations):
            t1 = rng.choice(class_texts)
            t2 = rng.choice(other_texts)
            examples.append(InputExample(texts=[t1, t2], label=0.0))

    rng.shuffle(examples)
    return examples


def main():
    if not LABELS_PATH.exists():
        print(f"ERROR: {LABELS_PATH} not found", file=sys.stderr)
        sys.exit(1)

    random.seed(RANDOM_SEED)
    np.random.seed(RANDOM_SEED)
    torch.manual_seed(RANDOM_SEED)

    # Load data
    print(f"Loading labels from {LABELS_PATH}...")
    records = load_human_labels(LABELS_PATH)
    print(f"  {len(records)} human-reviewed records loaded")

    # Extract primary label and content
    texts = [r["content"] for r in records]
    labels = [r["labels"][0] for r in records]

    # Filter to classes with >= MIN_EXAMPLES
    label_counts = Counter(labels)
    print(f"\nLabel distribution (all {len(label_counts)} classes):")
    for label, count in sorted(label_counts.items(), key=lambda x: -x[1]):
        status = "OK" if count >= MIN_EXAMPLES else "EXCLUDED"
        print(f"  {label:30s} {count:4d}  {status}")

    valid_labels = {l for l, c in label_counts.items() if c >= MIN_EXAMPLES}
    print(f"\nKeeping {len(valid_labels)} classes with >= {MIN_EXAMPLES} examples")

    filtered = [
        (t, l) for t, l in zip(texts, labels) if l in valid_labels
    ]
    texts = [t for t, _ in filtered]
    labels = [l for _, l in filtered]
    print(f"  {len(texts)} samples after filtering")

    # Stratified train/val split
    X_train, X_val, y_train, y_val = train_test_split(
        texts,
        labels,
        test_size=0.2,
        stratify=labels,
        random_state=RANDOM_SEED,
    )
    print(f"\nTrain: {len(X_train)} samples")
    print(f"Val:   {len(X_val)} samples")

    # Load base model (force CPU — nomic-bert at 2048 tokens exhausts MPS memory)
    print(f"\nLoading base model: {BASE_MODEL}")
    model = SentenceTransformer(BASE_MODEL, trust_remote_code=True, device="cpu")

    # Generate contrastive pairs from training set only
    print(f"\nGenerating contrastive pairs ({NUM_ITERATIONS} per class)...")
    train_examples = generate_contrastive_pairs(
        X_train, y_train, NUM_ITERATIONS, RANDOM_SEED
    )
    print(f"  {len(train_examples)} training pairs generated")

    # Fine-tune with contrastive loss
    print(f"\nFine-tuning sentence-transformer (epochs={NUM_EPOCHS}, batch={BATCH_SIZE})...")
    train_dataloader = DataLoader(train_examples, shuffle=True, batch_size=BATCH_SIZE)
    train_loss = losses.CosineSimilarityLoss(model)

    model.fit(
        train_objectives=[(train_dataloader, train_loss)],
        epochs=NUM_EPOCHS,
        warmup_steps=int(len(train_dataloader) * 0.1),
        show_progress_bar=True,
    )

    # Embed train and val sets with fine-tuned model
    print("\nEncoding train and val sets...")
    train_embeddings = model.encode(X_train, show_progress_bar=True, batch_size=32)
    val_embeddings = model.encode(X_val, show_progress_bar=True, batch_size=32)

    # Train logistic regression head
    print("\nTraining logistic regression head...")
    head = LogisticRegression(
        max_iter=1000,
        random_state=RANDOM_SEED,
        solver="lbfgs",
        C=1.0,
    )
    head.fit(train_embeddings, y_train)

    # Evaluate
    y_pred = head.predict(val_embeddings)

    print("\n" + "=" * 70)
    print("VALIDATION RESULTS")
    print("=" * 70)

    # Per-class metrics
    report = classification_report(y_val, y_pred, zero_division=0)
    print("\nClassification Report:")
    print(report)

    # Confusion matrix
    sorted_labels = sorted(set(y_train + y_val))
    cm = confusion_matrix(y_val, y_pred, labels=sorted_labels)
    print("Confusion Matrix:")
    # Header
    header = f"{'':30s} " + " ".join(f"{l[:6]:>6s}" for l in sorted_labels)
    print(header)
    for i, label in enumerate(sorted_labels):
        row = f"{label:30s} " + " ".join(f"{cm[i][j]:6d}" for j in range(len(sorted_labels)))
        print(row)

    # Overall accuracy
    accuracy = np.mean(np.array(y_pred) == np.array(y_val))
    print(f"\nOverall accuracy: {accuracy:.4f}")

    # Save model
    print(f"\nSaving model to {MODEL_DIR}...")
    MODEL_DIR.mkdir(parents=True, exist_ok=True)

    # Save sentence-transformer
    model.save(str(MODEL_DIR / "sentence-transformer"))

    # Save classification head
    import joblib

    joblib.dump(head, str(MODEL_DIR / "head.joblib"))

    # Save label mapping
    label_mapping = {str(i): label for i, label in enumerate(head.classes_)}
    with open(MODEL_DIR / "label_mapping.json", "w") as f:
        json.dump(label_mapping, f, indent=2)

    # Save training metadata
    metadata = {
        "base_model": BASE_MODEL,
        "num_iterations": NUM_ITERATIONS,
        "batch_size": BATCH_SIZE,
        "num_epochs": NUM_EPOCHS,
        "min_examples": MIN_EXAMPLES,
        "num_classes": len(sorted_labels),
        "classes": sorted_labels,
        "train_samples": len(X_train),
        "val_samples": len(X_val),
        "accuracy": float(accuracy),
    }
    with open(MODEL_DIR / "training_metadata.json", "w") as f:
        json.dump(metadata, f, indent=2)

    print(f"  Sentence-transformer saved to {MODEL_DIR / 'sentence-transformer'}")
    print(f"  Classification head saved to {MODEL_DIR / 'head.joblib'}")
    print(f"  Label mapping saved to {MODEL_DIR / 'label_mapping.json'}")
    print(f"  Training metadata saved to {MODEL_DIR / 'training_metadata.json'}")
    print("\nDone!")


if __name__ == "__main__":
    main()
