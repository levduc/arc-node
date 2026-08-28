#!/bin/bash
# V7 lean overlay: fresh lean nodes on all 4 machines (val1 local), then inject
# ARC_PAYMENT_LEAN_* env into every CL and recreate — lean nodes FIRST (boot-park).
set -uo pipefail
cd /home/papaduck/arc-node-paymentlane
LOCAL_TS=100.124.148.61
declare -A RHOST=( [2]=ginnythui [3]=papaduck [4]=papaduck-alien2 )
declare -A RTS=( [2]=100.85.150.119 [3]=100.70.62.92 [4]=100.86.97.40 )
ALL_IPS=($LOCAL_TS ${RTS[2]} ${RTS[3]} ${RTS[4]})
PEERSCSV=$(printf 'http://%s:8560,' "${ALL_IPS[@]}"); PEERSCSV=${PEERSCSV%,}
PAR=${1:-0}   # 0 = serial, 1 = parallel recovery
BUDGET=${2:-225000000}
say(){ echo "[$(date +%H:%M:%S)] lean-up: $*"; }

# --- local node (val1): fresh datadir per mode change is NOT wanted mid-chain;
# only wipe if WIPE=1 (first boot)
pid=$(ss -ltnp 2>/dev/null | grep ':8560 ' | grep -oP 'pid=\K[0-9]+' | head -1)
[ -n "$pid" ] && kill -9 $pid 2>/dev/null
[ "${WIPE:-0}" = 1 ] && rm -rf /home/papaduck/lean-lane/val1
sleep 1
PEERS1=$(printf 'http://%s:8560,' "${RTS[2]}" "${RTS[3]}" "${RTS[4]}"); PEERS1=${PEERS1%,}
LPV=0; [ "$PAR" = 1 ] && LPV=1
LEAN_PARALLEL_RECOVERY=$LPV setsid nice -n 5 target/release/lean-lane-node run --datadir /home/papaduck/lean-lane/val1 \
  --port 8560 --bind 0.0.0.0 --chain-id 1338 --shim --peers "$PEERS1" \
  --fund-file /home/papaduck/lean-fund.txt --fund-balance 10000000000000000000 >> /tmp/v7-lean-1.log 2>&1 &
disown
# --- remotes: kill, (wipe), relaunch — all fast, inside the CL 15s retry budget
for n in 2 3 4; do
  ( h=${RHOST[$n]}
    PEERS=""
    for ip in "${ALL_IPS[@]}"; do [ "$ip" != "${RTS[$n]}" ] && PEERS="${PEERS}http://$ip:8560,"; done
    ENVP="LEAN_PARALLEL_RECOVERY=0 "; [ "$PAR" = 1 ] && ENVP="LEAN_PARALLEL_RECOVERY=1 "
    W=""; [ "${WIPE:-0}" = 1 ] && W="rm -rf /home/papaduck/lean-lane/val$n;"
    # kill and launch MUST be separate ssh calls: pkill -f matches the shell
    # carrying the launch text (CLAUDE.md landmine; cost arm1 of V7)
    timeout 40 tailscale ssh papaduck@$h "pkill -9 -f 'lean-lane-node ru[n]' 2>/dev/null; true" 2>/dev/null
    timeout 75 tailscale ssh papaduck@$h "cd /home/papaduck && $W mkdir -p lean-lane && (setsid nohup env $ENVP nice -n 5 ./lean-lane-node run --datadir /home/papaduck/lean-lane/val$n --port 8560 --bind 0.0.0.0 --chain-id 1338 --shim --peers '${PEERS%,}' --fund-file /home/papaduck/lean-fund.txt --fund-balance 10000000000000000000 >> lean-v7.log 2>&1 < /dev/null &); true" 2>/dev/null ) &
done
wait
# verify all 4 respond (EFFECT check)
ok=0; T=$(date +%s)
while [ $(( $(date +%s) - T )) -lt 240 ]; do
  ok=0
  for ip in "${ALL_IPS[@]}"; do
    curl -s -m5 -X POST http://$ip:8560 -H 'content-type: application/json' \
      --data '{"jsonrpc":"2.0","id":1,"method":"arc_getHead","params":[]}' 2>/dev/null | grep -q commitment && ok=$((ok+1))
  done
  [ $ok -ge 4 ] && break; sleep 8
done
say "lean nodes up: $ok/4 (par=$PAR)"; [ $ok -ge 4 ] || exit 1

if [ "${ENVINJECT:-0}" = 1 ]; then
  say "injecting CL env (budget=$BUDGET) + recreating CLs"
  python3 - "$BUDGET" "$PEERSCSV" <<'PY'
import re,sys
budget, peers = sys.argv[1], sys.argv[2]
p='.quake/soak4/compose.yaml'; s=open(p).read()
if 'ARC_PAYMENT_LEAN_LANE' not in s:
    a="ARC_VALUE_SYNC_BATCH_SIZE: '3'"
    assert a in s, "env anchor"
    s=s.replace(a, a+f"\n      ARC_PAYMENT_LEAN_LANE: '1'\n      ARC_PAYMENT_LEAN_RPC: 'http://172.17.0.1:8560'\n      ARC_PAYMENT_LEAN_BUDGET_GAS: '{budget}'\n      ARC_PAYMENT_LEAN_PEER_RPCS: '{peers}'",1)
else:
    s=re.sub(r"BUDGET_GAS: '\d+'", f"BUDGET_GAS: '{budget}'", s)
open(p,'w').write(s)
PY
  docker compose -f .quake/soak4/compose.yaml up -d --force-recreate validator1_cl > /dev/null 2>&1
  for n in 2 3 4; do
    h=${RHOST[$n]}
    timeout 90 tailscale ssh papaduck@$h "python3 - <<'PY'
import re
budget='$BUDGET'; peers='$PEERSCSV'
p='/home/papaduck/arc-fleet/soak4/compose-val$n.yaml'; s=open(p).read()
if 'ARC_PAYMENT_LEAN_LANE' not in s:
    a=\"ARC_VALUE_SYNC_BATCH_SIZE: '3'\"
    assert a in s
    s=s.replace(a, a+\"\\n      ARC_PAYMENT_LEAN_LANE: '1'\\n      ARC_PAYMENT_LEAN_RPC: 'http://172.17.0.1:8560'\\n      ARC_PAYMENT_LEAN_BUDGET_GAS: '\"+budget+\"'\\n      ARC_PAYMENT_LEAN_PEER_RPCS: '\"+peers+\"'\",1)
else:
    s=re.sub(r\"BUDGET_GAS: '\\d+'\", \"BUDGET_GAS: '\"+budget+\"'\", s)
open(p,'w').write(s)
PY
docker compose -f /home/papaduck/arc-fleet/soak4/compose-val$n.yaml up -d --force-recreate validator${n}_cl > /dev/null 2>&1; docker exec validator${n}_cl env | grep -o 'BUDGET_GAS=[0-9]*'" 2>/dev/null | tail -1 | xargs echo "  val$n:"
  done
  docker exec validator1_cl env | grep -o 'BUDGET_GAS=[0-9]*' | xargs echo "  val1:"
fi
say done
