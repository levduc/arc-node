#!/usr/bin/env bash
# lean-smoke.sh [heights] — prove this repo can run the lean payment lane ALONE.
#
# No docker, no fleet, no reth fork, no tailscale: builds nothing but the two
# crates in this workspace, boots three lean nodes on loopback, and drives them
# with a minimal stand-in for the CL (round-robin proposer: arc_buildBlock on the
# proposer, arc_newBlock on everyone, then check all three agree). Also feeds
# real signed fan-out transactions so the blocks are not empty.
#
#   ./lean-smoke.sh          # 40 heights, ~1 min
#   ./lean-smoke.sh 200      # longer
#
# Exit 0 = the lane works from a clean checkout on this machine.
set -uo pipefail
cd "$(cd "$(dirname "$0")/../.." && pwd)" || exit 1
HEIGHTS=${1:-40}
PORTS=(8571 8572 8573)
RUN=$(mktemp -d /tmp/lean-smoke.XXXXXX)
FUND=${FUND:-$RUN/fund.txt}
BIN=target/release/lean-lane-node
SPAM=target/release/spammer
CHAIN=1338
BUDGET=${BUDGET:-50000000}
N=${N:-10}
say(){ echo "[$(date +%H:%M:%S)] smoke: $*"; }
cleanup(){ for p in "${PORTS[@]}"; do
    pid=$(ss -ltnp 2>/dev/null | grep ":$p " | grep -oP 'pid=\K[0-9]+' | head -1)
    [ -n "${pid:-}" ] && kill -9 "$pid" 2>/dev/null
  done; rm -rf "$RUN"; }
trap cleanup EXIT
rpc(){ # rpc <port> <method> <json-params>
  # body goes through a FILE, not argv: a budget-full block's base64 is ~1 MB and
  # blows the exec argument limit when passed inline.
  local body="$RUN/.req"
  printf '{"jsonrpc":"2.0","id":1,"method":"%s","params":%s}' "$2" "$3" > "$body"
  curl -s -m 30 -X POST "http://127.0.0.1:$1" -H 'content-type: application/json' --data-binary "@$body"; }

rpc_file(){ # rpc_file <port> <method> <params-file>
  local body="$RUN/.reqf"
  { printf '{"jsonrpc":"2.0","id":1,"method":"%s","params":' "$2"; cat "$3"; printf '}'; } > "$body"
  curl -s -m 60 -X POST "http://127.0.0.1:$1" -H 'content-type: application/json' --data-binary "@$body"; }

# ---------------------------------------------------------------- build
for t in "$BIN" "$SPAM"; do
  [ -x "$t" ] || { say "building $(basename $t) (first run is slow)"; cargo build --release -p "$(basename $t)" >/dev/null 2>&1 || cargo build --release -p lean-lane-node >/dev/null 2>&1; }
done
[ -x "$BIN" ] || { echo "FAIL: cargo build --release -p lean-lane-node"; exit 1; }
[ -x "$SPAM" ] || { echo "FAIL: cargo build --release -p spammer"; exit 1; }

# ------------------------------------------------------------ fund file
if [ ! -s "$FUND" ]; then
  say "generating 200-account fund file"
  bash experiments/dual-el/gen-lean-fund.sh 200 "$FUND" >/dev/null 2>&1 \
    || { echo "FAIL: fund generation (is foundry/cast installed?)"; exit 1; }
fi

# --------------------------------------------------------------- boot
peers_for(){ local self=$1 out=""; for p in "${PORTS[@]}"; do
    [ "$p" = "$self" ] || out="${out}http://127.0.0.1:$p,"; done; echo "${out%,}"; }
for p in "${PORTS[@]}"; do
  setsid "$BIN" run --datadir "$RUN/n$p" --port "$p" --bind 127.0.0.1 --chain-id $CHAIN \
    --shim --peers "$(peers_for "$p")" --fund-file "$FUND" \
    --fund-balance 10000000000000000000 > "$RUN/n$p.log" 2>&1 &
  disown
done
for p in "${PORTS[@]}"; do
  ok=0; for _ in $(seq 1 30); do
    rpc "$p" arc_getHead '{}' | grep -q commitment && { ok=1; break; }; sleep 1; done
  [ $ok = 1 ] || { echo "FAIL: node on :$p did not start"; tail -5 "$RUN/n$p.log"; exit 1; }
done
say "3 nodes up on ${PORTS[*]}"

# genesis must be identical everywhere (it is a pure function of chain id)
g=$(rpc "${PORTS[0]}" arc_getHead '{}' | python3 -c 'import sys,json;print(json.load(sys.stdin)["result"]["commitment"])')
for p in "${PORTS[@]}"; do
  gg=$(rpc "$p" arc_getHead '{}' | python3 -c 'import sys,json;print(json.load(sys.stdin)["result"]["commitment"])')
  [ "$gg" = "$g" ] || { echo "FAIL: genesis mismatch on :$p"; exit 1; }
done
say "genesis agreed: ${g:0:18}..."

# --------------------------------------------------------------- load
say "feeding fan-out transactions (N=$N)"
"$SPAM" ws --targets "ws://127.0.0.1:${PORTS[0]}" -r 2000 -g 2 -a 200 --account-offset 0 \
  -t 12 --chain-id $CHAIN --mix fanout=100 --fanout-outputs $N -l >/dev/null 2>&1
pend=$(rpc "${PORTS[0]}" txpool_status '{}' | python3 -c 'import sys,json;print(json.load(sys.stdin)["result"]["pending"])')
[ "${pend:-0}" -gt 0 ] || { echo "FAIL: no transactions reached the pool"; exit 1; }
say "pool: $pend pending"

# ------------------------------------------- drive heights (stand-in CL)
say "driving $HEIGHTS heights (round-robin proposer)"
txs=0; outs=0
for h in $(seq 1 "$HEIGHTS"); do
  prop=${PORTS[$(( (h-1) % ${#PORTS[@]} ))]}
  head=$(rpc "$prop" arc_getHead '{}')
  pc=$(echo "$head" | python3 -c 'import sys,json;r=json.load(sys.stdin)["result"];print(r["commitment"])')
  pn=$(echo "$head" | python3 -c 'import sys,json;r=json.load(sys.stdin)["result"];print(r["number"])')
  ts=$(( $(date +%s) * 1000 ))
  built=$(rpc "$prop" arc_buildBlock "{\"parentCommitment\":\"$pc\",\"number\":$((pn+1)),\"timestampMs\":$ts,\"budgetGas\":$BUDGET}")
  bytes=$(echo "$built" | python3 -c 'import sys,json;print(json.load(sys.stdin)["result"]["blockBytes"])' 2>/dev/null)
  [ -n "${bytes:-}" ] || { echo "FAIL: buildBlock at height $h"; echo "$built" | head -c 200; exit 1; }
  printf '{"blockBytes":"%s"}' "$bytes" > "$RUN/.blk"
  for p in "${PORTS[@]}"; do
    r=$(rpc_file "$p" arc_newBlock "$RUN/.blk")
    echo "$r" | grep -qE '"commitment"|SYNCING' || { echo "FAIL: newBlock rejected on :$p at height $h"; echo "$r" | head -c 200; exit 1; }
  done
  stat=$(python3 - "$RUN/.blk" <<'PY'
import base64,sys
import json
bb=base64.b64decode(json.load(open(sys.argv[1]))['blockBytes']); ntx=int.from_bytes(bb[48:52],'little'); off=52; o=0
for _ in range(ntx):
    l=int.from_bytes(bb[off:off+4],'little'); off+=4
    o+=int.from_bytes(bb[off+5:off+7],'little'); off+=l
print(f"{ntx} {o}")
PY
)
  txs=$((txs + $(echo $stat | cut -d' ' -f1))); outs=$((outs + $(echo $stat | cut -d' ' -f2)))
done

# ------------------------------------------------------------- verdict
heads=(); comms=()
for p in "${PORTS[@]}"; do
  r=$(rpc "$p" arc_getHead '{}')
  heads+=("$(echo "$r" | python3 -c 'import sys,json;print(json.load(sys.stdin)["result"]["number"])')")
  comms+=("$(echo "$r" | python3 -c 'import sys,json;print(json.load(sys.stdin)["result"]["commitment"])')")
done
uniq_c=$(printf '%s\n' "${comms[@]}" | sort -u | wc -l)
say "heads: ${heads[*]}  distinct commitments: $uniq_c"
[ "$uniq_c" = "1" ] || { echo "FAIL: nodes diverged"; exit 1; }
[ "${heads[0]}" = "$HEIGHTS" ] || { echo "FAIL: expected head $HEIGHTS, got ${heads[0]}"; exit 1; }
[ "$txs" -gt 0 ] || { echo "FAIL: all blocks were empty"; exit 1; }
echo
echo "✅ PASS — lean lane runs from this repo alone"
echo "   $HEIGHTS heights · $txs transactions · $outs payments · 3/3 nodes at ${comms[0]:0:18}..."
echo "   (no docker, no fleet, no reth fork)"
