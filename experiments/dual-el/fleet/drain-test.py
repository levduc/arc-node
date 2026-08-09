"""Drain test: with a pre-filled mempool and ALL spam stopped, the chain's height is pure
consensus capacity — fill time is zero because the transactions are already there."""
import json,statistics,sys,time,urllib.request
LABEL=sys.argv[1]; GAS=int(sys.argv[2])
def rpc(m,p=None,port=19545):
    r=urllib.request.Request(f"http://127.0.0.1:{port}",
        data=json.dumps({"jsonrpc":"2.0","id":1,"method":m,"params":p or []}).encode(),
        headers={"content-type":"application/json"})
    return json.load(urllib.request.urlopen(r,timeout=15))["result"]
def pending():
    try:
        st=rpc("txpool_status")
        return int(st["pending"],16) if isinstance(st["pending"],str) else int(st["pending"])
    except Exception: return -1
# wait until the backlog is actually draining (spam stopped by caller)
p0=pending()
h0=int(rpc("eth_blockNumber"),16); t0=time.time()
rows=[]
while True:
    time.sleep(5)
    h=int(rpc("eth_blockNumber"),16); p=pending(); t=time.time()
    rows.append((t,h,p))
    if p >= 0 and p < 3000:  # backlog nearly gone
        break
    if t-t0 > 240:
        break
# analyse only FULL blocks during the drain
h1=rows[-1][1]
txs=[]; gasu=[]
for x in range(h0+1,h1+1):
    b=rpc("eth_getBlockByNumber",[hex(x),False])
    txs.append(len(b["transactions"])); gasu.append(int(b["gasUsed"],16))
full=[i for i,g in enumerate(gasu) if g >= GAS*95//100]
n=len(full); dt=rows[-1][0]-t0
if n<5:
    print(f"[{LABEL}] insufficient full blocks in drain ({n})"); sys.exit()
# height during the full-block stretch: blocks/sec over that span
# use the count of full blocks over the total drain time up to when fullness ended
last_full=max(full); first_full=min(full)
span_blocks=last_full-first_full+1
# time attribution: interpolate via rows
def height_at(t):
    for tt,hh,_ in rows:
        if tt>=t: return hh
    return rows[-1][1]
# simpler: average over full blocks = (span time)/(span blocks): find timestamps around the span
# approximate with block-count-weighted total time fraction
frac=(span_blocks)/(h1-h0)
span_time=dt*frac
ms=span_time*1000/span_blocks
tps=sum(txs[i] for i in full)/span_time
print(f"[{LABEL}] drain: {h1-h0} blocks total, {n} FULL ({statistics.mean([txs[i] for i in full]):,.0f} tx avg)  "
      f"start-backlog {p0:,}  height(full-stretch) {ms:.0f} ms  tps {tps:,.0f}")
