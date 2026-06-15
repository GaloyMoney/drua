#!/usr/bin/env bats

REPO_ROOT="$(cd "$(dirname "$BATS_TEST_FILENAME")/.." && pwd)"
PROD_VALUES="$REPO_ROOT/ci/deploy/drua/prod-values.yml.tmpl"

github_actions_block() {
  sed -n '/^      - name: github_actions$/,/^      - name: github_pull_requests$/p' "$PROD_VALUES"
}

@test "prod github_actions upstream uses Actions endpoint" {
  block="$(github_actions_block)"

  grep -qx "        url: https://api.githubcopilot.com/mcp/x/actions" <<< "$block"
  [[ "$block" == *"toolPrefix: github_actions"* ]]
  [[ "$block" == *"authMode: github_app"* ]]
  [[ "$block" == *"category: ci"* ]]
}

@test "prod github_actions upstream exposes expected Actions tools" {
  block="$(github_actions_block)"

  [[ "$block" == *"- actions_list"* ]]
  [[ "$block" == *"- actions_get"* ]]
  [[ "$block" == *"- get_job_logs"* ]]
  [[ "$block" == *"- actions_run_trigger"* ]]
}
