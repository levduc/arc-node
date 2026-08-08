#!/usr/bin/env bash
# How big a payment-lane block still holds the 2 blk/s (500 ms) target?
#
# The payment lane's block gas limit is on-chain (ProtocolConfig), so it can be swept at RUNTIME
# on one chain -- same state, same load, same binaries -- with set-lane-economics.sh.
#
# An earlier version of this sweep produced garbage (a row read "3026% full"). Three bugs, all
# fixed here, all worth remembering:
#   1. it polled a hardcoded val1, which had parked -> read STALLED while the chain ran on 3-of-4.
#      Now it polls whichever validator has the highest head.
#   2. it applied the gas-limit change while saturating load was running, so the governance tx was
#      starved out of the mempool and the limit silently never changed. Now load is STOPPED for
#      the change and the new limit is VERIFIED on a fresh block before measuring.
#   3. its host map was fleet-only. Now it defaults to the single-machine demo (all validators on
#      127.0.0.1, ports 19545+100n) and takes FLEET='{"1":"ip",...}' for the 4-machine case.
#
#   ./blocksize-sweep.sh
#   SIZES="25 50 100 200 1000" WINDOW=75 LOAD=4 ./blocksize-sweep.sh
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"; cd "$REPO"
export PATH="$HOME/.foundry/bin:$HOME/.cargo/bin:$PATH"
SIZES=${SIZES:-"25 50 100 200 500 1000"}     # millions of gas
WINDOW=${WINDOW:-75}
SETTLE=${SETTLE:-40}
LOAD=${LOAD:-4}
TARGET=${TARGET:-2.0}                        # blocks/s we are trying to hold
OUT=${OUT:-/tmp/blocksize-sweep.txt}
: > "$OUT"

stop_load(){ pkill -x spammer 2>/dev/null; sleep 3; }
start_load(){
  for k in $(seq 0 $((LOAD-1))); do
    nohup target/release/spammer ws --targets ws://127.0.0.1:19546 --chain-id 1338 \
      -r 12000 -t $((WINDOW+SETTLE+120)) -g 20 -a 1000 --account-offset $((k*1000)) \
      -l --mix transfer=100 >/tmp/bss_spam_$k.log 2>&1 &
  done
}

printf "%8s %9s %9s %9s %10s %8s %8s %8s %8s  %s\n" \
  "gas(M)" "txs/blk" "blk/s" "ms/blk" "tps" "%full" "exec" "root" "persist" "cadence" | tee -a "$OUT"
echo "  target ${TARGET} blk/s; exec/root/persist are ms per block" | tee -a "$OUT"

for m in $SIZES; do
  gas=$((m * 1000000))
  # --- bug 2: quiesce before governance, then VERIFY the limit really changed ---
  stop_load
  PAY_GAS=$gas EVM_GAS=30000000 bash experiments/dual-el/fleet/set-lane-economics.sh apply \
    >/tmp/bss_apply_$m.log 2>&1
  sleep 10
  got=$(cast block latest --rpc-url http://127.0.0.1:19545 --json 2>/dev/null \
        | python3 -c 'import json,sys; print(int(json.load(sys.stdin)["gasLimit"],16))' 2>/dev/null)
  if [ "${got:-0}" != "$gas" ]; then
    printf "%8s %9s  gas limit did NOT take effect (header says %s) -- skipping\n" \
      "$m" "-" "${got:-?}" | tee -a "$OUT"
    continue
  fi

  start_load
  sleep $SETTLE
  GAS=$gas WINDOW=$WINDOW TARGET=$TARGET python3 - <<'PY' | tee -a "$OUT"
import json,os,time,urllib.request
GAS=int(os.environ["GAS"]); W=int(os.environ["WINDOW"]); TARGET=float(os.environ["TARGET"])
FLEET=os.environ.get("FLEET","")
HOSTS={v:("127.0.0.1" if not FLEET else json.loads(FLEET)[str(v)]) for v in (1,2,3,4)}
def rpc(v,m,p=None):
    r=urllib.request.Request(f"http://{HOSTS[v]}:{19545+(v-1)*100}",
        data=json.dumps({"jsonrpc":"2.0","id":1,"method":m,"params":p or []}).encode(),
        headers={"content-type":"application/json"})
    return json.load(urllib.request.urlopen(r,timeout=15))["result"]
def met(v):
    t=urllib.request.urlopen(f"http://{HOSTS[v]}:{19001+(v-1)*100}",timeout=10).read().decode()
    d={}
    for line in t.splitlines():
        if line.startswith("#") or " " not in line: continue
        k,_,val=line.rpartition(" ")
        try: d[k]=float(val)
        except ValueError: pass
    return d
# --- bug 1: follow whichever validator is furthest ahead, never a hardcoded one ---
def leader():
    best,bh=None,-1
    for v in HOSTS:
        try:
            h=int(rpc(v,"eth_blockNumber"),16)
            if h>bh: best,bh=v,h
        except Exception: pass
    return best
V=leader()
EX="reth_sync_execution_execution_histogram"
RT="reth_sync_block_validation_state_root_histogram"
PS="reth_consensus_engine_persistence_save_blocks_duration_seconds"
def snap():
    m=met(V); g=lambda k,s: m.get(f"{k}_{s}",0.0)
    return dict(h=int(rpc(V,"eth_blockNumber"),16),t=time.time(),
                exs=g(EX,"sum"),exc=g(EX,"count"),rts=g(RT,"sum"),rtc=g(RT,"count"),
                pss=g(PS,"sum"),psc=g(PS,"count"))
a=snap(); time.sleep(W); b=snap()
n=b["h"]-a["h"]; dt=b["t"]-a["t"]
if n<=0:
    print(f"{GAS//10**6:8d} {'-':>9} {'0.00':>9}  no blocks in window -- STALLED"); raise SystemExit
tx=g_used=0
for h in range(a["h"]+1,b["h"]+1):
    try:
        blk=rpc(V,"eth_getBlockByNumber",[hex(h),False])
        tx+=len(blk["transactions"]); g_used+=int(blk["gasUsed"],16)
    except Exception: pass
per=lambda s,c: ((b[s]-a[s])/max(b[c]-a[c],1))*1000.0
blks=n/dt
ok="HOLDS" if blks>=TARGET*0.95 else ("degraded" if blks>=TARGET*0.5 else "BROKEN")
print(f"{GAS//10**6:8d} {tx/n:9.0f} {blks:9.2f} {1000/blks:9.0f} {tx/dt:10.0f} "
      f"{100*g_used/n/GAS:7.0f}% {per('exs','exc'):8.1f} {per('rts','rtc'):8.1f} "
      f"{per('pss','psc'):8.1f}  {ok}   (val{V})")
PY
done
stop_load
echo "" | tee -a "$OUT"
echo "results: $OUT" | tee -a "$OUT"
