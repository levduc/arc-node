"""Drain test v2: with a pre-filled mempool and ALL spam stopped, block height is pure consensus
capacity -- fill time is zero because the transactions are already present.

v1's time attribution was WRONG: `dt*frac/span_blocks` reduces algebraically to the ALL-block
average, diluting full blocks with the fast empty blocks after backlog exhaustion, biasing LOW.
Iter-14's headline drains (208/385 ms) carried that bias (corrected 60M estimate ~237 ms from its
16-of-24-full composition). v2 samples the head every 1s and attributes time per interval,
keeping only intervals whose blocks were ALL >=95% full.

KNOWN CONFOUND (measured 2026-08-09): drain height is strongly CHAIN-AGE dependent -- a fresh
chain drained 60M at ~237 ms while the same chain after ~1.5 h of churn drained at 513 ms
(38-of-39 full, negligible dilution). Any drain-frontier sweep must therefore use FRESH chains
per point and report chain age alongside.
"""
import json,sys,time,urllib.request
LABEL=sys.argv[1]; GAS=int(sys.argv[2])
def rpc(m,p=None):
    r=urllib.request.Request("http://127.0.0.1:19545",
        data=json.dumps({"jsonrpc":"2.0","id":1,"method":m,"params":p or []}).encode(),
        headers={"content-type":"application/json"})
    return json.load(urllib.request.urlopen(r,timeout=15))["result"]
def pending():
    try:
        st=rpc("txpool_status")
        return int(st["pending"],16) if isinstance(st["pending"],str) else int(st["pending"])
    except Exception: return -1
p0=pending()
h_prev=int(rpc("eth_blockNumber"),16); t_prev=time.time(); t0=t_prev
intervals=[]
while True:
    time.sleep(1.0)
    h=int(rpc("eth_blockNumber"),16); t=time.time()
    if h>h_prev:
        intervals.append((t_prev,t,h_prev,h))
        h_prev,t_prev=h,t
    p=pending()
    if (p>=0 and p<3000) or t-t0>200:
        break
full_iv=[]
for (ta,tb,ha,hb) in intervals:
    gs=[]
    for x in range(ha+1,hb+1):
        b=rpc("eth_getBlockByNumber",[hex(x),False])
        gs.append((int(b["gasUsed"],16),len(b["transactions"])))
    if gs and all(g>=GAS*95//100 for g,_ in gs):
        full_iv.append((tb-ta,hb-ha,sum(n for _,n in gs)))
if len(full_iv)<3:
    print(f"[{LABEL}] insufficient full intervals ({len(full_iv)})"); sys.exit()
tot_t=sum(i[0] for i in full_iv); tot_b=sum(i[1] for i in full_iv); tot_tx=sum(i[2] for i in full_iv)
per=[i[0]/i[1]*1000 for i in full_iv]
print(f"[{LABEL}] {tot_b} FULL blocks / {tot_t:.1f}s (backlog {p0:,})  "
      f"height {tot_t*1000/tot_b:.0f} ms (spread {min(per):.0f}-{max(per):.0f})  tps {tot_tx/tot_t:,.0f}")
