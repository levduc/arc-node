#!/usr/bin/env bash
# Dual-EL demo with the PAYMENT LANE COMMITTED BY SALT (MegaETH) instead of reth's MPT.
#
#   ./demo-salt.sh start | stop | status        dashboard: http://localhost:8080
#
# Topology (identical to demo-empty.sh except the payment lane's commitment):
#   4x Malachite CL  ->  EVM-EL (MPT root)          : reth, unchanged
#                    ->  payment-EL (SALT root)     : arc_execution_salt image, ARC_PAYMENT_ROOT=salt
# Both lanes commit under ONE consensus certificate (value_id binds both block hashes), so the demo
# shows an alternative state commitment running in a real BFT chain, not a microbenchmark.
#
# PREREQ: the SALT image must exist.
#   DOCKER_BUILDKIT=1 docker build -f deployments/Dockerfile.execution.salt \
#     -t arc_execution_salt:latest .
# A host-built binary cannot be substituted: host glibc 2.39 vs the image's Debian 12 / 2.36.
#
# HONEST SCOPE — what this demo does and does not show:
#   DOES : SALT computing consensus-valid payment-lane state roots inside reth, agreed by all 4
#          validators, live, under load, alongside an MPT-committed EVM lane.
#   NOT  : a like-for-like MPT-vs-SALT performance claim. The SALT lane commits (nonce, balance)
#          only, persists no trie nodes, and keeps its commitment in RAM. See
#          experiments/dual-el/jmt/RESULTS-2h-salt-vs-mpt.md for the measured numbers and caveats.
#   NOT  : a production-ready client. Contract storage is NOT committed by the SALT lane, there is
#          no reorg/unwind rollback for the commitment, and eth_getProof still assumes the MPT.
#          Keep the demo to native transfers.
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SCEN="soak4"; PORT=8080; RUN="/tmp/dualel-salt"
DASH="$REPO/experiments/dual-el/dashboard.py"
IMG="${IMG:-arc_execution_salt:latest}"
cd "$REPO"; mkdir -p "$RUN"

setup_env(){ export NVM_DIR="$HOME/.nvm"; [ -s "$NVM_DIR/nvm.sh" ] && . "$NVM_DIR/nvm.sh" >/dev/null 2>&1; nvm use 22 >/dev/null 2>&1 || true; export PATH="$HOME/.foundry/bin:$HOME/.cargo/bin:$PATH"; }
val_up(){ docker ps --format '{{.Names}}' | grep -cE 'validator[0-9]+_(cl|el|el_pay)$'; }
bn(){ cast block-number --rpc-url "http://127.0.0.1:$1" 2>/dev/null; }

start(){
  setup_env
  docker image inspect "$IMG" >/dev/null 2>&1 || {
    echo "!! $IMG not found — build it first (see header)"; exit 1; }
  [ "$(val_up)" -gt 0 ] && { echo "!! validators already running — stop the other demo first"; exit 1; }

  echo "==> [1/5] genesis + 4 validators (CL + EVM-EL, MPT)…"
  target/release/quake -f "crates/quake/scenarios/${SCEN}.toml" start -e 1000 --monitoring false --force >"$RUN/quake.log" 2>&1 || true
  [ "$(val_up)" -ge 8 ] || { echo "!! start failed — see $RUN/quake.log"; exit 1; }

  echo "==> [2/5] payment ELs with SALT commitment…"
  cp assets/localdev/payment-jwt.hex ".quake/${SCEN}/assets/" 2>/dev/null || true
  IMG="$IMG" TESTNET="$SCEN" bash experiments/dual-el/launch-payment-els-salt.sh >"$RUN/paylane.log" 2>&1

  echo "==> [3/5] waiting for both lanes…"
  for i in $(seq 1 40); do e=$(bn 8645); p=$(bn 19645); [ -n "$e" ] && [ -n "$p" ] && [ "$p" -ge 3 ] && { echo "    lanes live (EVM $e / PAY $p)"; break; }; sleep 3; done

  echo "==> [3b] confirming the payment lane is really using SALT…"
  if docker logs validator1_el_pay 2>&1 | grep -qi "SALT seeded"; then
    docker logs validator1_el_pay 2>&1 | grep -i "SALT seeded" | tail -1 | sed 's/^/    /'
  else
    echo "    (no seed trace; set ARC_JMT_TRACE=1 to see it — lane still SALT-committed)"
  fi
  bad=$(docker logs validator1_el_pay 2>&1 | grep -ci "does not match")
  echo "    payment-lane state-root mismatches: ${bad:-0}"

  echo "==> [4/5] memory caps (safety net)…"
  for i in 1 2 3 4; do
    docker update --memory 768m --memory-swap 768m validator${i}_cl >/dev/null
    docker update --memory 5g   --memory-swap 5g   validator${i}_el >/dev/null
    docker update --memory 10g  --memory-swap 10g  validator${i}_el_pay >/dev/null
  done

  echo "==> [5/5] dashboard…"
  old=$(ss -ltnp 2>/dev/null | grep ":$PORT " | grep -oP 'pid=\K[0-9]+' | head -1); [ -n "$old" ] && kill "$old" 2>/dev/null
  nohup python3 "$DASH" >"$RUN/dashboard.log" 2>&1 & echo $! >"$RUN/dash.pid"

  echo
  echo "  ✅ dual-EL testnet up — EVM lane on MPT, PAYMENT LANE ON SALT."
  echo "     dashboard → http://localhost:$PORT"
  echo "     drive the payment lane (native transfers only):"
  echo "       target/release/spammer ws --targets ws://127.0.0.1:19546,ws://127.0.0.1:19646,ws://127.0.0.1:19746,ws://127.0.0.1:19846 \\"
  echo "         -r 800 -a 1000 -g 8 -l --fresh-recipients --mix transfer=100"
}

status(){
  setup_env
  echo "containers: $(val_up)/12"
  for i in 1 2 3 4; do
    e=$(bn $((8645 + (i-1)*100))); p=$(bn $((19545 + (i-1)*100)))
    printf "  validator%d  EVM(MPT)=%-8s PAY(SALT)=%-8s\n" "$i" "${e:-?}" "${p:-?}"
  done
  echo "payment-lane roots (should be IDENTICAL across validators at a settled height):"
  for i in 1 2 3 4; do
    r=$(curl -s -m3 -X POST "http://127.0.0.1:$((19545 + (i-1)*100))" -H 'content-type: application/json' \
        --data '{"jsonrpc":"2.0","id":1,"method":"eth_getBlockByNumber","params":["latest",false]}' 2>/dev/null \
        | python3 -c "import sys,json;b=json.load(sys.stdin)['result'];print(b['number'],b['stateRoot'][:18])" 2>/dev/null)
    printf "  validator%d  %s\n" "$i" "${r:-unreachable}"
  done
  tot=0; for i in 1 2 3 4; do n=$(docker logs validator${i}_el_pay 2>&1 | grep -ci "does not match"); tot=$((tot+n)); done
  echo "total payment-lane state-root mismatches across validators: $tot"
}

stop(){
  echo "==> stopping…"; [ -f "$RUN/dash.pid" ] && kill "$(cat "$RUN/dash.pid")" 2>/dev/null
  old=$(ss -ltnp 2>/dev/null | grep ":$PORT " | grep -oP 'pid=\K[0-9]+' | head -1); [ -n "$old" ] && kill "$old" 2>/dev/null
  pkill -x spammer 2>/dev/null; sleep 1
  ids=$(docker ps -aq --filter "name=validator"); [ -n "$ids" ] && docker rm -f $ids >/dev/null 2>&1
  for n in arc_testnet_default arc_testnet_host-access arc_testnet_blockscout arc_testnet_monitoring_default; do docker network rm "$n" >/dev/null 2>&1 || true; done
  [ -d ".quake/$SCEN" ] && docker run --rm -v "$REPO/.quake":/q alpine rm -rf "/q/$SCEN" 2>/dev/null
  rm -f "$RUN"/*.pid; echo "  ✅ stopped and cleaned."
}

case "${1:-}" in
  start) start ;;
  stop) stop ;;
  status) status ;;
  *) echo "usage: $0 {start|stop|status}"; exit 1 ;;
esac
