#!/usr/bin/env bash
# DEMO: same 4-machine empty-chain fleet as demo-fleet-empty.sh, but validator3's data
# lives on papaduck's FAST NVMe (/mnt/blockchain.ssd) instead of its home disk.
#
# Why: measured fsync latency — papaduck home disk 30.9 ms vs /mnt/blockchain.ssd 1.5 ms
# (20x). The dashboard's per-validator "persistence" row showed v3 at ~3,700 ms/block on
# the home disk; this run demonstrates the same machine dropping to the fleet's ~100 ms
# class purely by moving the datadir. Same software, same network — only the disk changes.
#
#   ./demo-fleet-empty-fastssd.sh start | stop | status
set -uxo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$REPO"
SCEN=soak4; RUN=/tmp/dualel-fleet-fastssd; mkdir -p "$RUN"
LBASE="$REPO/.quake/$SCEN"
LOCAL_TS=100.124.148.61
declare -A RHOST=( [2]=ginnythui [3]=papaduck [4]=papaduck-alien2 )
declare -A RTS=( [2]=100.85.150.119 [3]=100.70.62.92 [4]=100.86.97.40 )
# per-host data roots: papaduck (val3) -> fast NVMe; others -> home
declare -A RPARENT=( [2]=/home/papaduck/arc-fleet [3]=/mnt/blockchain.ssd/arc-fleet [4]=/home/papaduck/arc-fleet )
declare -A RB=( [2]=/home/papaduck/arc-fleet/$SCEN [3]=/mnt/blockchain.ssd/arc-fleet/$SCEN [4]=/home/papaduck/arc-fleet/$SCEN )
PAY_PORT(){ echo $((19545+($1-1)*100)); }
tss(){ local h=$1; shift; timeout "${TSS_TMO:-120}" tailscale ssh "$h" "$@"; }

setup_env(){ export NVM_DIR="$HOME/.nvm"; [ -s "$NVM_DIR/nvm.sh" ] && . "$NVM_DIR/nvm.sh" >/dev/null 2>&1; nvm use 22 >/dev/null 2>&1 || true; export PATH="$HOME/.foundry/bin:$HOME/.cargo/bin:$PATH"; }

payment_el_cmd(){ # payment_el_cmd <n> <base>
  local n=$1 base=$2
  local rpc=$(PAY_PORT $n) ws=$((19546+(n-1)*100)) auth=$((19551+(n-1)*100)) met=$((19001+(n-1)*100)) p2p=$((30410+n))
  echo "docker rm -f validator${n}_el_pay 2>/dev/null; docker run -d --name validator${n}_el_pay --network arc_testnet_host-access \
  --entrypoint /app/assets/entrypoint_el.sh \
  -v $base/validator${n}/reth-pay:/data/reth/execution-data -v $base/assets:/app/assets \
  -p ${rpc}:8545 -p ${ws}:8546 -p ${auth}:8551 -p ${met}:9001 -p ${p2p}:30303 \
  arc_execution:latest \
  node --datadir=/data/reth/execution-data --chain=/app/assets/genesis.json \
  --http --http.addr=0.0.0.0 --http.port=8545 --http.corsdomain='*' --http.api=eth,net,web3,txpool,debug,admin \
  --ws --ws.addr=0.0.0.0 --ws.port=8546 --ws.origins='*' --ws.api=eth,net,web3,txpool \
  --authrpc.addr=0.0.0.0 --authrpc.port=8551 --authrpc.jwtsecret=/app/assets/payment-jwt.hex \
  --metrics=0.0.0.0:9001 --disable-discovery --ipcdisable --port 30303 \
  --arc.builder.deadline=500 --arc.builder.wait-for-payload=true --txpool.nolocals \
  --txpool.pending-max-count=200000 --txpool.queued-max-count=200000 && docker network connect arc_testnet_default validator${n}_el_pay"
}

start(){
  setup_env
  echo "==> [0/9] clean everywhere"
  pkill -x spammer 2>/dev/null; rm -f "$RUN/spam.stop"
  ids=$(docker ps -aq --filter name=validator); [ -n "$ids" ] && docker rm -f $ids
  docker run --rm -v "$REPO/.quake":/q --user root alpine rm -rf /q/$SCEN
  for n in 2 3 4; do tss "${RHOST[$n]}" "docker rm -f validator1_cl validator1_el validator1_el_pay validator2_cl validator2_el validator2_el_pay validator3_cl validator3_el validator3_el_pay validator4_cl validator4_el validator4_el_pay 2>/dev/null; rm -rf ${RB[$n]} 2>/dev/null || docker run --rm -v ${RPARENT[$n]}:/f --user root alpine rm -rf /f/$SCEN; mkdir -p ${RB[$n]}; docker run --rm -v ${RPARENT[$n]}:/f --user root alpine chown -R \$(id -u):\$(id -g) /f; true" || true; done

  echo "==> [1/9] generate testnet locally (quake)"
  target/release/quake -f "crates/quake/scenarios/${SCEN}.toml" start -e 1000 --monitoring false --force >"$RUN/quake.log" 2>&1 || true
  [ "$(docker ps --format '{{.Names}}' | grep -cE 'validator[0-9]+_(cl|el)$')" -ge 8 ] || { echo "!! quake start failed"; exit 1; }
  docker rm -f $(docker ps -aq --filter name=validator) >/dev/null

  echo "==> [2/9] payment lane uses the PLAIN genesis (no preseed; ELs auto-init on boot)"
  cp assets/localdev/payment-jwt.hex "$LBASE/assets/" 2>/dev/null || true
  for i in 1 2 3 4; do rm -rf "$LBASE/validator$i/reth-pay"; mkdir -p "$LBASE/validator$i/reth-pay"; done
  echo "==> [3/9] fleet compose surgery"
  python3 experiments/dual-el/fleet/gen-fleet.py

  echo "==> [4/9] ship trees to remotes (parallel); val3's compose repointed to the fast NVMe"
  for n in 2 3 4; do
    ( tar -C "$LBASE" -czf - assets "validator$n" compose-val$n.yaml | TSS_TMO=900 tss "${RHOST[$n]}" "tar -C ${RB[$n]} -xzf -" \
      && echo "shipped val$n -> ${RHOST[$n]} (${RB[$n]})" ) &
  done; wait
  # gen-fleet writes home-disk paths into the compose; repoint val3's to the fast mount
  tss papaduck "sed -i 's|/home/papaduck/arc-fleet|/mnt/blockchain.ssd/arc-fleet|g' ${RB[3]}/compose-val3.yaml && grep -c blockchain.ssd ${RB[3]}/compose-val3.yaml"

  echo "==> [5/9] start validator1 locally + val2-4 remotely"
  docker compose -f "$LBASE/compose.yaml" up -d validator1_cl validator1_el
  for n in 2 3 4; do tss "${RHOST[$n]}" "docker compose -f ${RB[$n]}/compose-val$n.yaml up -d"; done

  echo "==> [6/9] payment ELs (val3's datadir on the fast NVMe)"
  eval "$(payment_el_cmd 1 "$LBASE")"
  for n in 2 3 4; do tss "${RHOST[$n]}" "$(payment_el_cmd $n "${RB[$n]}")"; done
  for n in 1 2 3 4; do
    host=127.0.0.1; [ $n -ge 2 ] && host=${RTS[$n]}
    for t in $(seq 1 60); do curl -s -m3 -X POST "http://$host:$(PAY_PORT $n)" -H 'content-type: application/json' --data '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' | grep -q result && break; sleep 10; done
  done
  tss papaduck-alien2 "docker update --memory 11g --memory-swap 11g validator4_el_pay" || true

  echo "==> [7/9] payment gossip mesh over tailscale"
  declare -A EN
  for n in 1 2 3 4; do
    host=127.0.0.1; ts=$LOCAL_TS; [ $n -ge 2 ] && { host=${RTS[$n]}; ts=${RTS[$n]}; }
    pub=$(curl -s -m4 -X POST "http://$host:$(PAY_PORT $n)" -H 'content-type: application/json' \
      --data '{"jsonrpc":"2.0","id":1,"method":"admin_nodeInfo","params":[]}' \
      | python3 -c "import sys,json;print(json.load(sys.stdin)['result']['enode'].split('@')[0])" 2>/dev/null)
    EN[$n]="${pub}@${ts}:$((30410+n))"
  done
  for n in 1 2 3 4; do
    host=127.0.0.1; [ $n -ge 2 ] && host=${RTS[$n]}
    for j in 1 2 3 4; do [ $n -ne $j ] && curl -s -m4 -X POST "http://$host:$(PAY_PORT $n)" -H 'content-type: application/json' \
      --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"admin_addPeer\",\"params\":[\"${EN[$j]}\"]}" >/dev/null; done
  done

  echo "==> [8/9] NO load started. Flood separately:"
  echo "    ./experiments/dual-el/fleet/spam-fleet.sh start   (stop|status; rates via env)"
  echo "==> [9/9] fleet dashboard"
  python3 -c "import json;json.dump({'val2':'${RTS[2]}','val3':'${RTS[3]}','val4':'${RTS[4]}'},open('$RUN/fleet-endpoints.json','w'))"
  old=$(ss -ltnp 2>/dev/null | grep ':8080 ' | grep -oP 'pid=\K[0-9]+' | head -1); [ -n "$old" ] && kill "$old" 2>/dev/null
  DUALEL_FLEET="$RUN/fleet-endpoints.json" nohup python3 experiments/dual-el/dashboard.py >"$RUN/dashboard.log" 2>&1 & echo $! >"$RUN/dash.pid"

  echo "==> VERIFY"
  sleep 60; verify
  echo "WATCH: dashboard 'persistence' row, v3 column — home disk gave ~900-3,700 ms/blk under load;"
  echo "       on /mnt/blockchain.ssd it should sit in the fleet's normal ~50-200 ms class."
}

verify(){
  ok=1
  for n in 1 2 3 4; do
    host=127.0.0.1; [ $n -ge 2 ] && host=${RTS[$n]}
    e=$((8545+(n-1)*100)); p=$(PAY_PORT $n)
    eh=$(curl -s -m5 -X POST "http://$host:$e" -H 'content-type: application/json' --data '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' | python3 -c "import sys,json;print(int(json.load(sys.stdin)['result'],16))" 2>/dev/null)
    ph=$(curl -s -m5 -X POST "http://$host:$p" -H 'content-type: application/json' --data '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' | python3 -c "import sys,json;print(int(json.load(sys.stdin)['result'],16))" 2>/dev/null)
    echo "val$n@$host evm=${eh:-X} pay=${ph:-X}"; { [ -z "$eh" ] || [ -z "$ph" ]; } && ok=0
  done
  [ "$ok" = 1 ] && echo "✅ FLEET VERIFIED (4 machines, empty chain, val3 on fast NVMe)" || echo "❌ FLEET VERIFY FAILED — fallback: ./experiments/dual-el/demo-fleet-empty.sh start"
}

stop(){
  touch "$RUN/spam.stop" 2>/dev/null; [ -f "$RUN/spam.pid" ] && kill "$(cat "$RUN/spam.pid")" 2>/dev/null; pkill -x spammer 2>/dev/null
  [ -f "$RUN/dash.pid" ] && kill "$(cat "$RUN/dash.pid")" 2>/dev/null
  ids=$(docker ps -aq --filter name=validator); [ -n "$ids" ] && docker rm -f $ids
  for n in 2 3 4; do tss "${RHOST[$n]}" "docker rm -f validator1_cl validator1_el validator1_el_pay validator2_cl validator2_el validator2_el_pay validator3_cl validator3_el validator3_el_pay validator4_cl validator4_el validator4_el_pay 2>/dev/null; docker network rm arc_testnet_default arc_testnet_host-access 2>/dev/null; rm -rf ${RB[$n]} 2>/dev/null || docker run --rm -v ${RPARENT[$n]}:/f --user root alpine rm -rf /f/$SCEN; true" || true; done
  # papaduck: also clean any leftovers on the home-disk location from other variants
  tss papaduck "rm -rf /home/papaduck/arc-fleet/$SCEN 2>/dev/null; true" || true
  docker run --rm -v "$REPO/.quake":/q --user root alpine rm -rf /q/$SCEN
  for net in arc_testnet_default arc_testnet_host-access arc_testnet_blockscout; do docker network rm $net >/dev/null 2>&1 || true; done
  echo "✅ fleet stopped and cleaned (all 4 machines, both papaduck locations)"
}

status(){ verify; }

case "${1:-}" in start) start;; stop) stop;; status) status;; verify) verify;;
  clean) exec bash "$(dirname "${BASH_SOURCE[0]}")/clean-fleet.sh";;
  *) echo "usage: $0 {start|stop|status|clean}"; exit 1;; esac
