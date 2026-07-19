#!/usr/bin/env bash
# FLEET demo with the PAYMENT LANE COMMITTED BY SALT (MegaETH) instead of reth's MPT.
# 4 validators on 4 physical tailscale machines, clean genesis, no preseed, no load.
#
#   validator1 -> here (ginny-alienware)      validator3 -> papaduck
#   validator2 -> ginnythui                   validator4 -> papaduck-alien2 (15G: pay EL capped 11G)
#
#   ./demo-fleet-salt.sh ship | start | verify | status | stop
#
# Derived from demo-fleet-empty.sh. The ONLY functional differences:
#   - payment ELs run arc_execution_salt:latest with ARC_PAYMENT_ROOT=salt
#   - a `ship` step, because the remotes do NOT have the SALT image (it is new to this branch)
#   - verify additionally checks that all 4 machines agree on the payment-lane state root
# The EVM lane is untouched and still MPT-committed, so the demo shows both commitments side by
# side under one consensus certificate, across real machines with real network latency.
#
# PREREQS (in order — each fails loudly if skipped):
#   1. tailscale ssh working to all 3 remotes. It expires; re-auth with:
#        tailscale ssh <host> true      # then visit the printed login.tailscale.com URL
#      Check with: ./demo-fleet-salt.sh preflight
#   2. SALT image built here:
#        DOCKER_BUILDKIT=1 docker build -f deployments/Dockerfile.execution.salt \
#          -t arc_execution_salt:latest .
#   3. SALT image shipped to the remotes:  ./demo-fleet-salt.sh ship   (~5-10 min, 235MB x3)
#   4. target/release/{quake,spammer} present here.
#
# FALLBACK on demo day: ../demo-salt.sh (single machine, same SALT payment lane).
#
# HONEST SCOPE: demonstrates SALT computing consensus-valid payment-lane roots agreed across 4
# physical validators. NOT a like-for-like MPT-vs-SALT perf claim (SALT commits (nonce,balance)
# only, persists no trie nodes, keeps its commitment in RAM), and NOT production-ready (contract
# storage uncommitted, no reorg rollback, eth_getProof assumes MPT). Keep to native transfers.
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$REPO"
SCEN=soak4; RUN=/tmp/dualel-fleet-salt; mkdir -p "$RUN"
LBASE="$REPO/.quake/$SCEN"
# Per-host data roots. validator3 (papaduck) lands on its FAST NVMe: measured fsync 19.6ms on its
# home disk vs 1.7ms on /mnt/blockchain.ssd (11.5x), which showed up as ~468ms/block persistence
# for v3 on BOTH lanes -- a disk effect, not a commitment effect. FASTSSD=0 to use home disks.
declare -A RPARENT=( [2]=/home/papaduck/arc-fleet [3]=/home/papaduck/arc-fleet [4]=/home/papaduck/arc-fleet )
declare -A RB=( [2]=/home/papaduck/arc-fleet/$SCEN [3]=/home/papaduck/arc-fleet/$SCEN [4]=/home/papaduck/arc-fleet/$SCEN )
if [ "${FASTSSD:-1}" = 1 ]; then
  RPARENT[3]=/mnt/blockchain.ssd/arc-fleet
  RB[3]=/mnt/blockchain.ssd/arc-fleet/$SCEN
fi
LOCAL_TS=100.124.148.61
IMG="${IMG:-arc_execution_salt:latest}"
declare -A RHOST=( [2]=ginnythui [3]=papaduck [4]=papaduck-alien2 )
declare -A RTS=( [2]=100.85.150.119 [3]=100.70.62.92 [4]=100.86.97.40 )
PAY_PORT(){ echo $((19545+($1-1)*100)); }
tss(){ local h=$1; shift; timeout "${TSS_TMO:-120}" tailscale ssh "$h" "$@"; }
setup_env(){ export NVM_DIR="$HOME/.nvm"; [ -s "$NVM_DIR/nvm.sh" ] && . "$NVM_DIR/nvm.sh" >/dev/null 2>&1; nvm use 22 >/dev/null 2>&1 || true; export PATH="$HOME/.foundry/bin:$HOME/.cargo/bin:$PATH"; }

# ---------------------------------------------------------------- preflight
preflight(){
  local ok=1
  echo "== preflight =="
  docker image inspect "$IMG" >/dev/null 2>&1 \
    && echo "  [ok] $IMG present locally" \
    || { echo "  [FAIL] $IMG missing — build it (see header)"; ok=0; }
  [ -x target/release/quake ]   && echo "  [ok] quake"   || { echo "  [FAIL] target/release/quake missing"; ok=0; }
  [ -x target/release/spammer ] && echo "  [ok] spammer" || { echo "  [FAIL] target/release/spammer missing"; ok=0; }
  for n in 2 3 4; do
    if out=$(TSS_TMO=15 tss "${RHOST[$n]}" "echo OK" 2>&1) && [ "$out" = "OK" ]; then
      echo "  [ok] ssh ${RHOST[$n]}"
    else
      echo "  [FAIL] ssh ${RHOST[$n]}: $(echo "$out" | head -2 | tr '\n' ' ')"
      echo "         re-auth:  tailscale ssh ${RHOST[$n]} true   (then visit the printed URL)"
      ok=0
    fi
  done
  for n in 2 3 4; do
    if TSS_TMO=20 tss "${RHOST[$n]}" "docker image inspect $IMG >/dev/null 2>&1" 2>/dev/null; then
      echo "  [ok] $IMG on ${RHOST[$n]}"
    else
      echo "  [warn] $IMG NOT on ${RHOST[$n]} — run: $0 ship"
    fi
  done
  [ "$ok" = 1 ] && echo "== preflight PASSED ==" || { echo "== preflight FAILED =="; return 1; }
}

# ---------------------------------------------------------------- ship image
ship(){
  echo "== shipping $IMG to remotes (235MB each; ~5-10 min) =="
  docker image inspect "$IMG" >/dev/null 2>&1 || { echo "!! $IMG missing locally"; exit 1; }
  for n in 2 3 4; do
    ( docker save "$IMG" | gzip -1 | TSS_TMO=1800 tss "${RHOST[$n]}" "gunzip | docker load" \
        && echo "  shipped -> ${RHOST[$n]}" || echo "  FAILED -> ${RHOST[$n]}" ) &
  done; wait
  for n in 2 3 4; do
    TSS_TMO=20 tss "${RHOST[$n]}" "docker image inspect $IMG >/dev/null 2>&1" 2>/dev/null \
      && echo "  [ok] ${RHOST[$n]} has $IMG" || echo "  [FAIL] ${RHOST[$n]} missing $IMG"
  done
}

# ---------------------------------------------------------------- payment EL (SALT)
payment_el_cmd(){ # <n> <base>
  local n=$1 base=$2
  local rpc=$(PAY_PORT $n) ws=$((19546+(n-1)*100)) auth=$((19551+(n-1)*100)) met=$((19001+(n-1)*100)) p2p=$((30410+n))
  echo "docker rm -f validator${n}_el_pay 2>/dev/null; docker run -d --name validator${n}_el_pay --network arc_testnet_host-access \
  --entrypoint /app/assets/entrypoint_el.sh \
  -e ARC_PAYMENT_ROOT=salt -e ARC_JMT_TRACE=1 \
  -v $base/validator${n}/reth-pay:/data/reth/execution-data -v $base/assets:/app/assets \
  -p ${rpc}:8545 -p ${ws}:8546 -p ${auth}:8551 -p ${met}:9001 -p ${p2p}:30303 \
  $IMG \
  node --datadir=/data/reth/execution-data --chain=/app/assets/genesis.json \
  --http --http.addr=0.0.0.0 --http.port=8545 --http.corsdomain='*' --http.api=eth,net,web3,txpool,debug,admin \
  --ws --ws.addr=0.0.0.0 --ws.port=8546 --ws.origins='*' --ws.api=eth,net,web3,txpool \
  --authrpc.addr=0.0.0.0 --authrpc.port=8551 --authrpc.jwtsecret=/app/assets/payment-jwt.hex \
  --metrics=0.0.0.0:9001 --disable-discovery --ipcdisable --port 30303 \
  --engine.state-root-fallback --engine.disable-parallel-sparse-trie \
  --arc.builder.deadline=500 --arc.builder.wait-for-payload=true --txpool.nolocals \
  --txpool.pending-max-count=200000 --txpool.queued-max-count=200000 && docker network connect arc_testnet_default validator${n}_el_pay"
}

# ---------------------------------------------------------------- start
start(){
  setup_env
  preflight || { echo "!! fix preflight failures first"; exit 1; }

  echo "==> [0/9] clean everywhere"
  pkill -x spammer 2>/dev/null; rm -f "$RUN/spam.stop"
  ids=$(docker ps -aq --filter name=validator); [ -n "$ids" ] && docker rm -f $ids >/dev/null
  docker run --rm -v "$REPO/.quake":/q --user root alpine rm -rf /q/$SCEN
  for n in 2 3 4; do tss "${RHOST[$n]}" "docker rm -f \$(docker ps -aq --filter name=validator) 2>/dev/null; rm -rf ${RB[$n]} 2>/dev/null || docker run --rm -v ${RPARENT[$n]}:/f --user root alpine rm -rf /f/$SCEN; mkdir -p ${RB[$n]}; docker run --rm -v ${RPARENT[$n]}:/f --user root alpine chown -R \$(id -u):\$(id -g) /f; true" || true; done

  echo "==> [1/9] generate testnet locally (quake)"
  target/release/quake -f "crates/quake/scenarios/${SCEN}.toml" start -e 1000 --monitoring false --force >"$RUN/quake.log" 2>&1 || true
  [ "$(docker ps --format '{{.Names}}' | grep -cE 'validator[0-9]+_(cl|el)$')" -ge 8 ] || { echo "!! quake start failed — see $RUN/quake.log"; exit 1; }
  docker rm -f $(docker ps -aq --filter name=validator) >/dev/null

  echo "==> [2/9] clean payment datadirs (ELs auto-init on boot)"
  cp assets/localdev/payment-jwt.hex "$LBASE/assets/" 2>/dev/null || true
  for i in 1 2 3 4; do rm -rf "$LBASE/validator$i/reth-pay"; mkdir -p "$LBASE/validator$i/reth-pay"; done

  echo "==> [3/9] fleet compose surgery (tailscale peer rewrite)"
  # Fail fast: this silently produced nothing when gen-fleet.py had a hardcoded worktree path,
  # and every downstream step then failed with confusing "no such file" errors on the remotes.
  python3 experiments/dual-el/fleet/gen-fleet.py || { echo "!! gen-fleet.py failed"; exit 1; }
  for n in 2 3 4; do
    [ -f "$LBASE/compose-val$n.yaml" ] || { echo "!! missing $LBASE/compose-val$n.yaml"; exit 1; }
  done

  echo "==> [4/9] ship trees to remotes (parallel)"
  for n in 2 3 4; do
    ( tar -C "$LBASE" -czf - assets "validator$n" compose-val$n.yaml | TSS_TMO=900 tss "${RHOST[$n]}" "tar -C ${RB[$n]} -xzf -" \
      && TSS_TMO=30 tss "${RHOST[$n]}" "test -f ${RB[$n]}/compose-val$n.yaml" \
      && echo "  shipped val$n -> ${RHOST[$n]} (${RB[$n]})" || echo "  !! SHIP FAILED val$n -> ${RHOST[$n]}" ) &
  done; wait
  # gen-fleet.py writes home-disk paths into every compose; repoint val3's to the fast mount
  if [ "${FASTSSD:-1}" = 1 ]; then
    tss papaduck "sed -i 's|/home/papaduck/arc-fleet|/mnt/blockchain.ssd/arc-fleet|g' ${RB[3]}/compose-val3.yaml && echo '  val3 compose repointed to NVMe:' \$(grep -c blockchain.ssd ${RB[3]}/compose-val3.yaml) 'paths'"
  fi

  echo "==> [5/9] start validator1 locally + val2-4 remotely"
  docker compose -f "$LBASE/compose.yaml" up -d validator1_cl validator1_el
  for n in 2 3 4; do tss "${RHOST[$n]}" "docker compose -f ${RB[$n]}/compose-val$n.yaml up -d"; done

  echo "==> [6/9] payment ELs with SALT (sequential — genesis parse RAM)"
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

  echo "==> [8/9] no load started. Drive the SALT payment lane (native transfers only):"
  echo "    target/release/spammer ws --targets ws://127.0.0.1:19546,ws://${RTS[2]}:19646,ws://${RTS[3]}:19746,ws://${RTS[4]}:19846 \\"
  echo "      -r 800 -g 8 -a 1000 -l --fresh-recipients --mix transfer=100"

  echo "==> [9/9] fleet dashboard"
  python3 -c "import json;json.dump({'val2':'${RTS[2]}','val3':'${RTS[3]}','val4':'${RTS[4]}'},open('$RUN/fleet-endpoints.json','w'))"
  old=$(ss -ltnp 2>/dev/null | grep ':8080 ' | grep -oP 'pid=\K[0-9]+' | head -1); [ -n "$old" ] && kill "$old" 2>/dev/null
  DUALEL_FLEET="$RUN/fleet-endpoints.json" nohup python3 experiments/dual-el/dashboard.py >"$RUN/dashboard.log" 2>&1 & echo $! >"$RUN/dash.pid"

  echo "==> VERIFY (60s settle)"; sleep 60; verify
}

# ---------------------------------------------------------------- verify
# The claim under test: 4 PHYSICAL validators independently compute the SAME SALT payment-lane
# state root. Checked at a settled height, not just "everyone is alive".
verify(){
  local ok=1
  echo "== liveness =="
  for n in 1 2 3 4; do
    host=127.0.0.1; [ $n -ge 2 ] && host=${RTS[$n]}
    e=$((8545+(n-1)*100)); p=$(PAY_PORT $n)
    eh=$(curl -s -m5 -X POST "http://$host:$e" -H 'content-type: application/json' --data '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' | python3 -c "import sys,json;print(int(json.load(sys.stdin)['result'],16))" 2>/dev/null)
    ph=$(curl -s -m5 -X POST "http://$host:$p" -H 'content-type: application/json' --data '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' | python3 -c "import sys,json;print(int(json.load(sys.stdin)['result'],16))" 2>/dev/null)
    echo "  val$n@$host  evm(MPT)=${eh:-X}  pay(SALT)=${ph:-X}"
    { [ -z "$eh" ] || [ -z "$ph" ]; } && ok=0
  done

  echo "== SALT root agreement across machines =="
  # settle to a height every validator has: min(heads) - 5
  local heights=() minh=""
  for n in 1 2 3 4; do
    host=127.0.0.1; [ $n -ge 2 ] && host=${RTS[$n]}
    h=$(curl -s -m5 -X POST "http://$host:$(PAY_PORT $n)" -H 'content-type: application/json' --data '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' | python3 -c "import sys,json;print(int(json.load(sys.stdin)['result'],16))" 2>/dev/null)
    [ -n "$h" ] && heights+=("$h")
  done
  [ ${#heights[@]} -eq 4 ] || { echo "  cannot settle — not all payment ELs responding"; return 1; }
  minh=$(printf '%s\n' "${heights[@]}" | sort -n | head -1); minh=$((minh-5))
  [ "$minh" -lt 1 ] && minh=1
  local hexh; hexh=$(printf '0x%x' "$minh")
  local roots=()
  for n in 1 2 3 4; do
    host=127.0.0.1; [ $n -ge 2 ] && host=${RTS[$n]}
    r=$(curl -s -m5 -X POST "http://$host:$(PAY_PORT $n)" -H 'content-type: application/json' \
        --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_getBlockByNumber\",\"params\":[\"$hexh\",false]}" 2>/dev/null \
        | python3 -c "import sys,json;b=json.load(sys.stdin)['result'];print(b['stateRoot'])" 2>/dev/null)
    echo "  val$n  block $minh  root ${r:-UNREACHABLE}"
    roots+=("${r:-X}")
  done
  local uniq; uniq=$(printf '%s\n' "${roots[@]}" | sort -u | wc -l)
  if [ "$uniq" = 1 ] && [ "${roots[0]}" != "X" ]; then
    echo "  ✅ all 4 machines agree on the SALT payment-lane root at block $minh"
  else
    echo "  ❌ ROOT DIVERGENCE across machines ($uniq distinct roots) — this is a consensus bug, investigate"
    ok=0
  fi

  echo "== mismatch counters =="
  local tot=0
  for n in 1 2 3 4; do
    if [ $n = 1 ]; then c=$(docker logs validator1_el_pay 2>&1 | grep -ci "does not match")
    else c=$(tss "${RHOST[$n]}" "docker logs validator${n}_el_pay 2>&1 | grep -ci 'does not match'" 2>/dev/null); fi
    # keep digits only: an unreachable host yields empty/garbage, which broke $(( )) arithmetic
    c=$(printf '%s' "${c:-}" | tr -cd '0-9')
    if [ -z "$c" ]; then
      echo "  val$n state-root mismatches: UNREACHABLE"; ok=0
    else
      echo "  val$n state-root mismatches: $c"
      [ "$c" != "0" ] && ok=0
      tot=$((tot+c))
    fi
  done

  echo "== SALT seed trace (proves the commitment covers genesis, not just touched accounts) =="
  docker logs validator1_el_pay 2>&1 | grep -i "SALT seeded" | tail -1 | sed 's/^/  val1 /' || echo "  (none)"

  [ "$ok" = 1 ] && echo "✅ FLEET SALT DEMO VERIFIED (4 machines, agreed root, 0 mismatches)" \
                || echo "❌ VERIFY FAILED — fallback: ../demo-salt.sh start (single machine)"
}

status(){ verify; }

stop(){
  echo "==> stopping fleet…"
  touch "$RUN/spam.stop" 2>/dev/null; pkill -x spammer 2>/dev/null
  [ -f "$RUN/dash.pid" ] && kill "$(cat "$RUN/dash.pid")" 2>/dev/null
  ids=$(docker ps -aq --filter name=validator); [ -n "$ids" ] && docker rm -f $ids >/dev/null 2>&1
  for n in 2 3 4; do tss "${RHOST[$n]}" "docker rm -f \$(docker ps -aq --filter name=validator) 2>/dev/null; true" || true; done
  for n in arc_testnet_default arc_testnet_host-access; do docker network rm "$n" >/dev/null 2>&1 || true; done
  rm -f "$RUN"/*.pid
  echo "  ✅ fleet stopped."
}

case "${1:-}" in
  preflight) preflight ;;
  ship) ship ;;
  start) start ;;
  verify|status) verify ;;
  stop) stop ;;
  *) echo "usage: $0 {preflight|ship|start|verify|status|stop}"; exit 1 ;;
esac
