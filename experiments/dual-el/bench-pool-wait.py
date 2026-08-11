import json,sys,time,urllib.request
# Fill-phase watcher: wait for the payment-lane pool to reach a deep backlog OR plateau.
# The plateau matters: reth's per-sender slot limit (--txpool.max-account-slots, default 16)
# hard-caps the pool at accounts*slots (800 accts * 16 = 12.8k on the default demo chain), so
# a fixed threshold would spin for the full timeout. Writes the final backlog to backlog.txt
# so run-bench.sh can size the drain block adaptively.
run=sys.argv[1] if len(sys.argv)>1 else "/tmp/pay-bench"
def pend():
    r=urllib.request.Request("http://127.0.0.1:19545",data=json.dumps({"jsonrpc":"2.0","id":1,"method":"txpool_status","params":[]}).encode(),headers={"content-type":"application/json"})
    s=json.load(urllib.request.urlopen(r,timeout=10))["result"]
    f=lambda x:int(x,16) if isinstance(x,str) else int(x)
    return f(s["pending"])
t0=time.time(); hist=[]
while time.time()-t0<180:
    p=pend()
    print("pool",p,flush=True)
    hist.append(p)
    if p>150000: break
    if len(hist)>=4 and p>5000:
        w=hist[-4:]
        if max(w)-min(w) < 0.03*max(w):
            print("plateau (per-sender slot ceiling)",flush=True); break
    time.sleep(6)
open(run+"/backlog.txt","w").write(str(hist[-1] if hist else 0))
