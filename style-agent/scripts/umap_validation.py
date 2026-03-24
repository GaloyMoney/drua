#!/usr/bin/env python3
"""Phase 0: UMAP validation of label clusters.

Loads human-reviewed labels from labels/labels.jsonl, embeds each chunk
via Ollama (nomic-embed-text), applies UMAP for 2D projection, and
plots color-coded by primary label.

Validates whether the embedding space naturally separates label classes.

Usage:
    nix develop .#training -c python3 scripts/umap_validation.py
"""

import json
import sys
from collections import Counter
from pathlib import Path

import matplotlib.pyplot as plt
import numpy as np
import requests
import umap
from sklearn.metrics import silhouette_score

LABELS_PATH = Path("labels/labels.jsonl")
OUTPUT_PATH = Path("data/umap_labels.png")
OLLAMA_URL = "http://localhost:11434/api/embeddings"
EMBED_MODEL = "nomic-embed-text"
EMBED_PREFIX = "search_document: "


def load_human_labels(path: Path) -> list[dict]:
    """Load records from JSONL, keeping only human-reviewed entries."""
    records = []
    seen_keys = {}

    with open(path) as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            record = json.loads(line)
            # Latest-write-wins dedup (same as Rust LabelStore)
            key = (
                record["key"]["repo"],
                record["key"]["file_path"],
                record["key"]["chunk_type"],
                record["key"]["entity_name"],
            )
            seen_keys[key] = record

    for record in seen_keys.values():
        if record.get("label_source") == "human" and record.get("labels"):
            records.append(record)

    return records


def embed_text(text: str) -> list[float]:
    """Get embedding from Ollama for a single text."""
    resp = requests.post(
        OLLAMA_URL,
        json={"model": EMBED_MODEL, "prompt": EMBED_PREFIX + text},
        timeout=60,
    )
    resp.raise_for_status()
    return resp.json()["embedding"]


def main():
    if not LABELS_PATH.exists():
        print(f"ERROR: {LABELS_PATH} not found", file=sys.stderr)
        sys.exit(1)

    print(f"Loading labels from {LABELS_PATH}...")
    records = load_human_labels(LABELS_PATH)
    print(f"  {len(records)} human-reviewed records loaded")

    # Extract primary labels
    labels = [r["labels"][0] for r in records]
    label_counts = Counter(labels)
    print("\nLabel distribution:")
    for label, count in sorted(label_counts.items(), key=lambda x: -x[1]):
        print(f"  {label:30s} {count:4d}")

    # Embed all chunks
    print(f"\nEmbedding {len(records)} chunks via Ollama ({EMBED_MODEL})...")
    embeddings = []
    for i, record in enumerate(records):
        if (i + 1) % 50 == 0 or i == 0:
            print(f"  [{i+1}/{len(records)}]")
        emb = embed_text(record["content"])
        embeddings.append(emb)
    embeddings = np.array(embeddings)
    print(f"  Embedding matrix shape: {embeddings.shape}")

    # UMAP projection
    print("\nRunning UMAP (2D)...")
    reducer = umap.UMAP(n_components=2, random_state=42, metric="cosine")
    projection = reducer.fit_transform(embeddings)

    # Silhouette score
    unique_labels = list(set(labels))
    label_ids = [unique_labels.index(l) for l in labels]
    sil_score = silhouette_score(projection, label_ids, metric="euclidean")
    print(f"\nSilhouette score (2D UMAP): {sil_score:.4f}")
    print("  (>0.3 = decent clusters, >0.5 = good, >0.7 = excellent)")

    # Also compute silhouette in full embedding space
    sil_full = silhouette_score(embeddings, label_ids, metric="cosine")
    print(f"Silhouette score (full embed space, cosine): {sil_full:.4f}")

    # Plot
    print(f"\nPlotting to {OUTPUT_PATH}...")
    fig, ax = plt.subplots(figsize=(16, 12))

    # Assign colors
    cmap = plt.cm.get_cmap("tab20", len(unique_labels))
    for idx, label in enumerate(sorted(unique_labels)):
        mask = [l == label for l in labels]
        pts = projection[mask]
        ax.scatter(
            pts[:, 0],
            pts[:, 1],
            c=[cmap(idx)],
            label=f"{label} ({label_counts[label]})",
            alpha=0.7,
            s=30,
        )

    ax.set_title(
        f"UMAP projection of code chunks by label\n"
        f"({len(records)} chunks, {len(unique_labels)} labels, "
        f"silhouette={sil_score:.3f})"
    )
    ax.legend(
        bbox_to_anchor=(1.05, 1),
        loc="upper left",
        fontsize=8,
        markerscale=1.5,
    )
    plt.tight_layout()

    OUTPUT_PATH.parent.mkdir(parents=True, exist_ok=True)
    fig.savefig(OUTPUT_PATH, dpi=150, bbox_inches="tight")
    print(f"  Saved to {OUTPUT_PATH}")
    print("\nDone!")


if __name__ == "__main__":
    main()
