# Stub: this resource has no fetchable artefact. A future v2 may poll
# `GET /webhooks/{def}/runs/{run}` and write the run state into the
# input dir; until then we drain stdin and return the static version.
cat > /dev/null
echo '{"version":{"ref":"none"}}'
