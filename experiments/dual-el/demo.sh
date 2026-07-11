#!/usr/bin/env bash
# Dual-EL payment-lane demo controller.
#
#   ./demo.sh start   — genesis + 4 validators (CL+EVM-EL+payment-EL) + dual-lane load + live dashboard
#   ./demo.sh stop    — stop load & dashboard, tear down all containers/networks, DELETE datadirs
#   ./demo.sh status  — quick health (heads, agreement, dashboard)
#
# Dashboard: http://localhost:8080  (EVM lane vs payment lane, two roots, value_id, state growth,
# unique addresses, cross-validator agreement).
set -uo pipefail

REPO="/home/papaduck/arc-node-paymentlane"
SCEN="soak4"
PORT=8080
RUN="/tmp/dualel-demo"            # pids, logs, stop-flag
DASH="$REPO/experiments/dual-el/dashboard.py"
EXTRA_ACCOUNTS=1000               # genesis prefunded accounts for load (bump for more unique addrs)
PAY_RATE=6000; EVM_RATE=60        # tx/s per lane
cd "$REPO"
mkdir -p "$RUN"

have_node22() { node -v 2>/dev/null | grep -qE '^v(2[024]|22)'; }
setup_env() {
  export NVM_DIR="$HOME/.nvm"; [ -s "$NVM_DIR/nvm.sh" ] && . "$NVM_DIR/nvm.sh" >/dev/null 2>&1
  nvm use 22 >/dev/null 2>&1 || true
  export PATH="$HOME/.foundry/bin:$HOME/.cargo/bin:$PATH"
}
val_up() { docker ps --format '{{.Names}}' | grep -cE 'validator[0-9]+_(cl|el|el_pay)$'; }
bn() { cast block-number --rpc-url "http://127.0.0.1:$1" 2>/dev/null; }

start() {
  setup_env
  have_node22 || { echo "!! need Node 22 (nvm use 22). got: $(node -v 2>/dev/null)"; exit 1; }
  [ "$(val_up)" -gt 0 ] && { echo "!! validators already running — run './demo.sh stop' first"; exit 1; }
  command -v cast >/dev/null || { echo "!! foundry 'cast' not on PATH"; exit 1; }

  echo "==> [1/5] genesis + 4 validators (CL + EVM-EL)…"
  # quake exits non-zero waiting for block 1 (the CLs block until their payment EL is up) — expected.
  target/release/quake -f "crates/quake/scenarios/${SCEN}.toml" start \
      -e "$EXTRA_ACCOUNTS" --monitoring false --force >"$RUN/quake.log" 2>&1 || true
  [ "$(val_up)" -ge 8 ] || { echo "!! validators failed to start — see $RUN/quake.log"; exit 1; }

  echo "==> [2/5] payment EL per validator…"
  cp assets/localdev/payment-jwt.hex ".quake/${SCEN}/assets/" 2>/dev/null || true
  TESTNET="$SCEN" bash experiments/dual-el/launch-payment-els.sh >"$RUN/paylane.log" 2>&1

  echo "==> [3/5] waiting for both lanes to produce…"
  for i in $(seq 1 30); do
    e=$(bn 8645); p=$(bn 19645)
    [ -n "$e" ] && [ -n "$p" ] && [ "$e" -ge 3 ] && [ "$p" -ge 3 ] && { echo "    both lanes live (EVM $e / PAY $p)"; break; }
    sleep 3
  done

  echo "==> [4/5] dual-lane load (EVM ~${EVM_RATE} tx/s, PAYMENT ~${PAY_RATE} tx/s)…"
  rm -f "$RUN/spam.stop"
  ( STOP="$RUN/spam.stop"
    # args: <tag> <targets> <rate> <generators> <accounts>   (accounts must be divisible by generators)
    loop(){ while [ ! -f "$STOP" ]; do
      target/release/spammer ws --targets "$2" -r "$3" -t 3600 -g "$4" -a "$5" --mix transfer=100 \
        >"$RUN/spam_$1.log" 2>&1; sleep 1; done; }
    # target ALL FOUR ELs per lane — the payment ELs run with discovery off (no tx gossip),
    # so every validator's payment mempool must be fed directly, or the un-fed validator
    # proposes empty payment blocks on its round-robin turn.
    loop evm "ws://127.0.0.1:8546,ws://127.0.0.1:8646,ws://127.0.0.1:8746,ws://127.0.0.1:8846" "$EVM_RATE" 2 200 &
    loop pay "ws://127.0.0.1:19546,ws://127.0.0.1:19646,ws://127.0.0.1:19746,ws://127.0.0.1:19846" "$PAY_RATE" 8 "$EXTRA_ACCOUNTS" &
    wait
  ) >/dev/null 2>&1 &
  echo $! >"$RUN/spam.pid"

  echo "==> [5/5] dashboard…"
  # free the port if a stale dashboard is bound
  old=$(ss -ltnp 2>/dev/null | grep ":$PORT " | grep -oP 'pid=\K[0-9]+' | head -1); [ -n "$old" ] && kill "$old" 2>/dev/null
  nohup python3 "$DASH" >"$RUN/dashboard.log" 2>&1 & echo $! >"$RUN/dash.pid"

  echo
  echo "  ✅ dual-EL demo up.   dashboard →  http://localhost:$PORT"
  echo "     EVM ELs  8645/8745/8845    payment ELs  19545/19645/19745/19845"
  echo "     ./demo.sh stop   tears everything down and deletes datadirs."
}

stop() {
  echo "==> stopping load…"
  touch "$RUN/spam.stop" 2>/dev/null
  [ -f "$RUN/spam.pid" ] && kill "$(cat "$RUN/spam.pid")" 2>/dev/null
  pkill -x spammer 2>/dev/null   # exact process name — cannot match the caller's shell
  echo "==> stopping dashboard…"
  [ -f "$RUN/dash.pid" ] && kill "$(cat "$RUN/dash.pid")" 2>/dev/null
  old=$(ss -ltnp 2>/dev/null | grep ":$PORT " | grep -oP 'pid=\K[0-9]+' | head -1); [ -n "$old" ] && kill "$old" 2>/dev/null
  sleep 1
  echo "==> removing containers…"
  ids=$(docker ps -aq --filter "name=validator"); [ -n "$ids" ] && docker rm -f $ids >/dev/null 2>&1
  echo "==> removing networks…"
  for n in arc_testnet_default arc_testnet_host-access arc_testnet_blockscout arc_testnet_monitoring_default; do
    docker network rm "$n" >/dev/null 2>&1 || true
  done
  echo "==> deleting datadirs (.quake/$SCEN, root-owned → via throwaway container)…"
  [ -d ".quake/$SCEN" ] && docker run --rm -v "$REPO/.quake":/q alpine rm -rf "/q/$SCEN" 2>/dev/null
  rm -f "$RUN"/*.pid "$RUN"/spam.stop
  echo "  ✅ stopped and cleaned. (logs kept in $RUN)"
}

status() {
  setup_env
  echo "containers: $(val_up)/12"
  echo "EVM heads : $(bn 8645) $(bn 8745) $(bn 8845)"
  echo "PAY heads : $(bn 19545) $(bn 19645) $(bn 19745) $(bn 19845)"
  if curl -sf "http://localhost:$PORT/state" >/dev/null 2>&1; then
    curl -s "http://localhost:$PORT/state" | python3 -c "
import sys,json;s=json.load(sys.stdin);g=s.get('growth',{})
b=s.get('both') or {}
print('dashboard : up  ('+('agree' if s['evm']['agree'] and s['pay']['agree'] else 'DIVERGENCE')+')')
print('value_id  :', (b.get('value_id') or '-')[:14]+'…')
print('state     : EVM %s MB / PAY %s MB'%(g.get('evm_mb'),g.get('pay_mb')))
print('addresses : EVM %s / PAY %s (unique, live)'%(g.get('evm_addr'),g.get('pay_addr')))" 2>/dev/null
  else
    echo "dashboard : down"
  fi
}

case "${1:-}" in
  start)  start ;;
  stop)   stop ;;
  status) status ;;
  *) echo "usage: $0 {start|stop|status}"; exit 1 ;;
esac
