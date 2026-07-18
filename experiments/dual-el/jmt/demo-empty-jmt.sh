#!/usr/bin/env bash
# Dual-EL EMPTY-CHAIN demo — PAYMENT LANE ON JMT (arc_execution_jmt), EVM lane on MPT (stock).
# Requires: docker build -t arc_execution_jmt:latest -f experiments/dual-el/jmt/Dockerfile --build-arg BIN=target/release/arc-node-execution .: fresh testnet, both lanes from a clean genesis, NO preseed, NO load.
# Use for a quick clean demo baseline (drive load manually or via the spammer as needed).
#   ./demo-empty.sh start | stop | status      dashboard: http://localhost:8080
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SCEN="soak4"; PORT=8080; RUN="/tmp/dualel-jmt"
DASH="$REPO/experiments/dual-el/dashboard.py"
cd "$REPO"; mkdir -p "$RUN"
setup_env(){ export NVM_DIR="$HOME/.nvm"; [ -s "$NVM_DIR/nvm.sh" ] && . "$NVM_DIR/nvm.sh" >/dev/null 2>&1; nvm use 22 >/dev/null 2>&1 || true; export PATH="$HOME/.foundry/bin:$HOME/.cargo/bin:$PATH"; }
val_up(){ docker ps --format '{{.Names}}' | grep -cE 'validator[0-9]+_(cl|el|el_pay)$'; }
bn(){ cast block-number --rpc-url "http://127.0.0.1:$1" 2>/dev/null; }

start(){
  setup_env
  [ "$(val_up)" -gt 0 ] && { echo "!! validators already running — stop the other demo first"; exit 1; }
  echo "==> [1/4] genesis + 4 validators (CL + EVM-EL)…"
  target/release/quake -f "crates/quake/scenarios/${SCEN}.toml" start -e 1000 --monitoring false --force >"$RUN/quake.log" 2>&1 || true
  [ "$(val_up)" -ge 8 ] || { echo "!! start failed — see $RUN/quake.log"; exit 1; }
  echo "==> [2/4] payment ELs (same clean genesis, gossip-peered)…"
  cp assets/localdev/payment-jwt.hex ".quake/${SCEN}/assets/" 2>/dev/null || true
  TESTNET="$SCEN" bash experiments/dual-el/jmt/launch-payment-els-jmt.sh >"$RUN/paylane.log" 2>&1
  echo "==> [3/4] waiting for both lanes…"
  for i in $(seq 1 30); do e=$(bn 8645); p=$(bn 19645); [ -n "$e" ] && [ -n "$p" ] && [ "$p" -ge 3 ] && { echo "    lanes live (EVM $e / PAY $p)"; break; }; sleep 3; done
  echo "==> [3b] safety-net memory caps…"
  for i in 1 2 3 4; do
    docker update --memory 768m --memory-swap 768m validator${i}_cl >/dev/null
    docker update --memory 5g --memory-swap 5g validator${i}_el >/dev/null
    docker update --memory 10g --memory-swap 10g validator${i}_el_pay >/dev/null
  done
  echo "==> [4/4] dashboard…"
  old=$(ss -ltnp 2>/dev/null | grep ":$PORT " | grep -oP 'pid=\K[0-9]+' | head -1); [ -n "$old" ] && kill "$old" 2>/dev/null
  nohup python3 "$DASH" >"$RUN/dashboard.log" 2>&1 & echo $! >"$RUN/dash.pid"
  echo; echo "  ✅ empty-chain dual-EL testnet up (no load).  dashboard → http://localhost:$PORT"
  echo "     drive load e.g.: target/release/spammer ws --targets ws://127.0.0.1:19546,... -r 1000 -a 1000 -g 8 -l --mix transfer=100"
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
status(){ setup_env; echo "containers: $(val_up)/12"; echo "EVM heads : $(bn 8645) $(bn 8745) $(bn 8845)"; echo "PAY heads : $(bn 19545) $(bn 19645) $(bn 19745) $(bn 19845)"; }
case "${1:-}" in start) start;; stop) stop;; status) status;; *) echo "usage: $0 {start|stop|status}"; exit 1;; esac
