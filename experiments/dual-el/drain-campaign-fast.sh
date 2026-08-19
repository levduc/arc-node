#!/usr/bin/env bash
# Deterministic capacity-drain campaign — the reproducible source for the deck's capacity
# numbers (finalize-bars totals, TLDR capacity series, plateau frame).
#
#   SIZES="300 300 150" FILL_TARGET=190000 ./drain-campaign.sh
#   SIZES="300 300"     FILL_TARGET=50000  ./drain-campaign.sh    # shallow arm (old-campaign depth)
#
# One drain = quiesce -> pool acceptance probe -> shrink 25M -> fleet fill to FILL_TARGET ->
# unpace -> flip <size> under load (high tip, header-verified) -> fast parallel intake stop ->
# stamped sampler counts only 100%-full blocks AT the target size after stop -> restore
# 200M/500ms (trap-guarded, high-tip). Results append to $OUT as one JSON line per drain.
#
# Method notes (each clause exists because omitting it produced a wrong number once):
# - pool depth is THE confound: 48-73k backlogs give ~3.5 blocks at 300M (resolution-limited,
#   biased slow); 190k gives 12+. Compare arms only at equal FILL_TARGET.
# - governance txs under load must outbid the backlog or they starve (blocksize-sweep bug 2).
# - after churn the pool can reject ("txpool is full") while txpool_status reads 0 — probe
#   with a real tx before filling.
# - a wrong-size transition block must neither count as capacity nor end the sampling.
set -uo pipefail
export PATH="$HOME/.foundry/bin:$PATH"
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SIZES=${SIZES:-"300"}
FILL_TARGET=${FILL_TARGET:-190000}
OUT=${OUT:-/tmp/drain-campaign-results.jsonl}
PC=0x3600000000000000000000000000000000000001
RPC=http://127.0.0.1:19545
KEY=0xdbda1821b80551c9d65939329250298aa3472ba22feea921c0cf5d620ea67b97
SIG='feeParams()((uint64,uint64,uint64,uint256,uint256,uint256))'
TIP='--gas-price 2000000000000 --priority-gas-price 1000000000000'

gas_set(){ local gas=$1; shift
  local cur a k iv mn mx
  cur=$(cast call $PC "$SIG" --rpc-url $RPC 2>/dev/null | sed 's/[()]//g; s/ \[[^]]*\]//g') || return 1
  a=$(echo "$cur"|cut -d, -f1|tr -d ' '); k=$(echo "$cur"|cut -d, -f2|tr -d ' ')
  iv=$(echo "$cur"|cut -d, -f3|tr -d ' '); mn=$(echo "$cur"|cut -d, -f4|tr -d ' '); mx=$(echo "$cur"|cut -d, -f5|tr -d ' ')
  cast send $PC "updateFeeParams((uint64,uint64,uint64,uint256,uint256,uint256))" \
    "($a,$k,$iv,$mn,$mx,$gas)" --private-key $KEY --rpc-url $RPC --timeout 60 "$@" >/dev/null
}
gl(){ python3 -c "
import json,urllib.request
r=urllib.request.Request('$RPC',data=json.dumps({'jsonrpc':'2.0','id':1,'method':'eth_getBlockByNumber','params':['latest',False]}).encode(),headers={'content-type':'application/json'})
print(int(json.load(urllib.request.urlopen(r,timeout=8))['result']['gasLimit'],16))" 2>/dev/null; }
pend(){ python3 -c "
import json,urllib.request
r=urllib.request.Request('$RPC',data=json.dumps({'jsonrpc':'2.0','id':1,'method':'txpool_status','params':[]}).encode(),headers={'content-type':'application/json'})
s=json.load(urllib.request.urlopen(r,timeout=8))['result']
f=lambda x:int(x,16) if isinstance(x,str) else int(x)
print(f(s['pending'])+f(s['queued']))" 2>/dev/null || echo -1; }
verify(){ local want=$1 t0=$SECONDS; while [ $((SECONDS-t0)) -lt "$2" ]; do [ "$(gl)" = "$want" ] && return 0; sleep 0.5; done; return 1; }
stop_all_spam(){
  pkill -x spammer 2>/dev/null
  for ip in 100.85.150.119 100.70.62.92 100.86.97.40; do
    timeout 12 tailscale ssh papaduck@$ip "pkill -x arc-spammer" >/dev/null 2>&1 &
  done; wait
}
restore(){ stop_all_spam; sleep 2
  gas_set 200000000 $TIP
  verify 200000000 40 || echo "WARN: 200M restore unverified"
  bash "$DIR/set-block-time.sh" 500 >/dev/null 2>&1 || true
}
trap restore EXIT

run_one(){ # $1 = size in M; returns via $RUN/capacity.json
  local SIZE_M=$1 SIZE=$(( $1 * 1000000 ))
  local RUN=/tmp/drain-run; rm -rf "$RUN"; mkdir -p "$RUN"

  echo "-- [${SIZE_M}M @ ${FILL_TARGET}] wait pool clean + acceptance probe"
  local t0=$SECONDS p
  while [ $((SECONDS-t0)) -lt 120 ]; do p=$(pend); [ "$p" -ge 0 ] && [ "$p" -lt 1000 ] && break; sleep 2; done
  t0=$SECONDS
  while [ $((SECONDS-t0)) -lt 300 ]; do
    cast send 0x0000000000000000000000000000000000001234 --value 1 \
      --private-key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 \
      --rpc-url $RPC --timeout 30 2>/dev/null | grep -q "status               1" && break
    sleep 4
  done

  echo "-- [${SIZE_M}M] shrink 25M + corpus fill"
  stop_all_spam; sleep 2
  gas_set 25000000 $TIP
  verify 25000000 30 || { echo "shrink failed"; return 1; }
  # replay pre-signed corpus fills (disjoint account ranges per fill) on every
  # machine's LOCAL pay EL until the pool reaches FILL_TARGET. FILL_IDX_FILE
  # tracks consumed fills across the whole chain lifetime.
  local IDXF=${FILL_IDX_FILE:-/tmp/corpus-fill-idx}
  local RTS=("" "100.85.150.119" "100.70.62.92" "100.86.97.40")
  local RPORTS=(19545 19645 19745 19845)
  while :; do
    p=$(pend); [ "$p" -gt "$FILL_TARGET" ] && break
    local FIDX=$(cat "$IDXF" 2>/dev/null || echo 0)
    [ -f "/tmp/corpus-f${FIDX}-m0.txt" ] || { echo "corpus exhausted at fill $FIDX"; break; }
    echo "   replaying corpus fill $FIDX (pool at $p)"
    python3 "$DIR/replay-corpus.py" /tmp/corpus-f${FIDX}-m0.txt http://127.0.0.1:19545 8 &
    local RPIDS=($!)
    for m in 1 2 3; do
      timeout -k 10 120 tailscale ssh papaduck@${RTS[$m]}         "python3 /tmp/replay-corpus.py /tmp/corpus-f${FIDX}-m${m}.txt http://127.0.0.1:${RPORTS[$m]} 1" </dev/null >/dev/null 2>&1 &
      RPIDS+=($!)
    done
    wait "${RPIDS[@]}" 2>/dev/null
    echo $((FIDX+1)) > "$IDXF"
  done
  local BACKLOG=$(pend)

  echo "-- [${SIZE_M}M] unpace + flip under load (backlog $BACKLOG)"
  bash "$DIR/set-block-time.sh" 0 >/dev/null 2>&1 || true
  gas_set $SIZE $TIP
  verify $SIZE 40 || { echo "flip failed"; return 1; }

  echo "-- [${SIZE_M}M] stop intake + drain"
  ( stop_all_spam; date +%s.%N > "$RUN/stop-ts" ) &
  local STOPPID=$!
  python3 "$DIR/bench-drain.py" "$RUN" "$SIZE_M" 150
  wait $STOPPID 2>/dev/null || true
  "$DIR/fleet/spam-fleet-distributed.sh" stop >/dev/null 2>&1 || true

  local cap; cap=$(cat "$RUN/capacity.json" 2>/dev/null || echo '{}')
  local age; age=$(python3 -c "
import json,urllib.request
r=urllib.request.Request('$RPC',data=json.dumps({'jsonrpc':'2.0','id':1,'method':'eth_blockNumber','params':[]}).encode(),headers={'content-type':'application/json'})
print(int(json.load(urllib.request.urlopen(r,timeout=8))['result'],16))" 2>/dev/null)
  python3 - "$cap" "$SIZE_M" "$FILL_TARGET" "$BACKLOG" "$age" <<'PY' >> "$OUT"
import json,sys
cap=json.loads(sys.argv[1])
cap.update(size_m=int(sys.argv[2]),fill_target=int(sys.argv[3]),backlog=int(sys.argv[4]),chain_age=int(sys.argv[5]))
print(json.dumps(cap))
PY
  echo "== [${SIZE_M}M @ ${FILL_TARGET}] $cap"
  # re-anchor the chain at 200M between drains so every run starts identically
  gas_set 200000000 $TIP; verify 200000000 40 || true
  bash "$DIR/set-block-time.sh" 500 >/dev/null 2>&1 || true
  sleep 10
}

echo "campaign: sizes [$SIZES] fill_target $FILL_TARGET -> $OUT"
for s in $SIZES; do run_one "$s" || echo "RUN FAILED at ${s}M"; done
restore; trap - EXIT
echo "== campaign done =="
cat "$OUT"
