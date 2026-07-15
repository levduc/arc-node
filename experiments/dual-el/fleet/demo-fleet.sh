#!/usr/bin/env bash
# DEMO-DAY fleet runner: 4 validators on 4 tailscale machines, driven entirely from this box.
#   validator1 -> here (ginny-alienware)   validator2 -> ginnythui
#   validator3 -> papaduck                 validator4 -> papaduck-alien2 (15G: pay EL capped 11G)
# 10M preseeded payment lane + demo-bloat loads + fleet dashboard (http://localhost:8080).
#
#   ./demo-fleet.sh start | stop | status
# FALLBACK on demo day: ./experiments/dual-el/demo-empty.sh (single machine, clean chain).
#
# Prereqs (one-time, already done): tailscale ssh enabled on all 3 remotes; arc images shipped.
set -uxo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$REPO"
SCEN=soak4; RUN=/tmp/dualel-fleet; mkdir -p "$RUN"
LBASE="$REPO/.quake/$SCEN"; RBASE=/home/papaduck/arc-fleet/$SCEN
LOCAL_TS=100.124.148.61
declare -A RHOST=( [2]=ginnythui [3]=papaduck [4]=papaduck-alien2 )
declare -A RTS=( [2]=100.85.150.119 [3]=100.70.62.92 [4]=100.86.97.40 )
PAY_PORT(){ echo $((19545+($1-1)*100)); }   # rpc; ws=+1 auth=+6 metrics: 19001+100(i-1)
tss(){ local h=$1; shift; timeout "${TSS_TMO:-120}" tailscale ssh "$h" "$@"; }

setup_env(){ export NVM_DIR="$HOME/.nvm"; [ -s "$NVM_DIR/nvm.sh" ] && . "$NVM_DIR/nvm.sh" >/dev/null 2>&1; nvm use 22 >/dev/null 2>&1 || true; export PATH="$HOME/.foundry/bin:$HOME/.cargo/bin:$PATH"; }

payment_el_cmd(){ # payment_el_cmd <n> <base> -> docker run command string for valN payment EL
  local n=$1 base=$2
  local rpc=$(PAY_PORT $n) ws=$((19546+(n-1)*100)) auth=$((19551+(n-1)*100)) met=$((19001+(n-1)*100)) p2p=$((30410+n))
  echo "docker rm -f validator${n}_el_pay 2>/dev/null; docker run -d --name validator${n}_el_pay --network arc_testnet_host-access \
  --entrypoint /app/assets/entrypoint_el.sh \
  -v $base/validator${n}/reth-pay:/data/reth/execution-data -v $base/assets:/app/assets \
  -p ${rpc}:8545 -p ${ws}:8546 -p ${auth}:8551 -p ${met}:9001 -p ${p2p}:30303 \
  arc_execution:latest \
  node --datadir=/data/reth/execution-data --chain=/app/assets/payment-genesis.json \
  --http --http.addr=0.0.0.0 --http.port=8545 --http.corsdomain='*' --http.api=eth,net,web3,txpool,debug,admin \
  --ws --ws.addr=0.0.0.0 --ws.port=8546 --ws.origins='*' --ws.api=eth,net,web3,txpool \
  --authrpc.addr=0.0.0.0 --authrpc.port=8551 --authrpc.jwtsecret=/app/assets/payment-jwt.hex \
  --metrics=0.0.0.0:9001 --disable-discovery --ipcdisable --port 30303 \
  --arc.builder.deadline=2000 --arc.builder.wait-for-payload=true --txpool.nolocals \
  --txpool.pending-max-count=200000 --txpool.queued-max-count=200000 && docker network connect arc_testnet_default validator${n}_el_pay"
}

start(){
  setup_env
  echo "==> [0/9] clean everywhere"
  pkill -x spammer 2>/dev/null; rm -f "$RUN/spam.stop"
  ids=$(docker ps -aq --filter name=validator); [ -n "$ids" ] && docker rm -f $ids
  docker run --rm -v "$REPO/.quake":/q --user root alpine rm -rf /q/$SCEN
  for n in 2 3 4; do tss "${RHOST[$n]}" "docker rm -f validator1_cl validator1_el validator1_el_pay validator2_cl validator2_el validator2_el_pay validator3_cl validator3_el validator3_el_pay validator4_cl validator4_el validator4_el_pay 2>/dev/null; rm -rf $RBASE 2>/dev/null || docker run --rm -v /home/papaduck/arc-fleet:/f --user root alpine rm -rf /f/$SCEN; mkdir -p $RBASE; docker run --rm -v /home/papaduck/arc-fleet:/f --user root alpine chown -R \$(id -u):\$(id -g) /f; true" || true; done

  echo "==> [1/9] generate testnet locally (quake)"
  target/release/quake -f "crates/quake/scenarios/${SCEN}.toml" start -e 1000 --monitoring false --force >"$RUN/quake.log" 2>&1 || true
  [ "$(docker ps --format '{{.Names}}' | grep -cE 'validator[0-9]+_(cl|el)$')" -ge 8 ] || { echo "!! quake start failed"; exit 1; }
  # stop everything again — we only wanted the generated trees; fleet topology comes next
  docker rm -f $(docker ps -aq --filter name=validator) >/dev/null

  echo "==> [2/9] preseed payment genesis (10M) + init + copy"
  python3 - <<'PY'
import json
g=json.load(open('.quake/soak4/assets/genesis.json'))
out=open('.quake/soak4/assets/payment-genesis.json','w')
out.write(json.dumps({k:v for k,v in g.items() if k!='alloc'})[:-1]+', "alloc": {')
first=True
for a,v in g['alloc'].items():
    out.write(('' if first else ',')+json.dumps(a)+':'+json.dumps(v)); first=False
for i in range(10_000_000):
    out.write(',"0x%040x":{"balance":"0xde0b6b3a7640000"}'%(0x2000000000+i))
out.write('}}')
PY
  BIN=$(grep -oE '[^" ]*arc-node-execution' "$LBASE/assets/entrypoint_el.sh" | head -1)
  mkdir -p "$LBASE/validator1/reth-pay"
  docker run --rm -v "$LBASE/validator1/reth-pay":/data/reth/execution-data \
    -v "$LBASE/assets":/app/assets --entrypoint "$BIN" arc_execution:latest \
    init --datadir /data/reth/execution-data --chain /app/assets/payment-genesis.json >/dev/null 2>&1
  for i in 2 3 4; do rm -rf "$LBASE/validator$i/reth-pay"; cp -a "$LBASE/validator1/reth-pay" "$LBASE/validator$i/reth-pay"; done
  cp assets/localdev/payment-jwt.hex "$LBASE/assets/" 2>/dev/null || true

  echo "==> [3/9] fleet compose surgery"
  python3 experiments/dual-el/fleet/gen-fleet.py

  echo "==> [4/9] ship trees to remotes (parallel)"
  for n in 2 3 4; do
    ( tar -C "$LBASE" -czf - assets "validator$n" compose-val$n.yaml | TSS_TMO=900 tss "${RHOST[$n]}" "tar -C $RBASE -xzf -" \
      && echo "shipped val$n -> ${RHOST[$n]}" ) &
  done; wait

  echo "==> [5/9] start validator1 locally + val2-4 remotely"
  docker compose -f "$LBASE/compose.yaml" up -d validator1_cl validator1_el
  for n in 2 3 4; do tss "${RHOST[$n]}" "docker compose -f $RBASE/compose-val$n.yaml up -d"; done

  echo "==> [6/9] payment ELs (local val1; remote val2-4; sequential — genesis parse RAM)"
  eval "$(payment_el_cmd 1 "$LBASE")"
  for n in 2 3 4; do tss "${RHOST[$n]}" "$(payment_el_cmd $n "$RBASE")"; done
  # wait for RPCs, then clamp only alien2 (15G box)
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

  echo "==> [8/9] NO load started. Flood the fleet separately with:"
  echo "    ./experiments/dual-el/fleet/spam-fleet.sh start   (stop|status; rates via env)"
  echo "==> [9/9] fleet dashboard"
  python3 -c "import json;json.dump({'val2':'${RTS[2]}','val3':'${RTS[3]}','val4':'${RTS[4]}'},open('$RUN/fleet-endpoints.json','w'))"
  old=$(ss -ltnp 2>/dev/null | grep ':8080 ' | grep -oP 'pid=\K[0-9]+' | head -1); [ -n "$old" ] && kill "$old" 2>/dev/null
  DUALEL_FLEET="$RUN/fleet-endpoints.json" nohup python3 experiments/dual-el/dashboard.py >"$RUN/dashboard.log" 2>&1 & echo $! >"$RUN/dash.pid"

  echo "==> VERIFY"
  sleep 60; verify
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
  b=$(curl -s -m5 -X POST http://127.0.0.1:19545 -H 'content-type: application/json' --data '{"jsonrpc":"2.0","id":1,"method":"eth_getBalance","params":["0x00000000000000000000000000000020004c4b40","latest"]}' | python3 -c "import sys,json;print(int(json.load(sys.stdin)['result'],16))" 2>/dev/null)
  echo "preseed[5M]=${b:-X}"
  [ "$ok" = 1 ] && [ "$b" = "1000000000000000000" ] && echo "✅ FLEET VERIFIED (4 machines, preseed present)" || echo "❌ FLEET VERIFY FAILED — fallback: ./experiments/dual-el/demo-empty.sh start"
}

stop(){
  touch "$RUN/spam.stop" 2>/dev/null; [ -f "$RUN/spam.pid" ] && kill "$(cat "$RUN/spam.pid")" 2>/dev/null; pkill -x spammer 2>/dev/null
  [ -f "$RUN/dash.pid" ] && kill "$(cat "$RUN/dash.pid")" 2>/dev/null
  ids=$(docker ps -aq --filter name=validator); [ -n "$ids" ] && docker rm -f $ids
  for n in 2 3 4; do tss "${RHOST[$n]}" "docker rm -f validator1_cl validator1_el validator1_el_pay validator2_cl validator2_el validator2_el_pay validator3_cl validator3_el validator3_el_pay validator4_cl validator4_el validator4_el_pay 2>/dev/null; docker network rm arc_testnet_default arc_testnet_host-access 2>/dev/null; rm -rf $RBASE 2>/dev/null || docker run --rm -v /home/papaduck/arc-fleet:/f --user root alpine rm -rf /f/$SCEN; true" || true; done
  docker run --rm -v "$REPO/.quake":/q --user root alpine rm -rf /q/$SCEN
  for net in arc_testnet_default arc_testnet_host-access arc_testnet_blockscout; do docker network rm $net >/dev/null 2>&1 || true; done
  echo "✅ fleet stopped and cleaned (all 4 machines)"
}

status(){ verify; }

case "${1:-}" in start) start;; stop) stop;; status) status;; verify) verify;; *) echo "usage: $0 {start|stop|status}"; exit 1;; esac
