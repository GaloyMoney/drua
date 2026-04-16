#!/usr/bin/env bash
set -euo pipefail

REPO_DIR="${1:-dev/library-test-repo}"

if [ -d "$REPO_DIR" ]; then
  echo "Repo already exists at $REPO_DIR — remove it first to recreate"
  exit 0
fi

mkdir -p "$REPO_DIR"
cd "$REPO_DIR"

git init
git checkout -b main

# Initial commit
git commit --allow-empty -m "initial commit"

# A few realistic commits on main
cat > README.md <<'DOC'
# Test Library

A test knowledge-base repo for library sync development.
DOC
git add README.md
git commit -m "docs: add README"

mkdir -p guides
cat > guides/getting-started.md <<'DOC'
# Getting Started

This is a guide for getting started.
DOC
git add guides/
git commit -m "docs: add getting-started guide"

cat > guides/architecture.md <<'DOC'
# Architecture

Overview of the system architecture.
DOC
git add guides/
git commit -m "docs: add architecture guide"

# A feature branch with its own commits
git checkout -b feature/add-faq
cat > guides/faq.md <<'DOC'
# FAQ

Frequently asked questions.
DOC
git add guides/
git commit -m "docs: add FAQ"

# Back to main with another commit
git checkout main
cat >> README.md <<'DOC'

## Contributing

PRs welcome.
DOC
git add README.md
git commit -m "docs: add contributing section"

echo ""
echo "Test repo created at $REPO_DIR"
echo "  Branches: main, feature/add-faq"
echo "  Commits:  $(git rev-list --all --count) total"
