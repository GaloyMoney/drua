PAYLOAD=$(cat)

URL=$(echo "$PAYLOAD" | jq -r '.source.url // empty')
WORKFLOW_ID=$(echo "$PAYLOAD" | jq -r '.source.workflow_id // empty')
PROVIDER=$(echo "$PAYLOAD" | jq -r '.source.provider // empty')
SECRET=$(echo "$PAYLOAD" | jq -r '.source.secret // empty')

if [ -z "$URL" ] || [ -z "$SECRET" ]; then
  echo "concourse-drua-resource: source.url and source.secret are required" >&2
  exit 1
fi

if [ -z "$WORKFLOW_ID" ] && [ -z "$PROVIDER" ]; then
  echo "concourse-drua-resource: one of source.workflow_id or source.provider is required" >&2
  exit 1
fi

ALERT_TYPE=$(echo "$PAYLOAD" | jq -r '.params.alert_type // "ci_failure"')
EXTRA=$(echo "$PAYLOAD" | jq -c '.params.extra // {}')

ATC_EXTERNAL_URL=${ATC_EXTERNAL_URL:-}
BUILD_TEAM_NAME=${BUILD_TEAM_NAME:-}
BUILD_PIPELINE_NAME=${BUILD_PIPELINE_NAME:-}
BUILD_JOB_NAME=${BUILD_JOB_NAME:-}
BUILD_NAME=${BUILD_NAME:-}

JOB_URL="${ATC_EXTERNAL_URL%/}/teams/${BUILD_TEAM_NAME}/pipelines/${BUILD_PIPELINE_NAME}/jobs/${BUILD_JOB_NAME}/builds/${BUILD_NAME}"
CORRELATION_ID="concourse-${BUILD_PIPELINE_NAME}-${BUILD_JOB_NAME}-${BUILD_NAME}"

BODY=$(jq -n \
  --arg alert_type "$ALERT_TYPE" \
  --arg team "$BUILD_TEAM_NAME" \
  --arg pipeline "$BUILD_PIPELINE_NAME" \
  --arg job "$BUILD_JOB_NAME" \
  --arg build "$BUILD_NAME" \
  --arg url "$JOB_URL" \
  --arg correlation_id "$CORRELATION_ID" \
  --argjson extra "$EXTRA" \
  '{
    source: "concourse",
    alert_type: $alert_type,
    team: $team,
    pipeline: $pipeline,
    job: $job,
    build: $build,
    build_url: $url,
    correlation_id: $correlation_id,
    extra: $extra
  }')

RESP_FILE=$(mktemp)
trap 'rm -f "$RESP_FILE"' EXIT

if [ -n "$PROVIDER" ]; then
  TARGET_URL="${URL%/}/webhooks/providers/${PROVIDER}"
else
  TARGET_URL="${URL%/}/webhooks/${WORKFLOW_ID}"
fi

HTTP_CODE=$(curl --silent --show-error \
  --output "$RESP_FILE" \
  --write-out '%{http_code}' \
  --request POST "$TARGET_URL" \
  --header "Authorization: Bearer ${SECRET}" \
  --header 'Content-Type: application/json' \
  --data "$BODY")

RUN_ID=$(cat "$RESP_FILE" 2>/dev/null || true)
if [ -z "$RUN_ID" ]; then
  RUN_ID="unknown"
fi

echo "concourse-drua-resource: POST ${TARGET_URL} -> ${HTTP_CODE} (run_id=${RUN_ID})" >&2

# Loud failure when the trigger itself is broken. Concourse marks the
# `put` step failed and the operator sees the diagnostic above.
case "$HTTP_CODE" in
  2*) ;;
  *) exit 1 ;;
esac

jq -n \
  --arg ref "$RUN_ID" \
  --arg code "$HTTP_CODE" \
  --arg url "$JOB_URL" \
  --arg correlation_id "$CORRELATION_ID" \
  '{
    version: { ref: $ref },
    metadata: [
      { name: "http_code", value: $code },
      { name: "run_id", value: $ref },
      { name: "build_url", value: $url },
      { name: "correlation_id", value: $correlation_id }
    ]
  }'
