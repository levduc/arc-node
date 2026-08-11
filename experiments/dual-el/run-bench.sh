#!/usr/bin/env bash
# One-shot payment-lane benchmark for the dashboard button — PROPER load methodology
# (see fleet/LOADING.md):
#   phase 1: GOVERNED sustained window (closed-loop --pool-target) -> measures the CHAIN with
#            live ingress. The old open-loop RATE=12000 collapsed the pool and measured the
#            spammer.
#   phase 2: capacity via PREFILL+DRAIN: blast the pool full, stop ALL intake, time the
#            full-block drain -> the chain's ceiling at this block size.
# Status contract unchanged (/tmp/pay-bench/bench-status.json); result gains capacity_* keys.
set -uo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUN=/tmp/pay-bench; mkdir -p "$RUN"
STATUS="$RUN/bench-status.json"
WINDOW=${WINDOW:-300}
# S*ACCTS*4 machines must fit the chain's prefunded accounts (MetaMask demo default:
# EXTRA_ACCOUNTS=1000) AND ACCTS must divide by the launcher's 20 generators.
# -> S=1 ACCTS=200 (4x200=800 <= 1000, 200/20=10). 16k-account chains can pass S=4 ACCTS=1000.
S=${S:-1}; ACCTS=${ACCTS:-200}; RATE=${RATE:-3500}
st(){ printf '%s\n' "$1" > "$STATUS"; }

GAS=$(python3 -c "
import json,urllib.request
r=urllib.request.Request('http://127.0.0.1:19545',data=json.dumps({'jsonrpc':'2.0','id':1,'method':'eth_getBlockByNumber','params':['latest',False]}).encode(),headers={'content-type':'application/json'})
print(int(json.load(urllib.request.urlopen(r,timeout=10))['result']['gasLimit'],16))")
PT=$(( GAS / 21000 * 3 )); [ "$PT" -lt 12000 ] && PT=12000

# When a remote spammer fails to launch (tailscale ssh check expired), start that validator's
# account range LOCALLY against val1's EL instead — the chain still gets the full 800-sender
# spread (gossip fans txs out), delivery just comes from one box. Without this, dead remotes
# halve delivery AND cap the fill at 200 senders x slots.
local_topup(){ # $1 = duration seconds, $2 = extra spammer flags ('' or --pool-target N), $3 = rate
  local n off i
  n=$(grep -c '!! failed to launch' "$RUN/bench.log" 2>/dev/null || echo 0)
  [ "$n" -gt 0 ] || return 0
  echo "local top-up: $n remote spammer(s) failed — launching their ranges locally" >>"$RUN/bench.log"
  i=0
  for off in 200 400 600; do
    [ $i -lt "$n" ] || break
    setsid nohup "$REPO_ROOT/target/release/spammer" ws --targets ws://127.0.0.1:19546 \
      --chain-id 1338 -r "$3" -t "$1" -g 20 -a 200 --account-offset $off -l \
      --mix transfer=100 $2 >/tmp/spam-topup-$off.log 2>&1 &
    i=$((i+1))
  done
}
REPO_ROOT="$(cd "$DIR/../.." && pwd)"

st "{\"state\":\"running\",\"phase\":\"starting governed load (pool target $PT)\",\"window_s\":$WINDOW}"
if ! POOL_TARGET=$PT S=$S ACCTS=$ACCTS RATE=$RATE DUR=$((WINDOW+240)) \
     "$DIR/fleet/spam-fleet-distributed.sh" start >"$RUN/bench.log" 2>&1; then
  st '{"state":"error","phase":"spam start failed (see /tmp/pay-bench/bench.log)"}'; exit 1
fi
local_topup $((WINDOW+240)) "--pool-target $PT" "$RATE"
st "{\"state\":\"running\",\"phase\":\"warming up (governed)\",\"window_s\":$WINDOW}"
sleep 60
st "{\"state\":\"running\",\"phase\":\"sustained window: ${WINDOW}s governed, live ingress\",\"window_s\":$WINDOW}"
rm -f "$RUN/result.json"
WINDOW=$WINDOW "$DIR/pay-throughput-bench.sh" measure >>"$RUN/bench.log" 2>&1 || true

st '{"state":"running","phase":"capacity: shrinking blocks to build a deep backlog"}'
rm -f "$RUN/capacity.json" "$RUN/backlog.txt"
# The chain's capacity EXCEEDS max spam delivery (~8k tps), so at the demo config the pool can
# NEVER build at any spam rate — the chain fill-paces every block (the drain-test finding,
# fleet/LOADING.md). And targetBlockTimeMs is CL-bounded to [0,1s] (consensus_params.rs), so
# pacing can't throttle consumption enough either. The working throttle is the GAS LIMIT:
# shrink the payment lane to 25M at runtime (ProtocolConfig governance, same path as
# blocksize-sweep.sh), fill the pool deep, restore the real limit, then stop intake and drain.
export PATH="$HOME/.foundry/bin:$PATH"
PC=0x3600000000000000000000000000000000000001
PAY_RPC=http://127.0.0.1:19545
SIG_FP='feeParams()((uint64,uint64,uint64,uint256,uint256,uint256))'
# localdev ProtocolConfig controller = hardhat dev account #8 (same key set-lane-economics.sh uses)
CTRL_KEY="${CTRL_KEY:-0xdbda1821b80551c9d65939329250298aa3472ba22feea921c0cf5d620ea67b97}"
pay_gas_set(){ # $1 = new blockGasLimit; $2.. = extra cast-send flags (pricing for under-load sends)
  local gas=$1; shift
  # the extra sed strips cast's scientific-notation annotations ("20000000000 [2e10]")
  local cur; cur=$(cast call $PC "$SIG_FP" --rpc-url $PAY_RPC 2>/dev/null | sed 's/[()]//g; s/ \[[^]]*\]//g') || return 1
  local a k iv mn mx
  a=$(echo "$cur"|cut -d, -f1|tr -d ' '); k=$(echo "$cur"|cut -d, -f2|tr -d ' ')
  iv=$(echo "$cur"|cut -d, -f3|tr -d ' '); mn=$(echo "$cur"|cut -d, -f4|tr -d ' '); mx=$(echo "$cur"|cut -d, -f5|tr -d ' ')
  cast send $PC "updateFeeParams((uint64,uint64,uint64,uint256,uint256,uint256))" \
    "($a,$k,$iv,$mn,$mx,$gas)" --private-key "$CTRL_KEY" --rpc-url $PAY_RPC --timeout 60 "$@" >/dev/null
}
pay_gas_verify(){ # $1 = expected limit, $2 = timeout seconds; poll the live header
  local want=$1 t0=$SECONDS g
  while [ $((SECONDS-t0)) -lt "$2" ]; do
    g=$(python3 -c "
import json,urllib.request
r=urllib.request.Request('$PAY_RPC',data=json.dumps({'jsonrpc':'2.0','id':1,'method':'eth_getBlockByNumber','params':['latest',False]}).encode(),headers={'content-type':'application/json'})
print(int(json.load(urllib.request.urlopen(r,timeout=8))['result']['gasLimit'],16))" 2>/dev/null)
    [ "$g" = "$want" ] && return 0
    sleep 1
  done
  return 1
}
pay_gas_verify_fast(){ # same, 0.5s poll — used under load where every second bleeds backlog
  local want=$1 t0=$SECONDS g
  while [ $((SECONDS-t0)) -lt "$2" ]; do
    g=$(python3 -c "
import json,urllib.request
r=urllib.request.Request('$PAY_RPC',data=json.dumps({'jsonrpc':'2.0','id':1,'method':'eth_getBlockByNumber','params':['latest',False]}).encode(),headers={'content-type':'application/json'})
print(int(json.load(urllib.request.urlopen(r,timeout=8))['result']['gasLimit'],16))" 2>/dev/null)
    [ "$g" = "$want" ] && return 0
    sleep 0.5
  done
  return 1
}
restore_all(){ # idempotent full restore: kill all intake, restore gas limit (quiet pool), repace 500
  pkill -x spammer 2>/dev/null
  for ip in 100.85.150.119 100.70.62.92 100.86.97.40; do
    timeout 12 tailscale ssh papaduck@$ip "pkill -x arc-spammer" >/dev/null 2>&1 &
  done
  wait
  sleep 3
  pay_gas_set "$GAS" >>"$RUN/bench.log" 2>&1
  pay_gas_verify "$GAS" 30 >>"$RUN/bench.log" 2>&1 || echo "WARN: gas restore unverified" >>"$RUN/bench.log"
  bash "$DIR/set-block-time.sh" 500 >>"$RUN/bench.log" 2>&1 || true
}
trap restore_all EXIT
# quiesce (a governance tx starves under saturating load — blocksize-sweep.sh bug 2), then shrink
"$DIR/fleet/spam-fleet-distributed.sh" stop >>"$RUN/bench.log" 2>&1 || true
pkill -x spammer 2>/dev/null || true
sleep 8
if ! pay_gas_set 25000000 >>"$RUN/bench.log" 2>&1 || ! pay_gas_verify 25000000 30; then
  echo "gas shrink failed — skipping capacity phase" >>"$RUN/bench.log"
else
  # fill: 25M @ 500ms consumes ~2.4k tps vs ~8k delivery -> the pool builds until either 150k or
  # the per-sender slot ceiling (accounts * max-account-slots; 12.8k on the default 800-acct demo)
  S=$S ACCTS=$ACCTS RATE=12000 DUR=300 "$DIR/fleet/spam-fleet-distributed.sh" start >>"$RUN/bench.log" 2>&1 || true
  local_topup 300 "" 12000
  python3 "$DIR/bench-pool-wait.py" "$RUN" >>"$RUN/bench.log" 2>&1
  BACKLOG=$(cat "$RUN/backlog.txt" 2>/dev/null || echo 0)

  # The stop window (parallel remote pkill, ~2s) bleeds ~8k txs of backlog, so a drain needs a
  # DEEP pool to survive it. Below 30k the number would be measured at tiny blocks and read
  # BELOW the sustained tps — confusing, not wrong, so we skip and say why. Pools this shallow
  # mean the per-sender slot cap (16 default): chains started with PAY_ACCOUNT_SLOTS=256 (now
  # the launcher default) fill to 150k+ and drain at the chain's own block size.
  DRAIN_GAS=$(( BACKLOG * 21000 / 6 )); [ "$DRAIN_GAS" -gt "$GAS" ] && DRAIN_GAS=$GAS
  DRAIN_GAS=$(( DRAIN_GAS / 1000000 * 1000000 ))
  if [ "$BACKLOG" -lt 30000 ]; then
    echo "backlog $BACKLOG < 30k — skipping drain (per-sender slot cap)" >>"$RUN/bench.log"
    printf '{"capacity_note":"capacity drain skipped: pool depth %s (per-sender slot cap, 16 tx x %s senders). Chains started with PAY_ACCOUNT_SLOTS=256 fill deep enough for a full-size drain."}' \
      "$BACKLOG" "$((S*ACCTS*4))" > "$RUN/capacity.json"
  else
  st '{"state":"running","phase":"capacity: restoring block size + timing the drain"}'
  echo "backlog $BACKLOG -> drain at $((DRAIN_GAS/1000000))M blocks" >>"$RUN/bench.log"
  # unpace so the drain measures NATURAL cadence (a 500ms pacer would floor small-block drains);
  # EVM-lane pool is quiet so this lands immediately
  bash "$DIR/set-block-time.sh" 0 >>"$RUN/bench.log" 2>&1 || true
  # switch to the drain gas limit UNDER LOAD: outbid the spam txs (they tip ~0) so the builder
  # includes the governance tx first; then require the limit on a live header before draining
  pay_gas_set "$DRAIN_GAS" --gas-price 2000000000000 --priority-gas-price 1000000000000 >>"$RUN/bench.log" 2>&1
  if ! pay_gas_verify_fast "$DRAIN_GAS" 25; then
    echo "gas flip under load failed — aborting drain (trap restores)" >>"$RUN/bench.log"
  else
    # FAST parallel stop (serial tailscale stop takes 10-15s; the chain eats ~4k tps of backlog net).
    # The sampler starts FIRST and stamps every block; capacity counts only full blocks after stop-ts.
    rm -f "$RUN/stop-ts"
    ( pkill -x spammer 2>/dev/null
      for ip in 100.85.150.119 100.70.62.92 100.86.97.40; do
        timeout 12 tailscale ssh papaduck@$ip "pkill -x arc-spammer" >/dev/null 2>&1 &
      done
      wait
      date +%s.%N > "$RUN/stop-ts"
    ) &
    STOPPID=$!
    python3 "$DIR/bench-drain.py" "$RUN" "$((DRAIN_GAS/1000000))" >>"$RUN/bench.log" 2>&1
    wait $STOPPID 2>/dev/null || true
  fi
  fi
fi
"$DIR/fleet/spam-fleet-distributed.sh" stop >>"$RUN/bench.log" 2>&1 || true
restore_all
trap - EXIT

if [ -f "$RUN/result.json" ]; then
  python3 - "$RUN" > "$STATUS" <<'PY'
import json,sys
run=sys.argv[1]
r=json.load(open(run+"/result.json"))
try: r.update(json.load(open(run+"/capacity.json")))
except Exception: pass
print(json.dumps({"state":"done","result":r}))
PY
else
  st '{"state":"error","phase":"no result produced (see /tmp/pay-bench/bench.log)"}'
fi
