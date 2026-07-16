#!/usr/bin/env bash
# Revive one validator's payment EL (and unstick its CL) on the fleet, without touching
# the rest of the chain. Handles the full failure ladder we've met in practice:
#   - pay EL OOM (exit 137) under its memory clamp  -> uncapped boot, then clamp
#   - lost unpersisted blocks + CL crash-loop on 'Payload validation failed' (finding #7)
#       -> engine_forkchoiceUpdatedV3(canonical head) so reth p2p-backfills from gossip
#          peers, then CL restart; sync then converges
#   - dropped gossip peering after restart -> re-addPeer from the healthy nodes
#
#   ./revive-val.sh [N]     (default 4 = papaduck-alien2)
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"; cd "$REPO"
N=${1:-4}
declare -A HOSTS=( [1]=127.0.0.1 [2]=100.85.150.119 [3]=100.70.62.92 [4]=100.86.97.40 )
declare -A SSHH=( [2]=ginnythui [3]=papaduck [4]=papaduck-alien2 )
declare -A CLAMP=( [1]=0 [2]=0 [3]=0 [4]=11g )   # only alien2 (15G) needs a clamp
TS=${HOSTS[$N]}; RPC=$((19545+(N-1)*100)); AUTH=$((19551+(N-1)*100)); P2P=$((30410+N))
JWT_FILE="$REPO/.quake/soak4/assets/payment-jwt.hex"
run(){ if [ "$N" = 1 ]; then bash -c "$1"; else timeout 90 tailscale ssh "${SSHH[$N]}" "$1"; fi }

echo "==> [1/6] boot pay EL uncapped"
run "docker update --memory 60g --memory-swap 60g validator${N}_el_pay 2>/dev/null; docker start validator${N}_el_pay"
for t in $(seq 1 60); do curl -s -m3 -X POST "http://$TS:$RPC" -H 'content-type: application/json' --data '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' | grep -q result && break; sleep 10; done
echo "    rpc up (~$((t*10))s)"
if [ "${CLAMP[$N]}" != 0 ]; then echo "==> [2/6] clamp ${CLAMP[$N]}"; run "docker update --memory ${CLAMP[$N]} --memory-swap ${CLAMP[$N]} validator${N}_el_pay"; else echo "==> [2/6] no clamp for this host"; fi

echo "==> [3/6] re-mesh gossip (healthy nodes dial this EL)"
pub=$(curl -s -m5 -X POST "http://$TS:$RPC" -H 'content-type: application/json' --data '{"jsonrpc":"2.0","id":1,"method":"admin_nodeInfo","params":[]}' | python3 -c "import sys,json;print(json.load(sys.stdin)['result']['enode'].split('@')[0])" 2>/dev/null)
for j in 1 2 3 4; do [ "$j" = "$N" ] && continue
  curl -s -m4 -X POST "http://${HOSTS[$j]}:$((19545+(j-1)*100))" -H 'content-type: application/json' \
    --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"admin_addPeer\",\"params\":[\"${pub}@${TS}:${P2P}\"]}" >/dev/null
done

echo "==> [4/6] finding-#7 heal: forkchoice to canonical head (p2p backfill of any lost blocks)"
REF=1; [ "$N" = 1 ] && REF=2
HEAD=$(curl -s -m4 -X POST "http://${HOSTS[$REF]}:$((19545+(REF-1)*100))" -H 'content-type: application/json' --data '{"jsonrpc":"2.0","id":1,"method":"eth_getBlockByNumber","params":["latest",false]}' | python3 -c "import sys,json;print(json.load(sys.stdin)['result']['hash'])")
python3 - "$HEAD" "$TS" "$AUTH" "$JWT_FILE" <<'PY'
import json,urllib.request,hmac,hashlib,base64,time,sys
h,ts,auth,jf=sys.argv[1:5]
secret=bytes.fromhex(open(jf).read().strip())
def b64u(b): return base64.urlsafe_b64encode(b).rstrip(b'=')
hd=b64u(json.dumps({"alg":"HS256","typ":"JWT"}).encode()); p=b64u(json.dumps({"iat":int(time.time())}).encode())
tok=(hd+b'.'+p+b'.'+b64u(hmac.new(secret,hd+b'.'+p,hashlib.sha256).digest())).decode()
r=urllib.request.Request(f'http://{ts}:{auth}',
  data=json.dumps({'jsonrpc':'2.0','id':1,'method':'engine_forkchoiceUpdatedV3','params':[{"headBlockHash":h,"safeBlockHash":h,"finalizedBlockHash":h},None]}).encode(),
  headers={'content-type':'application/json','Authorization':'Bearer '+tok})
print('   fcU:',json.load(urllib.request.urlopen(r,timeout=20))['result']['payloadStatus']['status'])
PY

echo "==> [5/6] restart CL (clears crash-loop / stale sync state)"
run "docker restart validator${N}_cl"

echo "==> [6/6] wait for convergence"
for t in $(seq 1 30); do sleep 20
  v=$(curl -s -m3 -X POST "http://$TS:$RPC" -H 'content-type: application/json' --data '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}'|python3 -c "import sys,json;print(int(json.load(sys.stdin)['result'],16))" 2>/dev/null)
  net=$(curl -s -m3 -X POST "http://${HOSTS[$REF]}:$((19545+(REF-1)*100))" -H 'content-type: application/json' --data '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}'|python3 -c "import sys,json;print(int(json.load(sys.stdin)['result'],16))" 2>/dev/null)
  echo "    val$N=$v net=$net"
  [ -n "$v" ] && [ -n "$net" ] && [ $((net-v)) -le 10 ] && { echo "✅ val$N CONVERGED ($v/$net)"; exit 0; }
done
echo "⚠ val$N not converged yet ($v/$net) — re-run step 4+5 or check docker logs validator${N}_cl"
exit 1
