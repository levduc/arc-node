#!/usr/bin/env bash
# The payment-lane throughput/latency frontier, measured properly: 4 machines, one block size at a
# time, each held under continuous load for 15+ minutes.
#
# WHY LONG WINDOWS: every short measurement in this project carried +-12-15% run-to-run variance,
# which twice produced a headline that failed to reproduce (50M read 1.96 blk/s once and 1.72 on a
# repeat). A 15-minute window with per-minute sampling averages that out AND shows whether the
# chain is STABLE or drifting -- which a single average cannot.
#
# WHY PER-SIZE DEMAND: offered load is a second variable. Too little and the block is not full (the
# measurement becomes about the spammer); too much and the surplus costs cadence (measured: at 200M,
# doubling load took blocks 65%->100% full for +6% tps and +45% latency) while the mempool backlog
# explodes and the txpool starts rejecting. So each size is offered ~1.15x its expected capacity:
# enough to fill the block, not enough to run away.
#
#   ./frontier-long.sh                       # 6 sizes x 15 min (~2 h)
#   SIZES="50 100 200" WINDOW=900 ./frontier-long.sh
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"; cd "$REPO"
export PATH="$HOME/.foundry/bin:$HOME/.cargo/bin:$PATH"

SIZES=${SIZES:-"25 50 100 200 500 1000"}
WINDOW=${WINDOW:-900}          # 15 min of measurement per size
SETTLE=${SETTLE:-120}
S=${S:-4}; ACCTS=${ACCTS:-1000}          # 16 spammers total
OUT=${OUT:-/tmp/frontier-long.txt}
: > "$OUT"

# per-spammer tx/s so that total offered ~= 1.15x expected capacity for that size
rate_for(){ case "$1" in
  25) echo 175;; 50) echo 331;; 100) echo 531;;
  200) echo 556;; 500) echo 588;; 1000) echo 525;; *) echo 531;; esac; }

echo "payment-lane frontier — 4 machines, ${WINDOW}s per size, controlled demand" | tee -a "$OUT"
echo "" | tee -a "$OUT"

for m in $SIZES; do
  gas=$((m * 1000000)); rate=$(rate_for "$m")
  timeout -k 20 240 bash experiments/dual-el/fleet/spam-fleet-distributed.sh stop >/dev/null 2>&1
  sleep 8
  PAY_GAS=$gas EVM_GAS=30000000 bash experiments/dual-el/fleet/set-lane-economics.sh apply \
    >/tmp/frl_apply_$m.log 2>&1
  sleep 12
  got=$(cast block latest --rpc-url http://127.0.0.1:19545 --json 2>/dev/null \
        | python3 -c 'import json,sys;print(int(json.load(sys.stdin)["gasLimit"],16))' 2>/dev/null)
  if [ "${got:-0}" != "$gas" ]; then
    echo "${m}M: gas limit did not apply (header ${got:-?}) — SKIPPED" | tee -a "$OUT"; continue
  fi
  S=$S ACCTS=$ACCTS RATE=$rate DUR=$((WINDOW+SETTLE+180)) \
    timeout -k 20 600 bash experiments/dual-el/fleet/spam-fleet-distributed.sh start >/dev/null 2>&1
  sleep $SETTLE

  GAS=$gas WINDOW=$WINDOW OFFERED=$((rate*4*S)) python3 - <<'PY' | tee -a "$OUT"
import json,os,statistics,time,urllib.request
H={1:"127.0.0.1",2:"100.85.150.119",3:"100.70.62.92",4:"100.86.97.40"}
GAS=int(os.environ["GAS"]); W=int(os.environ["WINDOW"]); OFF=int(os.environ["OFFERED"])
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
def leader():
    best,bh=None,-1
    for v in H:
        try:
            h=int(rpc(v,"eth_blockNumber"),16)
            if h>bh: best,bh=v,h
        except Exception: pass
    return best
V=leader()
EX="reth_sync_execution_execution_histogram"; RT="reth_sync_block_validation_state_root_histogram"
PS="reth_consensus_engine_persistence_save_blocks_duration_seconds"
def m0():
    m=met(V); g=lambda k,s: m.get(f"{k}_{s}",0.0)
    return dict(exs=g(EX,"sum"),exc=g(EX,"count"),rts=g(RT,"sum"),rtc=g(RT,"count"),
                pss=g(PS,"sum"),psc=g(PS,"count"))
def blkinfo(h):
    b=rpc(V,"eth_getBlockByNumber",[hex(h),False]); return len(b["transactions"]),int(b["gasUsed"],16)

a=m0(); h_prev=int(rpc(V,"eth_blockNumber"),16); h0=h_prev; t_prev=t0=time.time()
samples=[]                       # (tps, blk/s) per ~60 s interval
while time.time()-t0 < W:
    time.sleep(60)
    h=int(rpc(V,"eth_blockNumber"),16); t=time.time()
    tx=sum(blkinfo(x)[0] for x in range(h_prev+1,h+1))
    dt=t-t_prev
    if h>h_prev: samples.append((tx/dt,(h-h_prev)/dt))
    h_prev,t_prev=h,t
b=m0(); h1=h_prev; dt_tot=t_prev-t0
tx_tot=g_tot=0
for x in range(h0+1,h1+1):
    try:
        n,g=blkinfo(x); tx_tot+=n; g_tot+=g
    except Exception: pass
nblk=h1-h0
per=lambda s,c: ((b[s]-a[s])/max(b[c]-a[c],1))*1000.0
tps=[s[0] for s in samples]; bps=[s[1] for s in samples]
half=len(tps)//2
drift = (statistics.mean(tps[half:])/statistics.mean(tps[:half])-1)*100 if half>=2 else float('nan')
mean_bps=statistics.mean(bps)
print(f"{GAS//10**6}M gas — {nblk} blocks over {dt_tot/60:.1f} min, offered {OFF:,} tx/s")
print(f"  txs/block      {tx_tot/nblk:>9,.0f}   ({100*g_tot/nblk/GAS:.0f}% full)")
print(f"  block latency  {1000/mean_bps:>9,.0f} ms   ({mean_bps:.2f} blk/s)   "
      f"range {1000/max(bps):.0f}-{1000/min(bps):.0f} ms")
print(f"  throughput     {statistics.mean(tps):>9,.0f} tx/s  "
      f"median {statistics.median(tps):,.0f}  min {min(tps):,.0f}  max {max(tps):,.0f}")
print(f"  stability      sd {statistics.pstdev(tps)/statistics.mean(tps)*100:>6.1f}%   "
      f"2nd half vs 1st {drift:+.1f}%   ({len(samples)} samples)")
print(f"  EL per block   exec {per('exs','exc'):.1f} ms   root {per('rts','rtc'):.1f} ms   "
      f"persist {per('pss','psc'):.1f} ms")
# consensus health across all four machines
try:
    heads={v:int(rpc(v,"eth_blockNumber"),16) for v in H}
    top=min(heads.values())-2; bad=0
    for x in range(top-49,top+1):
        st={v:tuple(rpc(v,"eth_getBlockByNumber",[hex(x),False])[k]
                    for k in ("stateRoot","hash","receiptsRoot","logsBloom")) for v in H}
        if len(set(st.values()))!=1: bad+=1
    print(f"  agreement      {'all 4 agree over 50 blocks' if bad==0 else f'DIVERGED at {bad} heights'}")
except Exception as e:
    print(f"  agreement      check failed: {e}")
print()
PY
done
timeout -k 20 240 bash experiments/dual-el/fleet/spam-fleet-distributed.sh stop >/dev/null 2>&1
echo "results: $OUT" | tee -a "$OUT"
