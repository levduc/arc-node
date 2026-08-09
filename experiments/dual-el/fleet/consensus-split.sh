#!/usr/bin/env bash
# How does a block height split between EXECUTION and CONSENSUS as the gas limit grows?
#
# Every earlier answer here was "execution is a small slice", but only ever at one or two block
# sizes. This sweeps the range and decomposes the whole height, on the 4-machine fleet, using
# reth's beacon-engine metrics plus Arc's own payload-build metric:
#
#   exec        execution of the block's transactions            }
#   root        state root                                        } all inside newPayload
#   np-other    rest of newPayload (decode, tx/receipt roots)     }
#   vote gap    newPayload done -> next forkchoiceUpdated. NOT idle network time: the other
#               validators are receiving, decoding and EXECUTING the same block here, then two
#               vote rounds happen. Quorum waits for the slowest.
#   remainder   height - newPayload - vote gap = next proposer building its payload, SSZ encode,
#               proposal streaming, decode on the receivers. `build` is the proposer-side part of
#               this and is reported separately (summed over all 4, divided by heights).
#
# All figures are averaged across the four validators. Demand is tuned per size to just fill the
# block -- too little and the measurement is about the spammer, too much and the surplus distorts
# cadence.
#
#   ./consensus-split.sh
#   SIZES="25 50 100 200" WINDOW=360 ./consensus-split.sh
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"; cd "$REPO"
export PATH="$HOME/.foundry/bin:$HOME/.cargo/bin:$PATH"
SIZES=${SIZES:-"25 50 100 200"}
WINDOW=${WINDOW:-360}
SETTLE=${SETTLE:-100}
S=${S:-4}; ACCTS=${ACCTS:-1000}
OUT=${OUT:-/tmp/consensus-split.txt}
: > "$OUT"

rate_for(){ case "$1" in 25) echo 175;; 50) echo 331;; 100) echo 531;; 200) echo 700;; *) echo 531;; esac; }

printf "%6s %8s %7s %8s %7s %6s %9s %9s %10s %7s %7s\n" \
  "gas" "txs/blk" "%full" "height" "exec" "root" "np-other" "remainder" "vote-gap" "build" "cons%" | tee -a "$OUT"

for m in $SIZES; do
  gas=$((m*1000000)); rate=$(rate_for "$m")
  timeout -k 20 240 bash experiments/dual-el/fleet/spam-fleet-distributed.sh stop >/dev/null 2>&1
  sleep 8
  PAY_GAS=$gas EVM_GAS=30000000 bash experiments/dual-el/fleet/set-lane-economics.sh apply >/tmp/cs_ap_$m.log 2>&1
  sleep 12
  got=$(cast block latest --rpc-url http://127.0.0.1:19545 --json 2>/dev/null \
        | python3 -c 'import json,sys;print(int(json.load(sys.stdin)["gasLimit"],16))' 2>/dev/null)
  [ "${got:-0}" = "$gas" ] || { echo "${m}M: gas did not apply (${got:-?}) — skipped" | tee -a "$OUT"; continue; }
  S=$S ACCTS=$ACCTS RATE=$rate DUR=$((WINDOW+SETTLE+150)) \
    timeout -k 20 600 bash experiments/dual-el/fleet/spam-fleet-distributed.sh start >/dev/null 2>&1
  sleep $SETTLE

  GAS=$gas WINDOW=$WINDOW python3 - <<'PY' | tee -a "$OUT"
import json,os,time,urllib.request
H={1:"127.0.0.1",2:"100.85.150.119",3:"100.70.62.92",4:"100.86.97.40"}
GAS=int(os.environ["GAS"]); W=int(os.environ["WINDOW"])
def rpc(v,m,p=None):
    r=urllib.request.Request(f"http://{H[v]}:{19545+(v-1)*100}",
        data=json.dumps({"jsonrpc":"2.0","id":1,"method":m,"params":p or []}).encode(),
        headers={"content-type":"application/json"})
    return json.load(urllib.request.urlopen(r,timeout=20))["result"]
def met(v):
    t=urllib.request.urlopen(f"http://{H[v]}:{19001+(v-1)*100}",timeout=10).read().decode()
    d={}
    for l in t.splitlines():
        if l.startswith("#") or " " not in l: continue
        k,_,val=l.rpartition(" ")
        try: d[k]=float(val)
        except ValueError: pass
    return d
B="reth_consensus_engine_beacon_"
K={"np":B+"new_payload_latency","gap":B+"forkchoice_updated_new_payload_time_diff",
   "build":"reth_arc_payload_total_duration_seconds",
   "exec":"reth_sync_execution_execution_histogram",
   "root":"reth_sync_block_validation_state_root_histogram"}
def snap():
    o={}
    for v in H:
        try:
            m=met(v); o[v]={k:(m.get(x+"_sum",0.0),m.get(x+"_count",0.0)) for k,x in K.items()}
        except Exception: o[v]=None
    o["h"]=int(rpc(1,"eth_blockNumber"),16); o["t"]=time.time(); return o
a=snap(); time.sleep(W); b=snap()
n=b["h"]-a["h"]; dt=b["t"]-a["t"]
if n<=0: print(f"{GAS//10**6:6d}  no blocks"); raise SystemExit
tx=g=0
for x in range(a["h"]+1,b["h"]+1):
    try:
        bl=rpc(1,"eth_getBlockByNumber",[hex(x),False]); tx+=len(bl["transactions"]); g+=int(bl["gasUsed"],16)
    except Exception: pass
ok=[v for v in H if a[v] and b[v]]
def avg(k):
    xs=[((b[v][k][0]-a[v][k][0])/max(b[v][k][1]-a[v][k][1],1))*1000.0 for v in ok]
    return sum(xs)/len(xs)
# proposer build is round-robin: sum across all validators, divide by heights
bs=sum(b[v]["build"][0]-a[v]["build"][0] for v in ok)
bc=sum(b[v]["build"][1]-a[v]["build"][1] for v in ok)
build=(bs/max(bc,1))*1000
height=dt/n*1000
npl,gap,ex,rt = avg("np"),avg("gap"),avg("exec"),avg("root")
npother=max(npl-ex-rt,0.0)
rem=max(height-npl-gap,0.0)
cons=100*(gap+rem)/height          # everything the EL is not executing
print(f"{GAS//10**6:5d}M {tx/n:8.0f} {100*g/n/GAS:6.0f}% {height:7.0f}ms {ex:6.1f} {rt:5.1f} "
      f"{npother:8.1f} {rem:9.1f} {gap:9.1f} {build:6.1f} {cons:6.0f}%")
PY
done
timeout -k 20 240 bash experiments/dual-el/fleet/spam-fleet-distributed.sh stop >/dev/null 2>&1
echo "" | tee -a "$OUT"; echo "results: $OUT" | tee -a "$OUT"
