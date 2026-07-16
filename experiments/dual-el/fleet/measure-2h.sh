#!/usr/bin/env bash
# 2-hour measured run on the 4-machine 10M-preseed fleet (all fixes: val3 fast NVMe,
# 500ms builder deadline, local-feed spam). Produces per-lane, per-validator averages
# for state root, execution AND disk persistence from cumulative reth histograms.
#   ./measure-2h.sh            (start fleet + load, measure 2h, report)
#   DUR=7200 PAY_RATE=8000 EVM_RATE=6 ... env-tunable
# Results: /tmp/fleet2h-results.txt   Snapshots: /tmp/fleet2h-snaps.txt
set -uxo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"; cd "$REPO"
DUR=${DUR:-7200}
OUT=/tmp/fleet2h-results.txt; SNAPS=/tmp/fleet2h-snaps.txt; : > "$OUT"; : > "$SNAPS"
declare -A HOSTS=( [1]=127.0.0.1 [2]=100.85.150.119 [3]=100.70.62.92 [4]=100.86.97.40 )

snap(){ # snap <tag>: all validators, both lanes, three histograms
  for n in 1 2 3 4; do
    for base in 9001 19001; do
      port=$((base+(n-1)*100))
      curl -s -m5 "http://${HOSTS[$n]}:$port/metrics" \
        | grep -E "^reth_(sync_execution_execution_histogram|sync_block_validation_state_root_histogram|consensus_engine_persistence_save_blocks_duration_seconds)_(sum|count) " \
        | sed "s/^/$1 $n $port /"
    done
  done >> "$SNAPS"
}

echo "==> fleet up (10M preseed, val3 fast NVMe)"
bash experiments/dual-el/fleet/demo-fleet-fast.sh start
echo "==> load (local-feed: gossip fans out)"
PAY_RATE=${PAY_RATE:-8000} EVM_RATE=${EVM_RATE:-6} bash experiments/dual-el/fleet/spam-fleet.sh start
sleep 240   # settle: pools warm, cadence steady
snap base
date +%F_%T >> "$OUT"; echo "BASE taken; measuring ${DUR}s" >> "$OUT"
sleep "$DUR"
snap end

# sample avg tx/blk per lane (last 40 blocks, local node)
for spec in "8545 evm" "19545 pay"; do
  set -- $spec 2>/dev/null || true
  port=$(echo $spec | cut -d' ' -f1); lane=$(echo $spec | cut -d' ' -f2)
  python3 - "$port" "$lane" >> "$OUT" <<'PY'
import urllib.request, json, sys
port, lane = sys.argv[1], sys.argv[2]
def rpc(m,p):
    r=urllib.request.Request(f'http://127.0.0.1:{port}',data=json.dumps({'jsonrpc':'2.0','id':1,'method':m,'params':p}).encode(),headers={'content-type':'application/json'})
    return json.load(urllib.request.urlopen(r,timeout=6))['result']
h=int(rpc('eth_blockNumber',[]),16)
t=[len(rpc('eth_getBlockByNumber',[hex(n),False])['transactions']) for n in range(max(1,h-40),h)]
print(f"{lane}_txblk_avg {sum(t)/max(len(t),1):.0f} head {h}")
PY
done

python3 - >> "$OUT" <<'PY'
import collections
snaps=collections.defaultdict(dict)
for ln in open('/tmp/fleet2h-snaps.txt'):
    parts=ln.split()
    if len(parts)!=5: continue
    tag,n,port,metric,val=parts
    snaps[(tag,int(n),int(port))][metric]=float(val)
M={'exec':'reth_sync_execution_execution_histogram',
   'root':'reth_sync_block_validation_state_root_histogram',
   'persist':'reth_consensus_engine_persistence_save_blocks_duration_seconds'}
for lane,base in (('EVM',9001),('PAY',19001)):
    print(f"== {lane} lane (delta over the window, ms/block) ==")
    for label,pref in M.items():
        row=[]; vals=[]
        for n in (1,2,3,4):
            b=snaps.get(('base',n,base+(n-1)*100),{}); e=snaps.get(('end',n,base+(n-1)*100),{})
            try:
                ds=e[pref+'_sum']-b[pref+'_sum']; dc=e[pref+'_count']-b[pref+'_count']
                if dc>0:
                    v=ds/dc*1000.0; vals.append(v); row.append(f"v{n} {v:.2f}")
                else: row.append(f"v{n} —")
            except KeyError: row.append(f"v{n} ?")
        avg=f"{sum(vals)/len(vals):.2f}" if vals else "—"
        blocks=""
        try:
            b=snaps[('base',1,base)]; e=snaps[('end',1,base)]
            blocks=f" (n={int(e[pref+'_count']-b[pref+'_count'])} blocks)"
        except Exception: pass
        print(f"  {label:8s}: avg {avg} ms/blk [{' · '.join(row)}]{blocks}")
PY
echo "MEASURE2H DONE $(date +%F_%T)" >> "$OUT"
cat "$OUT"
