#!/usr/bin/env bash
# TRULY distributed payment-lane spam: run S spammers ON EACH fleet machine, each hitting its OWN
# local payment EL, with globally-DISJOINT account ranges (via --account-offset, no nonce clashes).
#
# Why this and not the others:
#   spam-fleet.sh          -> all spam to val1's EL, relies on gossip (one EL ingests everything)
#   spam-fleet-fanout.sh   -> one box feeds all ELs over the network (ack-gated by the slowest link)
#   THIS                    -> each EL is fed by a spammer on its OWN box (no network in the send path,
#                             ingestion spread across all 4 ELs) -> the way to actually push 20-50k tx/s
#
# Usage (run from ginny-alienware, tailscale authenticated):
#   ./spam-fleet-distributed.sh ship      # copy the spammer binary to each remote (once per build)
#   ./spam-fleet-distributed.sh start      # launch S spammers per machine
#   ./spam-fleet-distributed.sh status
#   ./spam-fleet-distributed.sh stop
#
# env: S (spammers/machine, def 4), ACCTS (accounts/spammer, def 1000), RATE (per-spammer tx/s, def
#      12000), DUR (seconds, def 300), CID (payment chainId, def 1338)
# Requires: fleet up with the payment lane, and >= 4*S*ACCTS prefunded accounts, i.e. start the fleet
#      with EXTRA_ACCOUNTS >= 4*S*ACCTS (e.g. PAY_GAS=1000000000 EXTRA_ACCOUNTS=16000 demo-fleet-metamask.sh start).
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"; cd "$REPO"
SPAMMER="$REPO/target/release/spammer"
RBIN=/tmp/arc-spammer                       # where the binary lands on each remote
declare -A RHOST=( [1]="" [2]=ginnythui [3]=papaduck [4]=papaduck-alien2 )
S=${S:-4}; ACCTS=${ACCTS:-1000}; RATE=${RATE:-12000}; DUR=${DUR:-300}; CID=${CID:-1338}
# `tailscale ssh` can wedge on an interactive auth/TTY check and IGNORE SIGTERM, so a plain
# `timeout N` hangs forever. -k forces SIGKILL; </dev/null removes the TTY wait (the real wedge).
tss(){ local h=$1; shift;
  # -k forces SIGKILL after the SIGTERM tailscale-ssh ignores. Close stdin ONLY when it is a TTY
  # (the wedge); callers that PIPE data in (tar | tss ...) must keep their stdin.
  if [ -t 0 ]; then timeout -k 10 "${TSS_TMO:-90}" tailscale ssh "$h" "$@" </dev/null;
  else timeout -k 10 "${TSS_TMO:-90}" tailscale ssh "$h" "$@"; fi; }
localws(){ echo $((19546+($1-1)*100)); }    # payment EL ws port published on the box hosting val_n

need(){ [ -x "$SPAMMER" ] || { echo "!! build the spammer first: cargo build --release -p spammer"; exit 1; }; }

ship(){
  need
  local sha; sha=$(sha256sum "$SPAMMER" | awk '{print $1}')
  echo "local spammer sha256=$sha ($(du -h "$SPAMMER" | cut -f1))"
  for n in 2 3 4; do
    local h=${RHOST[$n]}
    # already there with the SAME binary? skip (idempotent — handles a prior ship).
    local rsha; rsha=$(tss "$h" "sha256sum $RBIN 2>/dev/null | awk '{print \$1}'" 2>/dev/null | tr -d '\r')
    if [ "$rsha" = "$sha" ]; then echo "==> $h: already up to date, skip"; continue; fi
    echo "==> $h: shipping (base64 over ssh)$([ -n "$rsha" ] && echo ' [replacing older build]')..."
    if base64 "$SPAMMER" | tss "$h" "base64 -d > $RBIN && chmod +x $RBIN"; then
      rsha=$(tss "$h" "sha256sum $RBIN | awk '{print \$1}'" 2>/dev/null | tr -d '\r')
      if [ "$rsha" = "$sha" ]; then echo "    OK ($h)"; else echo "    !! sha MISMATCH on $h (got $rsha) — link truncated (wifi?). Re-run 'ship' or use taildrop."; fi
    else
      echo "    !! ship failed to $h"
    fi
  done
}

start(){
  need
  echo "==> [1/2] ensuring the spammer binary is on every remote (idempotent)"
  ship            # auto-ship: skips hosts that already have the matching binary
  echo "==> [2/2] launching distributed spam"
  local total=$((4*S*ACCTS))
  echo "    4 machines x $S spammers ($ACCTS accts each) = $((4*S)) spammers, $total accounts needed"
  for n in 1 2 3 4; do
    local ws; ws=$(localws $n)
    for j in $(seq 0 $((S-1))); do
      local gidx=$(( (n-1)*S + j )) off; off=$(( ((n-1)*S + j) * ACCTS ))
      # -l resyncs each account to its latest on-chain nonce, so re-runs on a live chain don't
      # nonce-gap (without it, a second run starts from nonce 0 -> all "nonce too low").
      # FF=1 -> --fire-and-forget: send without awaiting each WS response (backpressure mode's
      # per-tx round-trip is the measured delivery ceiling ~5.3-5.8k tx/s; capacity is ~12k).
      # MEASURED DEAD for sustained load (no nonce repair -> queue collapse); kept for repro only.
      local ffarg=""; [ "${FF:-0}" = "1" ] && ffarg="--fire-and-forget"
      # POOL_TARGET>0 -> closed-loop governor: pause while local pool pending+queued exceeds it.
      # Open-loop RATE>=1500/spammer collapses the pool (iter 17b); with the governor you can set
      # RATE high and let pool depth pace delivery. ~2-3 full blocks' worth is the right target.
      local ptarg=""; [ "${POOL_TARGET:-0}" != "0" ] && ptarg="--pool-target $POOL_TARGET"
      local run="ws --targets ws://127.0.0.1:$ws --chain-id $CID -r $RATE -t $DUR -g 20 -a $ACCTS --account-offset $off -l --mix transfer=100 $ffarg $ptarg"
      if [ "$n" -eq 1 ]; then
        nohup "$SPAMMER" $run >/tmp/spam-d-1-$j.log 2>&1 & disown
      else
        tss "${RHOST[$n]}" "nohup $RBIN $run >/tmp/spam-d-$n-$j.log 2>&1 & echo ok" >/dev/null 2>&1 \
          || echo "  !! failed to launch on ${RHOST[$n]} (binary shipped? tailscale up?)"
      fi
    done
    echo "  val$n -> local EL :$ws  (account offsets $(( (n-1)*S*ACCTS ))..$(( ((n-1)*S+S)*ACCTS - 1 )))"
  done
  echo "started. measure with:  experiments/dual-el/pay-throughput-bench.sh measure"
}

status(){
  echo "val1 (local): $(pgrep -xc spammer 2>/dev/null || echo 0) spammers"
  for n in 2 3 4; do
    c=$(tss "${RHOST[$n]}" "pgrep -fc '$RBIN' 2>/dev/null || echo 0" 2>/dev/null | tr -d '\r')
    echo "val$n (${RHOST[$n]}): ${c:-?} spammers"
  done
}

stop(){
  pkill -x spammer 2>/dev/null || true
  for n in 2 3 4; do tss "${RHOST[$n]}" "pkill -f '$RBIN' 2>/dev/null; echo stopped" >/dev/null 2>&1 || true; done
  echo "distributed spam stopped (all machines)"
}

case "${1:-}" in
  ship) ship ;;
  start) start ;;
  status) status ;;
  stop) stop ;;
  *) echo "usage: $0 {ship|start|status|stop}   env: S ACCTS RATE DUR CID"; exit 1 ;;
esac
