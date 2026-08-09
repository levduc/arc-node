#!/usr/bin/env bash
# Per-validator payload BUILD time — the ~20% of block height that speculative building would hide.
#
# WHY THIS MEASUREMENT: consensus-split.sh showed the proposer's payload build is essentially all of
# the non-vote-gap remainder and sits on the critical path (50M: build 96.6 ms of a 551 ms height;
# 100M: 167.4 of 829). Hiding it inside the previous height's vote gap -- where the EL is idle for
# 350-900 ms -- is the largest single win in any measurement so far. Before forking reth to build
# speculatively, three EXISTING flags attack the same cost and have never been tested:
#
#   --engine.share-execution-cache-with-payload-builder   give the builder the engine's cross-block
#       cache. reth warns: only if the node will not process payloads in parallel with building --
#       which a validator DOES (it validates other proposers' blocks), so this may hurt.
#   --engine.share-sparse-trie-with-payload-builder       state root concurrent with execution.
#       reth warns: engine and builder contend for the trie; newPayload blocks if a build is running.
#   --engine.suppress-persistence-during-build            defer persistence I/O during a build.
#
# Proposer selection is RoundRobin, so over a long enough window each validator builds ~1/4 of the
# blocks and `reth_arc_payload_total_duration_seconds` is directly comparable BETWEEN validators --
# but only against that machine's OWN baseline. Comparing treated-vs-untreated across different
# boxes is what produced a bogus "16% improvement" earlier in this project; run stock first.
#
#   ./build-time.sh            # one window, prints per-validator build time
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"; cd "$REPO"
export PATH="$HOME/.foundry/bin:$HOME/.cargo/bin:$PATH"
WINDOW=${WINDOW:-600}
LABEL=${LABEL:-run}

WINDOW=$WINDOW LABEL=$LABEL python3 - <<'PY'
import json,os,time,urllib.request
H={1:("ginny","127.0.0.1"),2:("ginnythui","100.85.150.119"),
   3:("papaduck","100.70.62.92"),4:("alien2","100.86.97.40")}
W=int(os.environ["WINDOW"]); LABEL=os.environ["LABEL"]
def rpc(v,m,p=None):
    r=urllib.request.Request(f"http://{H[v][1]}:{19545+(v-1)*100}",
        data=json.dumps({"jsonrpc":"2.0","id":1,"method":m,"params":p or []}).encode(),
        headers={"content-type":"application/json"})
    return json.load(urllib.request.urlopen(r,timeout=20))["result"]
def met(v):
    t=urllib.request.urlopen(f"http://{H[v][1]}:{19001+(v-1)*100}",timeout=10).read().decode()
    d={}
    for l in t.splitlines():
        if l.startswith("#") or " " not in l: continue
        k,_,val=l.rpartition(" ")
        try: d[k]=float(val)
        except ValueError: pass
    return d
BK="reth_arc_payload_total_duration_seconds"
NP="reth_consensus_engine_beacon_new_payload_latency"
def snap():
    o={}
    for v in H:
        try:
            m=met(v)
            o[v]=dict(bs=m.get(BK+"_sum",0.0), bc=m.get(BK+"_count",0.0),
                      ns=m.get(NP+"_sum",0.0), nc=m.get(NP+"_count",0.0))
        except Exception: o[v]=None
    o["h"]=int(rpc(1,"eth_blockNumber"),16); o["t"]=time.time(); return o
a=snap(); time.sleep(W); b=snap()
n=b["h"]-a["h"]; dt=b["t"]-a["t"]
print(f"\n[{LABEL}] {n} blocks in {dt/60:.1f} min — height {dt/n*1000:.0f} ms")
print(f"{'validator':<12} {'blocks built':>13} {'build ms':>10} {'newPayload ms':>14}")
tot_b=tot_c=0.0
for v in H:
    if not (a[v] and b[v]): print(f"{H[v][0]:<12} unreachable"); continue
    bc=b[v]["bc"]-a[v]["bc"]; bs=b[v]["bs"]-a[v]["bs"]
    nc=b[v]["nc"]-a[v]["nc"]; ns=b[v]["ns"]-a[v]["ns"]
    tot_b+=bs; tot_c+=bc
    bm = bs/bc*1000 if bc>0 else float('nan')
    nm = ns/nc*1000 if nc>0 else float('nan')
    print(f"{H[v][0]:<12} {bc:>13.0f} {bm:>9.1f} {nm:>13.1f}")
print(f"{'fleet mean':<12} {tot_c:>13.0f} {tot_b/max(tot_c,1)*1000:>9.1f}")
PY
