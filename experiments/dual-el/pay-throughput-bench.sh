#!/usr/bin/env bash
# Measure payment-lane throughput on a single-machine dual-lane demo.
#
# Fans out N disjoint spammers (each a distinct prefunded account range via --account-offset, so no
# nonce collisions) across the 4 payment ELs, then samples the payment lane for WINDOW seconds and
# reports tx/s, blocks/s, txs/block, block fullness, and exec/root/persist ms from prometheus.
#
#   ./pay-throughput-bench.sh              # 8 spammers, 90s window
#   N=6 ACCTS=1000 RATE=10000 WINDOW=120 ./pay-throughput-bench.sh
#
# Requires: a running dual-lane demo (e.g. PAY_GAS=1000000000 EXTRA_ACCOUNTS=8000 demo-metamask.sh
# start) with >= N*ACCTS prefunded accounts.
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"; cd "$REPO"
SPAMMER=target/release/spammer
N=${N:-8}                 # spammer processes
ACCTS=${ACCTS:-1000}      # accounts per spammer (genesis needs >= N*ACCTS prefunded EOAs)
RATE=${RATE:-12000}       # per-spammer target tx/s (ack-gated; real is lower)
WINDOW=${WINDOW:-90}      # measurement window (s)
CID=${CID:-1338}          # payment lane chainId
RUN=/tmp/pay-bench; mkdir -p "$RUN"; rm -f "$RUN"/*.log
PORTS=(19546 19646 19746 19846)     # payment EL ws ports (4 validators)
RPC=19545                            # val1 payment EL http (for sampling)
MET=19001                            # val1 payment EL prometheus

MODE="${1:-run}"
case "$MODE" in
  stop) pkill -x spammer 2>/dev/null; echo "spammers stopped"; exit 0 ;;
esac

# 'measure' just samples (drive load yourself, e.g. fleet/spam-fleet-distributed.sh); 'run' also
# launches N local spammers first.
if [ "$MODE" != measure ]; then
  echo "==> launching $N spammers ($ACCTS accts each, offsets 0..$(((N-1)*ACCTS)), round-robin over ${PORTS[*]})"
  for k in $(seq 0 $((N-1))); do
    ws="ws://127.0.0.1:${PORTS[$((k % 4))]}"
    off=$((k * ACCTS))
    nohup $SPAMMER ws --targets "$ws" --chain-id "$CID" \
      -r "$RATE" -t "$((WINDOW+40))" -g 20 -a "$ACCTS" --account-offset "$off" \
      --mix transfer=100 >"$RUN/spam_$k.log" 2>&1 &
  done
  sleep 8   # let mempools fill and blocks start packing
fi
echo "==> sampling the payment lane for ${WINDOW}s..."

python3 - "$RPC" "$MET" "$WINDOW" "$RUN/result.json" <<'PY'
import sys, json, time, urllib.request
rpc_port, met_port, window, out_json = int(sys.argv[1]), int(sys.argv[2]), int(sys.argv[3]), sys.argv[4]
def rpc(m,p=None):
    req=urllib.request.Request(f"http://127.0.0.1:{rpc_port}",
        data=json.dumps({"jsonrpc":"2.0","id":1,"method":m,"params":p or []}).encode(),
        headers={"content-type":"application/json"})
    return json.load(urllib.request.urlopen(req,timeout=8))["result"]
def blk(n):
    b=rpc("eth_getBlockByNumber",[hex(n),False])
    return (int(b["gasUsed"],16), len(b["transactions"]), int(b["gasLimit"],16)) if b else None
def prom(names):
    raw=urllib.request.urlopen(f"http://127.0.0.1:{met_port}/metrics",timeout=8).read().decode()
    out={}
    for ln in raw.splitlines():
        for nm in names:
            if ln.startswith(nm+" "):
                try: out[nm]=float(ln.split()[1])
                except: pass
    return out
h0=int(rpc("eth_blockNumber"),16); t0=time.time(); last=h0
gas_l=None; rows=[]; arr=[]   # rows: (gas,txs,gaslimit); arr: (wall_t, txs) per block as first observed
pm0=prom(["reth_sync_execution_execution_histogram_sum","reth_sync_execution_execution_histogram_count",
          "reth_sync_block_validation_state_root_histogram_sum","reth_sync_block_validation_state_root_histogram_count",
          "reth_consensus_engine_persistence_save_blocks_duration_seconds_sum","reth_consensus_engine_persistence_save_blocks_duration_seconds_count"])
while time.time()-t0 < window:
    h=int(rpc("eth_blockNumber"),16)
    while last < h:
        last+=1; r=blk(last)
        if r: rows.append(r); arr.append((time.time(), r[1])); gas_l=r[2]
    time.sleep(0.3)
pm1=prom(list(pm0))
dt=time.time()-t0
tot_tx=sum(r[1] for r in rows); tot_gas=sum(r[0] for r in rows); nblk=len(rows)
# peak = max tx/s over any rolling window spanning >= 10s (smooths single-block noise)
peak=0.0
for i in range(len(arr)):
    for j in range(i+1, len(arr)):
        span=arr[j][0]-arr[i][0]
        if span>=10:
            tps=sum(arr[k][1] for k in range(i+1,j+1))/span
            if tps>peak: peak=tps
            break
def per(a,b):  # avg ms per block-op from histogram deltas
    ds=pm1.get(a,0)-pm0.get(a,0); dc=pm1.get(b,0)-pm0.get(b,0)
    return (ds/dc*1000) if dc>0 else None
print(f"\n================ PAYMENT LANE @ {gas_l/1e6 if gas_l else 0:.0f}M gas limit ================")
print(f"  window            {dt:.1f}s   blocks {nblk}")
print(f"  throughput (avg)  {tot_tx/dt:,.0f} tx/s")
print(f"  throughput (peak) {peak:,.0f} tx/s   (best rolling >=10s window)")
print(f"  block rate        {nblk/dt:.2f} blocks/s")
print(f"  txs / block       {tot_tx/max(nblk,1):,.0f}   (avg)")
print(f"  gas / block       {tot_gas/max(nblk,1)/1e6:,.1f}M   ({100*tot_gas/max(nblk,1)/gas_l if gas_l else 0:.0f}% full)")
e=per("reth_sync_execution_execution_histogram_sum","reth_sync_execution_execution_histogram_count")
r=per("reth_sync_block_validation_state_root_histogram_sum","reth_sync_block_validation_state_root_histogram_count")
p=per("reth_consensus_engine_persistence_save_blocks_duration_seconds_sum","reth_consensus_engine_persistence_save_blocks_duration_seconds_count")
print(f"  exec / block      {e:.1f} ms" if e else "  exec / block      n/a")
print(f"  state-root / block{r:8.1f} ms" if r else "  state-root/block  n/a")
print(f"  persist / block   {p:.1f} ms" if p else "  persist / block   n/a")
print("=========================================================")
json.dump({"window_s": round(dt,1), "blocks": nblk, "avg_tps": round(tot_tx/dt),
           "peak_tps": round(peak), "block_rate": round(nblk/dt,3),
           "txs_per_block": round(tot_tx/max(nblk,1)),
           "gas_per_block_m": round(tot_gas/max(nblk,1)/1e6,1),
           "full_pct": round(100*tot_gas/max(nblk,1)/gas_l) if gas_l else 0,
           "gas_limit_m": round(gas_l/1e6) if gas_l else 0,
           "exec_ms": round(e,1) if e else None, "root_ms": round(r,1) if r else None,
           "persist_ms": round(p,1) if p else None}, open(out_json,"w"))
PY
echo "==> stop spammers with: $0 stop"
