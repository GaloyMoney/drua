# No upstream version stream — drua workflows are triggered, not polled.
# Concourse calls `check` periodically; returning a static singleton
# version means the resource never accumulates new versions in the
# pipeline UI.
echo '[{"ref":"none"}]'
