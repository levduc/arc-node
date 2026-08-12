import json,sys,time,urllib.request
run=sys.argv[1]
drain_gas_m=int(sys.argv[2]) if len(sys.argv)>2 else None
deadline_s=int(sys.argv[3]) if len(sys.argv)>3 else 75
def rpc(m,p):
    r=urllib.request.Request("http://127.0.0.1:19545",data=json.dumps({"jsonrpc":"2.0","id":1,"method":m,"params":p}).encode(),headers={"content-type":"application/json"})
    return json.load(urllib.request.urlopen(r,timeout=10))["result"]
h=int(rpc("eth_blockNumber",[]),16)
events=[]
deadline=time.time()+deadline_s
while time.time()<deadline:
    time.sleep(0.4)
    h2=int(rpc("eth_blockNumber",[]),16)
    for x in range(h+1,h2+1):
        b=rpc("eth_getBlockByNumber",[hex(x),False])
        gl=int(b["gasLimit"],16)
        full=int(b["gasUsed"],16) >= 0.95*gl
        at_size = (not drain_gas_m) or abs(gl/1e6 - drain_gas_m) <= 1
        # transition guard: blocks sealed at the FILL gas limit (25M) can be 100% full and land
        # after the stop; only blocks at the drain size are capacity evidence
        events.append((time.time(),x,full and at_size,len(b["transactions"]),at_size))
    h=h2
    try: stop_ts=float(open(run+"/stop-ts").read().strip())
    except Exception: stop_ts=None
    # end-of-drain = a NON-full block AT the drain size. A wrong-size transition block must not
    # trigger this (that early-exit cost a 1G run: the first 1G block took ~8s to land and the
    # sampler had already quit on a 25M straggler).
    if stop_ts and events and events[-1][0]>stop_ts+3 and events[-1][4] and not events[-1][2]:
        break
try: stop_ts=float(open(run+"/stop-ts").read().strip())
except Exception: stop_ts=None
cap={}
if stop_ts:
    post=[e for e in events if e[0]>stop_ts+0.5 and e[2]]
    if len(post)>=3:
        dt=post[-1][0]-post[0][0]; n=len(post); txs=sum(e[3] for e in post)
        if dt>0:
            cap={"capacity_blocks":n,"capacity_ms":round(dt/(n-1)*1000),
                 "capacity_tps":round((txs-txs/n)/dt),
                 "capacity_txs_per_block":round(txs/n)}
            if drain_gas_m: cap["capacity_gas_m"]=drain_gas_m
json.dump(cap,open(run+"/capacity.json","w"))
print("capacity:",cap)
