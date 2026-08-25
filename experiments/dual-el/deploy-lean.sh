#!/usr/bin/env bash
# deploy-lean.sh — put everything a lean-lane fleet run needs on every machine.
#
#   ./deploy-lean.sh          # build locally, ship to the hosts in fleet.env
#   ./deploy-lean.sh --check  # verify only, ship nothing
#
# Ships: the lean node binary, the spammer, lean-feeder.py, and the genesis fund
# file — each sha-verified after transfer, because tailscale ssh exit codes lie
# when its session check has expired (a silent no-op cost a whole campaign
# night). Idempotent: matching sha = skipped.
set -uo pipefail
cd "$(cd "$(dirname "$0")/../.." && pwd)" || exit 1
CFG=experiments/dual-el/fleet.env
[ -f "$CFG" ] || { echo "missing $CFG — copy fleet.env.example and edit"; exit 1; }
# shellcheck disable=SC1090
source "$CFG"
CHECK_ONLY=${1:-}
say(){ echo "[$(date +%H:%M:%S)] deploy: $*"; }

# ------------------------------------------------------------ build
if [ "$CHECK_ONLY" != "--check" ]; then
  say "building lean-lane-node + spammer (release)"
  cargo build --release -p lean-lane-node -p spammer >/dev/null 2>&1 \
    || { echo "FAIL: cargo build"; exit 1; }
fi
for b in "$LEAN_BIN" "$SPAMMER_BIN"; do
  [ -x "$b" ] || { echo "FAIL: $b missing (cargo build --release -p lean-lane-node -p spammer)"; exit 1; }
done
if [ ! -s "$FUND" ]; then
  say "generating fund file ($FUND_ACCOUNTS accounts)"
  bash experiments/dual-el/gen-lean-fund.sh "$FUND_ACCOUNTS" "$FUND" >/dev/null || exit 1
fi
say "local: $(basename "$LEAN_BIN") $(sha256sum "$LEAN_BIN" | cut -c1-12) · fund $(wc -l < "$FUND") accts"

# ------------------------------------------------------------ ship
ship(){ # ship <host> <local-file> <remote-path>
  local h=$1 src=$2 dst=$3
  local want; want=$(sha256sum "$src" | cut -c1-16)
  local have; have=$(timeout 60 tailscale ssh "$REMOTE_USER@$h" "sha256sum $dst 2>/dev/null | cut -c1-16" 2>/dev/null | tail -1 | tr -dc '0-9a-f')
  if [ "$have" = "$want" ]; then echo "    $(basename "$dst"): current"; return 0; fi
  [ "$CHECK_ONLY" = "--check" ] && { echo "    $(basename "$dst"): STALE (have ${have:-none})"; return 1; }
  gzip -c "$src" | timeout 600 tailscale ssh "$REMOTE_USER@$h" \
    "cat > $dst.gz && gunzip -f $dst.gz && chmod +x $dst 2>/dev/null; true" 2>/dev/null
  have=$(timeout 60 tailscale ssh "$REMOTE_USER@$h" "sha256sum $dst 2>/dev/null | cut -c1-16" 2>/dev/null | tail -1 | tr -dc '0-9a-f')
  # VERIFY THE EFFECT, not the exit code
  [ "$have" = "$want" ] && echo "    $(basename "$dst"): shipped ✓" || { echo "    $(basename "$dst"): FAILED (have ${have:-none}, want $want)"; return 1; }
}

rc=0
for i in "${!FLEET_HOSTS[@]}"; do
  h=${FLEET_HOSTS[$i]}
  [ "$h" = localhost ] && continue
  say "$h"
  timeout 45 tailscale ssh "$REMOTE_USER@$h" "mkdir -p $LEAN_DATA_ROOT" >/dev/null 2>&1 \
    || { echo "    UNREACHABLE (tailscale ssh check expired? run: tailscale ssh $REMOTE_USER@$h true)"; rc=1; continue; }
  ship "$h" "$LEAN_BIN"    "$LEAN_BIN_REMOTE"                  || rc=1
  ship "$h" "$SPAMMER_BIN" "$REMOTE_HOME/arc-spammer"          || rc=1
  ship "$h" experiments/dual-el/lean-feeder.py "$REMOTE_HOME/lean-feeder.py" || rc=1
  ship "$h" "$FUND"        "$LEAN_FUND_REMOTE"                 || rc=1
done
[ $rc = 0 ] && say "all machines current" || say "one or more machines incomplete (rc=$rc)"
exit $rc
