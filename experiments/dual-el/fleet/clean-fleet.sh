#!/usr/bin/env bash
# Deep-clean the fleet across ALL machines. Force-removes every validator* container and every
# testnet datadir (local .quake/* and each remote's arc-fleet on home + NVMe), then prunes buildx
# cache. Reports disk freed per machine.
#
# Why this exists: start/stop track a specific scenario/host set, so ORPHANED leftovers (a run whose
# val1 vanished, a datadir from a different scenario) survive them and quietly eat 50-200 GB/machine.
# `clean` is the explicit "wipe everything fleet-related, everywhere" hammer. It does NOT touch code,
# git, docker images, or the monitoring config -- only regenerable chain datadirs + stopped containers.
#
#   ./clean-fleet.sh            # clean local + all 3 remotes
# Safe to run anytime nothing should be running; a fresh `demo-fleet-*.sh start` recreates everything.
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
declare -A RHOST=( [2]=ginnythui [3]=papaduck [4]=papaduck-alien2 )
# `tailscale ssh` can wedge on an interactive auth/TTY check and IGNORE SIGTERM -- a plain
# `timeout N` then hangs forever (observed: a 20s timeout still alive after 479s). So:
#   -k 10  -> SIGKILL 10s after the SIGTERM it ignores
#   </dev/null -> never wait on stdin/a TTY (the actual wedge)
tss(){ local h=$1; shift; timeout -k 10 "${TSS_TMO:-200}" tailscale ssh "$h" "$@" </dev/null; }
# If this script is interrupted (Ctrl-C) or killed, take any wedged ssh children with it --
# otherwise they linger and the next run inherits a stuck session.
cleanup(){ pkill -9 -P $$ -f "tailscale ssh" 2>/dev/null; }
trap 'cleanup; echo; echo "interrupted — remaining hosts not cleaned"; exit 130' INT TERM

# remote one-liner: rm validator containers, both arc-fleet locations (root-owned -> root container),
# prune buildx, print disk.
REMOTE_CLEAN='docker ps -aq --filter name=validator | xargs -r docker rm -f >/dev/null 2>&1; \
docker run --rm -v /home/papaduck:/h --user root alpine rm -rf /h/arc-fleet >/dev/null 2>&1; \
docker run --rm -v /mnt/blockchain.ssd:/s --user root alpine rm -rf /s/arc-fleet >/dev/null 2>&1; \
docker buildx prune -af >/dev/null 2>&1; \
df -h / | awk "NR==2{print \$3\" used, \"\$4\" free (\"\$5\")\"}"'

echo "==> LOCAL ($(hostname 2>/dev/null || echo this-box))"
docker ps -aq --filter name=validator | xargs -r docker rm -f >/dev/null 2>&1
# remove every .quake datadir scenario EXCEPT the monitoring config
docker run --rm -v "$REPO/.quake":/q --user root alpine sh -c 'cd /q 2>/dev/null && for d in *; do [ "$d" = monitoring ] && continue; rm -rf "$d"; done' >/dev/null 2>&1 || true
docker buildx prune -af >/dev/null 2>&1
echo "   validators left: $(docker ps -aq --filter name=validator | wc -l | tr -d ' ')   disk: $(df -h / | awk 'NR==2{print $3" used, "$4" free ("$5")"}')"

for n in 2 3 4; do
  h=${RHOST[$n]}
  echo "==> $h"
  if timeout -k 5 20 tailscale ssh "$h" "echo ok" </dev/null >/dev/null 2>&1; then
    out=$(tss "$h" "$REMOTE_CLEAN" 2>/dev/null | tr -d '\r' | tail -1)
    left=$(timeout -k 5 20 tailscale ssh "$h" "docker ps -aq --filter name=validator | wc -l" </dev/null 2>/dev/null | tr -d ' \r')
    echo "   validators left: ${left:-?}   disk: ${out:-?}"
  else
    echo "   ⚠ unreachable via tailscale — skipped (reauth: tailscale up)"
  fi
done
echo "==> clean complete."
