STYLE_AGENT_CONFIG ?= ./config.toml
export STYLE_AGENT_CONFIG

bootstrap:
	cargo run -p style-agent -- bootstrap

start:
	cargo run -p style-agent -- label
	cargo run -p style-agent -- replay-labels
	cargo run -p style-agent -- up

backup-labels:
	@mkdir -p labels/snapshots
	@if [ -f labels/labels.jsonl ]; then \
		cp labels/labels.jsonl "labels/snapshots/labels-$$(date +%Y-%m-%d).jsonl"; \
		echo "Backed up labels.jsonl → labels/snapshots/labels-$$(date +%Y-%m-%d).jsonl"; \
	fi

review: backup-labels
	cargo run -p style-agent -- label
	cargo run -p style-agent -- replay-labels
	cargo run -p style-agent -- review

telegram: backup-labels
	cargo run -p style-agent -- label
	cargo run -p style-agent -- replay-labels
	cargo run -p style-agent -- telegram-review

eval:
	cargo run -p style-agent -- eval

eval-ml:
	cargo run -p style-agent -- eval-ml

audit-labels:
	cargo run -p style-agent -- audit-labels

review-confused: backup-labels
	cargo run -p style-agent -- review --confused

telegram-confused: backup-labels
	cargo run -p style-agent -- telegram-review --confused

# --- Training targets (run inside nix develop .#training) ---

TRAIN_SHELL := nix develop '.\#training' -c

umap:
	$(TRAIN_SHELL) python3 scripts/umap_validation.py

train:
	$(TRAIN_SHELL) python3 scripts/train_setfit.py

export-onnx:
	$(TRAIN_SHELL) python3 scripts/export_onnx.py

# Full training pipeline: train → export
train-all: train export-onnx

# --- Label stats ---

MIN_PER_LABEL ?= 50
ALL_LABELS := entity entity_command entity_query entity_hydration entity_event \
              published_event new_entity service_method service repository error \
              authorization value_object domain_primitives api job event_handler \
              type_conversion test config
label-stats:
	@echo "Human-reviewed: $$(wc -l < labels/labels.jsonl | tr -d ' ')"
	@echo ""
	@printf "%-25s %5s / %-5s %s\n" "Label" "Count" "$(MIN_PER_LABEL)" "Status"
	@echo "--------------------------------------------------"
	@for label in $(ALL_LABELS); do \
		n=$$(jq -r '.labels[0] // empty' labels/labels.jsonl | grep -cx "$$label"); \
		need=$$(($(MIN_PER_LABEL) - n)); \
		if [ $$need -le 0 ]; then status="OK"; else status="NEED $$need"; fi; \
		printf "%-25s %5d / %-5d %s\n" "$$label" "$$n" "$(MIN_PER_LABEL)" "$$status"; \
	done
	@echo ""
	@done=0; total=0; for label in $(ALL_LABELS); do \
		n=$$(jq -r '.labels[0] // empty' labels/labels.jsonl | grep -cx "$$label"); \
		total=$$((total + 1)); \
		if [ $$n -ge $(MIN_PER_LABEL) ]; then done=$$((done + 1)); fi; \
	done; echo "$$done/$$total labels at target"
