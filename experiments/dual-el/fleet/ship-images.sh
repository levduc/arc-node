#!/usr/bin/env bash
# Ship the locally-built arc images to the 3 fleet remotes over tailscale, verifying by image ID.
#
#   ./ship-images.sh                 # ship arc_execution + arc_consensus if the remote ID differs
#   IMAGES=arc_execution:latest ./ship-images.sh
#
# Verification matters: alien2's Wi-Fi has truncated GB-scale pipes before (documented in
# CLAUDE.md), and a truncated `docker load` can leave a STALE image silently in place.
# We verify by CONTENT (sha256 of the binary inside the image), not by image ID: docker
# recomputes image IDs across daemon versions/storage drivers, so IDs differ between hosts even
# when the image is byte-identical (observed 2026-08-08: two hosts reported the same 'wrong' ID
# while the binary hashed identically).
set -uo pipefail
export PATH="$HOME/.foundry/bin:$PATH"
HOSTS=${HOSTS:-"ginnythui papaduck papaduck-alien2"}
IMAGES=${IMAGES:-"arc_execution:latest arc_consensus:latest"}
tss(){ local h=$1; shift;
  if [ -t 0 ]; then timeout -k 10 "${TSS_TMO:-1800}" tailscale ssh "$h" "$@" </dev/null;
  else timeout -k 10 "${TSS_TMO:-1800}" tailscale ssh "$h" "$@"; fi; }

rc=0
for img in $IMAGES; do
  case "$img" in arc_execution*) probe=/usr/local/bin/arc-node-execution;; *) probe=/usr/local/bin/arc-node-consensus;; esac
  local_sum=$(docker run --rm --entrypoint sha256sum "$img" $probe 2>/dev/null | awk '{print $1}')
  [ -n "$local_sum" ] || { echo "!! cannot hash $probe in local $img"; rc=1; continue; }
  echo "=== $img (binary sha ${local_sum:0:16}) ==="
  for h in $HOSTS; do
    remote_sum=$(tss "$h" "docker run --rm --entrypoint sha256sum $img $probe 2>/dev/null | awk '{print \$1}'" 2>/dev/null | tr -d ' \r')
    if [ -n "$local_sum" ] && [ "$remote_sum" = "$local_sum" ]; then echo "  $h: already current — skip"; continue; fi
    echo "  $h: shipping..."
    if docker save "$img" | gzip -1 | tss "$h" "gunzip | docker load" >/dev/null 2>&1; then
      new_sum=$(tss "$h" "docker run --rm --entrypoint sha256sum $img $probe 2>/dev/null | awk '{print \$1}'" 2>/dev/null | tr -d ' \r')
      if [ "$new_sum" = "$local_sum" ]; then echo "    OK (binary sha matches)"
      else echo "    !! CONTENT MISMATCH: remote=${new_sum:0:16} local=${local_sum:0:16} (truncated pipe?)"; rc=1; fi
    else
      echo "    !! transfer failed to $h"; rc=1
    fi
  done
done
[ $rc -eq 0 ] && echo "✅ all images current on: $HOSTS" || echo "❌ some ships failed — re-run (it skips hosts already current)"
exit $rc
