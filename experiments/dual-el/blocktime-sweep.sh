#!/usr/bin/env bash
# 2-D sweep: TARGET BLOCK TIME x GAS LIMIT -> what is the ideal block time for the payment lane?
#
# Both axes are runtime-governable on one chain, no restarts:
#   * target block time  -- ProtocolConfig consensusParams, via set-block-time.sh
#   * block gas limit    -- ProtocolConfig, via set-lane-economics.sh
#
# WHY DISTRIBUTED LOAD IS MANDATORY HERE: local load generation on this box saturates at ~6,000
# tx/s (12 containers + spammers contend for 16 cores; measured 2026-08-09, doubling spammers
# 6->12 barely moved delivery). Every previous high-gas row was therefore DELIVERY-bound at
# 19-67% full and measured the spammer, not the lane. This script drives load from ginnythui and
# papaduck-alien2 as well, over tailscale, against THIS box's payment EL.
#
# A row is only trustworthy when %full is high. Rows below FULL_MIN are flagged NOT-SATURATED and
# must not be quoted as capacity.
#
#   ./blocktime-sweep.sh
#   BLOCK_TIMES="250 500 1000" SIZES="50 100 200" ./blocktime-sweep.sh
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"; cd "$REPO"
export PATH="$HOME/.foundry/bin:$HOME/.cargo/bin:$PATH"

BLOCK_TIMES=${BLOCK_TIMES:-"250 500 1000"}
SIZES=${SIZES:-"50 100 200"}
WINDOW=${WINDOW:-75}
SETTLE=${SETTLE:-45}
LOCAL_N=${LOCAL_N:-4}
GINNY_IP=${GINNY_IP:-100.124.148.61}     # this box, as the remotes see it
REMOTES=${REMOTES:-"ginnythui:6 papaduck-alien2:4"}
FULL_MIN=${FULL_MIN:-85}
OUT=${OUT:-/tmp/blocktime-sweep.txt}
: > "$OUT"

tss(){ local h=$1; shift; timeout -k 10 120 tailscale ssh "$h" "$@" </dev/null; }

stop_load(){
  pkill -x spammer 2>/dev/null
  for spec in $REMOTES; do tss "${spec%%:*}" 'pkill -x spammer 2>/dev/null; true' >/dev/null 2>&1; done
  sleep 4
}

start_load(){ # start_load <duration>
  local dur=$1 off=0 k
  for k in $(seq 0 $((LOCAL_N-1))); do
    nohup target/release/spammer ws --targets ws://127.0.0.1:19546 --chain-id 1338 \
      -r 12000 -t "$dur" -g 20 -a 1000 --account-offset $((off*1000)) -l --mix transfer=100 \
      >/tmp/bts_local_$k.log 2>&1 &
    off=$((off+1))
  done
  for spec in $REMOTES; do
    local h=${spec%%:*} n=${spec##*:}
    for k in $(seq 0 $((n-1))); do
      tss "$h" "nohup ~/spammer ws --targets ws://${GINNY_IP}:19546 --chain-id 1338 \
        -r 12000 -t $dur -g 20 -a 1000 --account-offset $((off*1000)) -l --mix transfer=100 \
        >/tmp/bts_${h}_$k.log 2>&1 & echo started" >/dev/null 2>&1
      off=$((off+1))
    done
  done
  echo "  load: ${LOCAL_N} local + ${REMOTES} = $off spammers"
}

printf "%7s %7s %9s %8s %8s %9s %7s %8s %8s %9s  %s\n" \
  "targetms" "gas(M)" "txs/blk" "blk/s" "ms/blk" "tps" "%full" "exec" "root" "persist" "verdict" | tee -a "$OUT"

for bt in $BLOCK_TIMES; do
  stop_load
  bash experiments/dual-el/set-block-time.sh "$bt" >/tmp/bts_bt_$bt.log 2>&1
  sleep 8
  for m in $SIZES; do
    gas=$((m * 1000000))
    stop_load
    PAY_GAS=$gas EVM_GAS=30000000 bash experiments/dual-el/fleet/set-lane-economics.sh apply \
      >/tmp/bts_apply_${bt}_$m.log 2>&1
    sleep 10
    got=$(cast block latest --rpc-url http://127.0.0.1:19545 --json 2>/dev/null \
          | python3 -c 'import json,sys;print(int(json.load(sys.stdin)["gasLimit"],16))' 2>/dev/null)
    if [ "${got:-0}" != "$gas" ]; then
      printf "%7s %7s  gas limit did not take effect (header %s) -- skipped\n" "$bt" "$m" "${got:-?}" | tee -a "$OUT"
      continue
    fi
    start_load $((WINDOW+SETTLE+90)) >/dev/null
    sleep $SETTLE
    BT=$bt GAS=$gas WINDOW=$WINDOW FULL_MIN=$FULL_MIN python3 - <<'PY' | tee -a "$OUT"
import json,os,time,urllib.request
BT=int(os.environ["BT"]); GAS=int(os.environ["GAS"]); W=int(os.environ["WINDOW"])
FULL_MIN=float(os.environ["FULL_MIN"])
def rpc(v,m,p=None):
    r=urllib.request.Request(f"http://127.0.0.1:{19545+(v-1)*100}",
        data=json.dumps({"jsonrpc":"2.0","id":1,"method":m,"params":p or []}).encode(),
        headers={"content-type":"application/json"})
    return json.load(urllib.request.urlopen(r,timeout=20))["result"]
def met(v):
    t=urllib.request.urlopen(f"http://127.0.0.1:{19001+(v-1)*100}",timeout=10).read().decode()
    d={}
    for l in t.splitlines():
        if l.startswith("#") or " " not in l: continue
        k,_,val=l.rpartition(" ")
        try: d[k]=float(val)
        except ValueError: pass
    return d
def leader():
    best,bh=None,-1
    for v in (1,2,3,4):
        try:
            h=int(rpc(v,"eth_blockNumber"),16)
            if h>bh: best,bh=v,h
        except Exception: pass
    return best
V=leader()
EX="reth_sync_execution_execution_histogram"; RT="reth_sync_block_validation_state_root_histogram"
PS="reth_consensus_engine_persistence_save_blocks_duration_seconds"
def snap():
    m=met(V); g=lambda k,s: m.get(f"{k}_{s}",0.0)
    return dict(h=int(rpc(V,"eth_blockNumber"),16),t=time.time(),
                exs=g(EX,"sum"),exc=g(EX,"count"),rts=g(RT,"sum"),rtc=g(RT,"count"),
                pss=g(PS,"sum"),psc=g(PS,"count"))
a=snap(); time.sleep(W); b=snap()
n=b["h"]-a["h"]; dt=b["t"]-a["t"]
if n<=0:
    print(f"{BT:7d} {GAS//10**6:7d}  no blocks in window -- STALLED"); raise SystemExit
tx=g_used=0
for h in range(a["h"]+1,b["h"]+1):
    try:
        blk=rpc(V,"eth_getBlockByNumber",[hex(h),False])
        tx+=len(blk["transactions"]); g_used+=int(blk["gasUsed"],16)
    except Exception: pass
per=lambda s,c: ((b[s]-a[s])/max(b[c]-a[c],1))*1000.0
blks=n/dt; ms=1000/blks; full=100*g_used/n/GAS
# "holds" = achieved cadence within 5% of the requested target
hold = ms <= BT*1.05
verdict = ("HOLDS" if hold else "misses") + ("" if full>=FULL_MIN else "  NOT-SATURATED")
print(f"{BT:7d} {GAS//10**6:7d} {tx/n:9.0f} {blks:8.2f} {ms:8.0f} {tx/dt:9.0f} {full:6.0f}% "
      f"{per('exs','exc'):8.1f} {per('rts','rtc'):8.1f} {per('pss','psc'):9.1f}  {verdict}   (val{V})")
PY
  done
done
stop_load
echo "" | tee -a "$OUT"
echo "results: $OUT" | tee -a "$OUT"
