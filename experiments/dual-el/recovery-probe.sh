#!/usr/bin/env bash
# Where does the newPayload execution phase actually go, per validator?
#
# reth's execute_transactions loop (payload_validator.rs:1276) splits into:
#   wait  -- blocked on transactions.next(), i.e. the ORDERED channel fed by the
#            signature-recovery producer (RLP decode + ECDSA recover, run on the
#            reth cpu_pool via for_each_ordered_in)
#   exec  -- executor.execute_transaction(), i.e. OUR ArcBlockExecutor
#   other -- receipt clone/send, metrics, atomics
#
# Sampling several validators at once is a clean A/B: they import the SAME blocks
# on the SAME box, so any per-validator config difference (e.g. a payment EL
# started with --engine.state-root-fallback via PAY_EL<n>_EXTRA_ARGS) is the only
# variable. Cross-MACHINE comparisons are NOT valid here (hardware-confounded).
#
#   ./recovery-probe.sh                    # sample all 4, 90s
#   VALS="1 2" WINDOW=120 ./recovery-probe.sh
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"; cd "$REPO"
VALS=${VALS:-"1 2 3 4"}
WINDOW=${WINDOW:-90}
OUT=${OUT:-/tmp/recovery-probe.txt}
: > "$OUT"

VALS=$VALS WINDOW=$WINDOW python3 - <<'PY' | tee -a "$OUT"
import json,os,time,urllib.request
VALS=[int(v) for v in os.environ["VALS"].split()]; W=int(os.environ["WINDOW"])
EX="reth_sync_execution_transaction_execution_histogram"
WT="reth_sync_execution_transaction_wait_histogram"
def met(v):
    port=19001+(v-1)*100
    t=urllib.request.urlopen(f"http://127.0.0.1:{port}",timeout=10).read().decode()
    d={}
    for line in t.splitlines():
        if line.startswith("#") or " " not in line: continue
        k,_,val=line.rpartition(" ")
        try: d[k]=float(val)
        except ValueError: pass
    return d
def head(v):
    port=19545+(v-1)*100
    r=urllib.request.Request(f"http://127.0.0.1:{port}",
        data=json.dumps({"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}).encode(),
        headers={"content-type":"application/json"})
    return int(json.load(urllib.request.urlopen(r,timeout=10))["result"],16)
def snap(v):
    m=met(v); g=lambda k,s: m.get(f"{k}_{s}",0.0)
    return dict(exs=g(EX,"sum"),exc=g(EX,"count"),wts=g(WT,"sum"),h=head(v))
a={}
for v in VALS:
    try: a[v]=snap(v)
    except Exception as e: print(f"  val{v}: unreachable ({e})")
time.sleep(W)
print(f"{'val':>4} {'blocks':>7} {'txs/blk':>9} {'loop/tx':>9} {'wait/tx':>9} {'exec/tx':>9} {'wait%':>6} {'exec ms/blk':>12}")
for v in VALS:
    if v not in a: continue
    try: b=snap(v)
    except Exception as e: print(f"{v:>4}  unreachable ({e})"); continue
    ntx=b["exc"]-a[v]["exc"]; nblk=b["h"]-a[v]["h"]
    if ntx<=0 or nblk<=0:
        print(f"{v:>4} {nblk:>7} {'-':>9}  no progress in window"); continue
    wait=(b["wts"]-a[v]["wts"])*1e6/ntx      # us per tx
    exe =(b["exs"]-a[v]["exs"])*1e6/ntx
    loop=wait+exe                             # 'other' is not separately instrumented
    print(f"{v:>4} {nblk:>7} {ntx/nblk:>9.0f} {loop:>9.2f} {wait:>9.2f} {exe:>9.2f} "
          f"{100*wait/max(loop,1e-9):>5.0f}% {(wait+exe)*ntx/nblk/1000:>11.1f}")
PY
echo "" | tee -a "$OUT"
echo "results: $OUT"
