#!/usr/bin/env bash
# One-shot payment-lane benchmark for the dashboard button — PROPER load methodology
# (see fleet/LOADING.md):
#   phase 1: GOVERNED sustained window (closed-loop --pool-target) -> measures the CHAIN with
#            live ingress. The old open-loop RATE=12000 collapsed the pool and measured the
#            spammer.
#   phase 2: capacity via PREFILL+DRAIN: blast the pool full, stop ALL intake, time the
#            full-block drain -> the chain's ceiling at this block size.
# Status contract unchanged (/tmp/pay-bench/bench-status.json); result gains capacity_* keys.
set -uo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUN=/tmp/pay-bench; mkdir -p "$RUN"
STATUS="$RUN/bench-status.json"
WINDOW=${WINDOW:-300}
S=${S:-4}; ACCTS=${ACCTS:-1000}; RATE=${RATE:-3500}
st(){ printf '%s\n' "$1" > "$STATUS"; }

GAS=$(python3 -c "
import json,urllib.request
r=urllib.request.Request('http://127.0.0.1:19545',data=json.dumps({'jsonrpc':'2.0','id':1,'method':'eth_getBlockByNumber','params':['latest',False]}).encode(),headers={'content-type':'application/json'})
print(int(json.load(urllib.request.urlopen(r,timeout=10))['result']['gasLimit'],16))")
PT=$(( GAS / 21000 * 3 )); [ "$PT" -lt 12000 ] && PT=12000

st "{\"state\":\"running\",\"phase\":\"starting governed load (pool target $PT)\",\"window_s\":$WINDOW}"
if ! POOL_TARGET=$PT S=$S ACCTS=$ACCTS RATE=$RATE DUR=$((WINDOW+240)) \
     "$DIR/fleet/spam-fleet-distributed.sh" start >"$RUN/bench.log" 2>&1; then
  st '{"state":"error","phase":"spam start failed (see /tmp/pay-bench/bench.log)"}'; exit 1
fi
st "{\"state\":\"running\",\"phase\":\"warming up (governed)\",\"window_s\":$WINDOW}"
sleep 60
st "{\"state\":\"running\",\"phase\":\"sustained window: ${WINDOW}s governed, live ingress\",\"window_s\":$WINDOW}"
rm -f "$RUN/result.json"
WINDOW=$WINDOW "$DIR/pay-throughput-bench.sh" measure >>"$RUN/bench.log" 2>&1 || true

st '{"state":"running","phase":"capacity: prefilling the pool"}'
# governed spammers are still running and hold the pool at target; briefly re-launch open to fill
"$DIR/fleet/spam-fleet-distributed.sh" stop >>"$RUN/bench.log" 2>&1 || true
pkill -x spammer 2>/dev/null || true
sleep 8
S=$S ACCTS=$ACCTS RATE=6000 DUR=180 "$DIR/fleet/spam-fleet-distributed.sh" start >>"$RUN/bench.log" 2>&1 || true
python3 -c "
import json,time,urllib.request
def pend():
    r=urllib.request.Request('http://127.0.0.1:19545',data=json.dumps({'jsonrpc':'2.0','id':1,'method':'txpool_status','params':[]}).encode(),headers={'content-type':'application/json'})
    s=json.load(urllib.request.urlopen(r,timeout=10))['result']
    f=lambda x:int(x,16) if isinstance(x,str) else int(x)
    return f(s['pending'])
t0=time.time()
while time.time()-t0<150:
    if pend()>45000: break
    time.sleep(8)" >>"$RUN/bench.log" 2>&1
"$DIR/fleet/spam-fleet-distributed.sh" stop >>"$RUN/bench.log" 2>&1 || true
pkill -x spammer 2>/dev/null || true

st '{"state":"running","phase":"capacity: draining (intake stopped)"}'
python3 - "$RUN" <<'PY'
import json,time,urllib.request
run=__import__('sys').argv[1]
def rpc(m,p):
    r=urllib.request.Request('http://127.0.0.1:19545',data=json.dumps({'jsonrpc':'2.0','id':1,'method':m,'params':p}).encode(),headers={'content-type':'application/json'})
    return json.load(urllib.request.urlopen(r,timeout=10))['result']
# walk new blocks with wall-clock stamps; a block counts while >=95% full
h=int(rpc('eth_blockNumber',[]),16)
t_first=None; t_last=None; n=0; txs=0; deadline=time.time()+45
while time.time()<deadline:
    time.sleep(0.5)
    h2=int(rpc('eth_blockNumber',[]),16)
    for x in range(h+1,h2+1):
        b=rpc('eth_getBlockByNumber',[hex(x),False])
        if int(b['gasUsed'],16) >= 0.95*int(b['gasLimit'],16):
            if t_first is None: t_first=time.time()
            t_last=time.time(); n+=1; txs+=len(b['transactions'])
        elif t_first is not None:
            deadline=0  # drain over
    h=h2
cap={}
if n>=3 and t_last and t_last>t_first:
    dt=t_last-t_first; per=dt/max(n-1,1)
    cap={"capacity_blocks":n,"capacity_ms":round(per*1000),
         "capacity_tps":round((txs-txs/n)/dt),"capacity_txs_per_block":round(txs/n)}
json.dump(cap,open(run+"/capacity.json","w"))
print("capacity:",cap)
PY

if [ -f "$RUN/result.json" ]; then
  python3 - "$RUN" > "$STATUS" <<'PY'
import json,sys
run=sys.argv[1]
r=json.load(open(run+"/result.json"))
try: r.update(json.load(open(run+"/capacity.json")))
except Exception: pass
print(json.dumps({"state":"done","result":r}))
PY
else
  st '{"state":"error","phase":"no result produced (see /tmp/pay-bench/bench.log)"}'
fi
