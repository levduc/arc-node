#!/usr/bin/env bash
# Ship the locally-built arc images to the 3 fleet remotes over tailscale, verifying by image ID.
#
#   ./ship-images.sh                 # ship arc_execution + arc_consensus if the remote ID differs
#   IMAGES=arc_execution:latest ./ship-images.sh
#
# Verification matters: alien2's Wi-Fi has truncated GB-scale pipes before (documented in
# CLAUDE.md), and a truncated `docker load` can leave a STALE image silently in place. So we
# compare the image ID after loading and fail loudly on mismatch rather than trusting the pipe.
set -uo pipefail
export PATH="$HOME/.foundry/bin:$PATH"
HOSTS=${HOSTS:-"ginnythui papaduck papaduck-alien2"}
IMAGES=${IMAGES:-"arc_execution:latest arc_consensus:latest"}
tss(){ local h=$1; shift;
  if [ -t 0 ]; then timeout -k 10 "${TSS_TMO:-1800}" tailscale ssh "$h" "$@" </dev/null;
  else timeout -k 10 "${TSS_TMO:-1800}" tailscale ssh "$h" "$@"; fi; }

rc=0
for img in $IMAGES; do
  local_id=$(docker images --format '{{.ID}}' "$img" | head -1)
  [ -n "$local_id" ] || { echo "!! no local image $img"; rc=1; continue; }
  echo "=== $img (local $local_id) ==="
  for h in $HOSTS; do
    remote_id=$(tss "$h" "docker images --format '{{.ID}}' $img 2>/dev/null | head -1" 2>/dev/null | tr -d ' \r')
    if [ "$remote_id" = "$local_id" ]; then echo "  $h: already current ($remote_id) — skip"; continue; fi
    echo "  $h: shipping (remote has '${remote_id:-none}')..."
    if docker save "$img" | gzip -1 | tss "$h" "gunzip | docker load" >/dev/null 2>&1; then
      new_id=$(tss "$h" "docker images --format '{{.ID}}' $img 2>/dev/null | head -1" 2>/dev/null | tr -d ' \r')
      if [ "$new_id" = "$local_id" ]; then echo "    OK ($new_id)"
      else echo "    !! MISMATCH after load: remote=$new_id local=$local_id (truncated pipe?)"; rc=1; fi
    else
      echo "    !! transfer failed to $h"; rc=1
    fi
  done
done
[ $rc -eq 0 ] && echo "✅ all images current on: $HOSTS" || echo "❌ some ships failed — re-run (it skips hosts already current)"
exit $rc
