import json, subprocess, sys
HOSTS={1:("127.0.0.1","ginny-alienware"),2:("100.85.150.119","ginnythui"),
       3:("100.70.62.92","papaduck"),4:("100.86.97.40","papaduck-alien2")}
def get(url):
    try: return subprocess.run(["curl","-s","-m","12",url],capture_output=True,text=True,timeout=15).stdout
    except Exception: return ""
def rpc(host,port,method,params):
    out=get_post(f"http://{host}:{port}",method,params); return out
def get_post(url,method,params):
    import subprocess,json
    d=json.dumps({"jsonrpc":"2.0","id":1,"method":method,"params":params})
    try:
        r=subprocess.run(["curl","-s","-m","15","-X","POST",url,"-H","content-type: application/json","--data",d],
                         capture_output=True,text=True,timeout=20).stdout
        return json.loads(r).get("result")
    except Exception: return None
def hist(metrics,name):
    s=c=0.0
    for ln in metrics.splitlines():
        if ln.startswith(name+"_sum"): s=float(ln.split()[1])
        elif ln.startswith(name+"_count"): c=float(ln.split()[1])
    return (s/c*1000 if c else 0.0, int(c))

print(f"{'':22} {'EVM lane (MPT)':>30}   {'PAY lane (SALT)':>30}")
print(f"{'machine':22} {'blk':>7}{'root_ms':>9}{'blocks':>8}   {'blk':>7}{'root_ms':>9}{'blocks':>8}")
evm_roots={}; pay_roots={}
for n,(ip,name) in HOSTS.items():
    e_rpc=8545+(n-1)*100; p_rpc=19545+(n-1)*100
    e_met=9001+(n-1)*100;  p_met=19001+(n-1)*100
    eb=get_post(f"http://{ip}:{e_rpc}","eth_blockNumber",[]) 
    pb=get_post(f"http://{ip}:{p_rpc}","eth_blockNumber",[])
    em=get(f"http://{ip}:{e_met}/metrics"); pm=get(f"http://{ip}:{p_met}/metrics")
    er,ec=hist(em,"reth_sync_block_validation_state_root_histogram")
    pr,pc=hist(pm,"reth_sync_block_validation_state_root_histogram")
    ebn=int(eb,16) if eb else None; pbn=int(pb,16) if pb else None
    print(f"{name:22} {str(ebn):>7}{er:>9.3f}{ec:>8}   {str(pbn):>7}{pr:>9.3f}{pc:>8}")
    if ebn: evm_roots[n]=ebn
    if pbn: pay_roots[n]=pbn

# divergence check at a settled height on BOTH lanes
print()
for lane,ports,heights in (("EVM (MPT)",lambda n:8545+(n-1)*100,evm_roots),
                           ("PAY (SALT)",lambda n:19545+(n-1)*100,pay_roots)):
    if len(heights)<4: print(f"{lane}: only {len(heights)}/4 responding — cannot settle"); continue
    h=min(heights.values())-5
    hexh=hex(h); roots=set(); detail=[]
    for n,(ip,name) in HOSTS.items():
        b=get_post(f"http://{ip}:{ports(n)}","eth_getBlockByNumber",[hexh,False])
        r=b.get("stateRoot") if b else None
        roots.add(r); detail.append((name,r))
    ok = len(roots)==1 and None not in roots
    print(f"{lane} @ block {h}: {'✅ ALL 4 AGREE' if ok else '❌ DIVERGENCE'}  root={list(roots)[0] if ok else roots}")
