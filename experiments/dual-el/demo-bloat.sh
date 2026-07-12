#!/usr/bin/env bash
# Dual-EL STATE-BLOAT demo: artificially grow the EVM lane's state until the cost of
# state-root computation / disk I/O visibly diverges from the lean payment lane.
#
#   EVM lane load     : GasGuzzler.storageWrite — every call writes FRESH storage slots
#                       (baseIndex = keccak(sender, totalWrite), monotonic counter), so state
#                       grows gas-bound (~4.5k slots per 100M-gas block), while its HISTORY
#                       stays small (few, huge-gas txs).
#   Payment lane load : plain transfers — history grows, state stays lean (user-bound).
#
# This is the paper's isolation thesis enacted: shared general-purpose EVM state bloats with
# CONTRACT activity; the payment lane's state grows only with users.
#
#   ./demo-bloat.sh start   — testnet + bloat load + dashboard (http://localhost:8080)
#   ./demo-bloat.sh stop    — stop everything, tear down containers/networks, DELETE datadirs
#   ./demo-bloat.sh status  — health + current state sizes / state-root latency
#
# Uses its own run dir (/tmp/dualel-bloat); do not run together with demo.sh.
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SCEN="soak4"
PORT=8080
RUN="/tmp/dualel-bloat"
DASH="$REPO/experiments/dual-el/dashboard.py"
EXTRA_ACCOUNTS=1000
# EVM lane: guzzler storage-write. 250 fresh slots/call ≈ 5.5M gas/tx (2x-estimate fits the pool's
# per-tx gas cap; 600 was rejected "gas limit too high"). ~18 tx fill a 100M block.
EVM_RATE=8; SLOTS_PER_CALL=250
# Payment lane: moderate transfer load (leave CPU headroom for the EVM lane's trie work).
PAY_RATE=6000
cd "$REPO"
mkdir -p "$RUN"

setup_env() {
  export NVM_DIR="$HOME/.nvm"; [ -s "$NVM_DIR/nvm.sh" ] && . "$NVM_DIR/nvm.sh" >/dev/null 2>&1
  nvm use 22 >/dev/null 2>&1 || true
  export PATH="$HOME/.foundry/bin:$HOME/.cargo/bin:$PATH"
}
val_up() { docker ps --format '{{.Names}}' | grep -cE 'validator[0-9]+_(cl|el|el_pay)$'; }
bn() { cast block-number --rpc-url "http://127.0.0.1:$1" 2>/dev/null; }

start() {
  setup_env
  node -v 2>/dev/null | grep -qE '^v(20|22|24)' || { echo "!! need Node 22 (nvm use 22)"; exit 1; }
  [ "$(val_up)" -gt 0 ] && { echo "!! validators already running — './demo-bloat.sh stop' (or demo.sh stop) first"; exit 1; }
  command -v cast >/dev/null || { echo "!! foundry 'cast' not on PATH"; exit 1; }

  echo "==> [1/5] genesis + 4 validators (CL + EVM-EL)…"
  target/release/quake -f "crates/quake/scenarios/${SCEN}.toml" start \
      -e "$EXTRA_ACCOUNTS" --monitoring false --force >"$RUN/quake.log" 2>&1 || true
  [ "$(val_up)" -ge 8 ] || { echo "!! validators failed to start — see $RUN/quake.log"; exit 1; }

  echo "==> [1b] preseed payment genesis (10M accounts) + init datadirs…"
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
  BIN=$(grep -oE '[^" ]*arc-node-execution' ".quake/$SCEN/assets/entrypoint_el.sh" | head -1)
  mkdir -p ".quake/$SCEN/validator1/reth-pay"
  docker run --rm -v "$PWD/.quake/$SCEN/validator1/reth-pay":/data/reth/execution-data \
    -v "$PWD/.quake/$SCEN/assets":/app/assets --entrypoint "$BIN" arc_execution:latest \
    init --datadir /data/reth/execution-data --chain /app/assets/payment-genesis.json >/dev/null 2>&1
  for i in 2 3 4; do rm -rf ".quake/$SCEN/validator$i/reth-pay"; cp -a ".quake/$SCEN/validator1/reth-pay" ".quake/$SCEN/validator$i/reth-pay"; done

  echo "==> [2/5] payment EL per validator (gossip-peered)…"
  cp assets/localdev/payment-jwt.hex ".quake/${SCEN}/assets/" 2>/dev/null || true
  PAYMENT_GENESIS=payment-genesis.json TESTNET="$SCEN" bash experiments/dual-el/launch-payment-els.sh >"$RUN/paylane.log" 2>&1

  echo "==> [3/5] waiting for both lanes to produce…"
  for i in $(seq 1 30); do
    e=$(bn 8645); p=$(bn 19645)
    [ -n "$e" ] && [ -n "$p" ] && [ "$e" -ge 3 ] && [ "$p" -ge 3 ] && { echo "    both lanes live (EVM $e / PAY $p)"; break; }
    sleep 3
  done

  echo "==> [4/5] load: EVM = storage-bloat (guzzler ${SLOTS_PER_CALL} fresh slots/call), PAYMENT = transfers…"
  rm -f "$RUN/spam.stop"
  ( STOP="$RUN/spam.stop"
    bloat(){ while [ ! -f "$STOP" ]; do
      target/release/spammer ws \
        --targets "ws://127.0.0.1:8546,ws://127.0.0.1:8646,ws://127.0.0.1:8746,ws://127.0.0.1:8846" \
        -r 6 -t 3600 -g 2 -a 200 -l --mix guzzler=100 \
        --guzzler-fn-weights "storage-write=100@${SLOTS_PER_CALL}" \
        >"$RUN/spam_evm.log" 2>&1; sleep 1; done; }
    pay(){ while [ ! -f "$STOP" ]; do
      target/release/spammer ws \
        --targets "ws://127.0.0.1:19546,ws://127.0.0.1:19646,ws://127.0.0.1:19746,ws://127.0.0.1:19846" \
        -r 3500 -t 3600 -g 8 -a "$EXTRA_ACCOUNTS" --recipient-pool 0x2000000000:10000000 --mix transfer=100 \
        >"$RUN/spam_pay.log" 2>&1; sleep 1; done; }
    bloat & pay & wait
  ) >/dev/null 2>&1 &
  echo $! >"$RUN/spam.pid"

  echo "==> [5/5] dashboard…"
  old=$(ss -ltnp 2>/dev/null | grep ":$PORT " | grep -oP 'pid=\K[0-9]+' | head -1); [ -n "$old" ] && kill "$old" 2>/dev/null
  nohup python3 "$DASH" >"$RUN/dashboard.log" 2>&1 & echo $! >"$RUN/dash.pid"

  echo
  echo "  ✅ state-bloat demo up.  dashboard →  http://localhost:$PORT"
  echo "     EVM state grows ~10-15 GB/h (fresh SSTOREs); watch state-root latency diverge."
  echo "     ./demo-bloat.sh stop  tears everything down and deletes datadirs."
}

stop() {
  echo "==> stopping load…"
  touch "$RUN/spam.stop" 2>/dev/null
  [ -f "$RUN/spam.pid" ] && kill "$(cat "$RUN/spam.pid")" 2>/dev/null
  pkill -x spammer 2>/dev/null
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
  echo "==> deleting datadirs (.quake/$SCEN)…"
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
import sys,json;s=json.load(sys.stdin);g=s.get('growth',{});x=s.get('exec') or {}
print('dashboard : up  ('+('agree' if s['evm']['agree'] and s['pay']['agree'] else 'DIVERGENCE')+')')
def mb(kb): return '%.1f MB'%(kb/1024) if kb<1048576 else '%.2f GB'%(kb/1048576)
print('state     : EVM %s / PAY %s'%(mb(g.get('evm_state',0)),mb(g.get('pay_state',0))))
e=x.get('evm') or {}; p=x.get('pay') or {}
print('state-root: EVM %s ms / PAY %s ms'%(e.get('root_ms','-'),p.get('root_ms','-')))" 2>/dev/null
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
