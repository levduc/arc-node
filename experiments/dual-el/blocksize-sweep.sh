#!/usr/bin/env bash
# Find the LARGEST payment-lane block that still sustains the target cadence (2 blk/s / 500 ms).
#
# Question this answers: with latency held fixed at Arc mainnet's 500 ms target, how many
# transactions can the CL actually carry per block before block production degrades?
#
# Method: the payment-lane block gas limit is on-chain (ProtocolConfig), so it can be swept at
# RUNTIME with set-lane-economics.sh — no restarts, same chain, same load, same everything else.
# For each size we measure achieved block rate and tps, and flag whether cadence holds.
#
#   ./blocksize-sweep.sh                    # default sweep
#   SIZES="25 50 100 200 400" WINDOW=90 ./blocksize-sweep.sh
#
# Requires: fleet (or single-machine demo) already running, and load driven separately.
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"; cd "$REPO"
export PATH="$HOME/.foundry/bin:$PATH"
SIZES=${SIZES:-"25 50 100 200 400 1000"}      # millions of gas
WINDOW=${WINDOW:-75}
TARGET_BLKS=${TARGET_BLKS:-2.0}                # blocks/s we are trying to hold
OUT=/tmp/blocksize-sweep.txt
: > "$OUT"

hdr=$(printf "%9s %9s %9s %9s %9s %10s  %s" "gas(M)" "txs/blk" "blk/s" "ms/blk" "tps" "% full" "cadence")
echo "$hdr" | tee -a "$OUT"
echo "  target: ${TARGET_BLKS} blk/s ($(python3 -c "print(int(1000/$TARGET_BLKS))") ms)" | tee -a "$OUT"

for m in $SIZES; do
  gas=$((m * 1000000))
  PAY_GAS=$gas EVM_GAS=30000000 bash experiments/dual-el/fleet/set-lane-economics.sh apply >/dev/null 2>&1
  sleep 12   # let the new limit take effect and cadence settle

  python3 - "$gas" "$WINDOW" "$TARGET_BLKS" <<'PY' | tee -a "$OUT"
import json,urllib.request,sys,time
gas,window,target=int(sys.argv[1]),int(sys.argv[2]),float(sys.argv[3])
def rpc(m,p=None):
    r=urllib.request.Request("http://127.0.0.1:19545",
        data=json.dumps({"jsonrpc":"2.0","id":1,"method":m,"params":p or []}).encode(),
        headers={"content-type":"application/json"})
    return json.load(urllib.request.urlopen(r,timeout=10))["result"]
h0=int(rpc("eth_blockNumber"),16); t0=time.time()
time.sleep(window)
h1=int(rpc("eth_blockNumber"),16); dt=time.time()-t0
n=h1-h0
if n<=0:
    print(f"{gas//10**6:9d} {'-':>9} {'0.00':>9} {'-':>9} {'-':>9} {'-':>10}  STALLED"); sys.exit()
tx=g=0
for b in range(h0+1,h1+1):
    try:
        blk=rpc("eth_getBlockByNumber",[hex(b),False])
        tx+=len(blk["transactions"]); g+=int(blk["gasUsed"],16)
    except Exception: pass
blks=n/dt; tps=tx/dt; tpb=tx/n; full=100*g/n/gas
ok = "HOLDS" if blks>=target*0.95 else ("degraded" if blks>=target*0.5 else "BROKEN")
print(f"{gas//10**6:9d} {tpb:9.0f} {blks:9.2f} {1000/blks:9.0f} {tps:9.0f} {full:9.0f}%  {ok}")
PY
done
echo "" | tee -a "$OUT"
echo "results: $OUT"
