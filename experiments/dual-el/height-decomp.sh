#!/usr/bin/env bash
# Where does a payment-lane HEIGHT go? EL-visible work vs everything else.
#
# MISSION-EL.md's hypothesis #1 was "engine-API ingestion (UNMEASURED, top suspect)" -- the idea
# that a big slice of block time hides between the CL issuing newPayload and reth starting work,
# and that switching the payment lane to IPC (a config choice, so inside the EL-only constraint)
# would recover it.
#
# reth's beacon-engine metrics decompose the height from the EL's side:
#   new_payload_latency   -- EL handling of newPayload (execution + root, synchronous)
#   time_diff             -- newPayload completing -> the following forkchoiceUpdated arriving.
#                            The EL is IDLE here; this is the CL voting round.
#   fcu_latency           -- EL handling of forkchoiceUpdated
#   remainder             -- wall-clock height minus the three above: next proposer building its
#                            payload, SSZ encode, proposal streaming, decode on the receivers.
#
# If new_payload_latency ~= the internal execution+root total, there is no hidden ingestion cost
# and IPC cannot help. If it is much larger, the delta is JSON transport/decode and IPC is worth
# testing.
#
#   ./height-decomp.sh              # 90s window on val1
#   VAL=2 WINDOW=120 ./height-decomp.sh
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"; cd "$REPO"
VAL=${VAL:-1}
WINDOW=${WINDOW:-90}

VAL=$VAL WINDOW=$WINDOW python3 - <<'PY'
import json,os,time,urllib.request
V=int(os.environ["VAL"]); W=int(os.environ["WINDOW"])
def met():
    t=urllib.request.urlopen(f"http://127.0.0.1:{19001+(V-1)*100}",timeout=10).read().decode()
    d={}
    for line in t.splitlines():
        if line.startswith("#") or " " not in line: continue
        k,_,v=line.rpartition(" ")
        try: d[k]=float(v)
        except ValueError: pass
    return d
def rpc(m,p=None):
    r=urllib.request.Request(f"http://127.0.0.1:{19545+(V-1)*100}",
        data=json.dumps({"jsonrpc":"2.0","id":1,"method":m,"params":p or []}).encode(),
        headers={"content-type":"application/json"})
    return json.load(urllib.request.urlopen(r,timeout=15))["result"]
B="reth_consensus_engine_beacon_"
KEYS={"np":B+"new_payload_latency","ins":B+"block_insert_total_duration",
      "fcu":B+"forkchoice_updated_latency","gap":B+"forkchoice_updated_new_payload_time_diff",
      "exec":"reth_sync_execution_execution_histogram",
      "root":"reth_sync_block_validation_state_root_histogram",
      "pers":"reth_consensus_engine_persistence_save_blocks_duration_seconds"}
def snap():
    m=met(); s={k:(m.get(v+"_sum",0.0),m.get(v+"_count",0.0)) for k,v in KEYS.items()}
    s["h"]=int(rpc("eth_blockNumber"),16); s["t"]=time.time(); return s
a=snap(); time.sleep(W); b=snap()
n=b["h"]-a["h"]; dt=b["t"]-a["t"]
if n<=0: print("no blocks in window"); raise SystemExit
tx=0
for h in range(a["h"]+1,b["h"]+1):
    try: tx+=len(rpc("eth_getBlockByNumber",[hex(h),False])["transactions"])
    except Exception: pass
per=lambda k: ((b[k][0]-a[k][0])/max(b[k][1]-a[k][1],1))*1000.0
height=dt/n*1000.0
np_,ins,fcu,gap = per("np"),per("ins"),per("fcu"),per("gap")
ex,rt,ps = per("exec"),per("root"),per("pers")
rest = height-(np_+gap+fcu)
print(f"val{V}: {n} blocks in {dt:.0f}s, {tx/n:.0f} txs/blk, height {height:.0f} ms\n")
print(f"  {'newPayload (EL works)':<34}{np_:8.1f} ms  {100*np_/height:5.1f}%")
print(f"  {'  of which execution':<34}{ex:8.1f} ms")
print(f"  {'  of which state root':<34}{rt:8.1f} ms")
print(f"  {'  unaccounted in newPayload':<34}{np_-ex-rt:8.1f} ms   <- decode/ingestion if large")
print(f"  {'newPayload -> FCU (EL IDLE)':<34}{gap:8.1f} ms  {100*gap/height:5.1f}%   CL voting round")
print(f"  {'forkchoiceUpdated (EL works)':<34}{fcu:8.1f} ms  {100*fcu/height:5.1f}%")
print(f"  {'remainder (EL IDLE)':<34}{rest:8.1f} ms  {100*rest/height:5.1f}%   build+SSZ+stream")
print(f"\n  EL busy   {np_+fcu:8.1f} ms  {100*(np_+fcu)/height:5.1f}%")
print(f"  EL idle   {gap+rest:8.1f} ms  {100*(gap+rest)/height:5.1f}%")
print(f"  (persistence {ps:.1f} ms/blk, async — overlaps the idle time above)")
PY
