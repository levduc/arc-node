#!/usr/bin/env bash
# lean-testnet.sh up | load | status | down | restart <n>
#
# Local testnet with the lean payment lane: quake's localdev-lean scenario (5
# validators, ARC_PAYMENT_LEAN_LANE=1) plus one lean node per validator on the
# host, on ports 8561..8565, which the CL containers reach as
# host.docker.internal. See docs/lean-lane-integration.md.
#
# The lean node and its load generator come from a lean-lane checkout:
# LEAN_LANE_DIR (default: ../lean-lane next to this repository).
#
# `up` always starts both chains fresh: a lean chain must never outlive the
# consensus chain that certified it.
set -uo pipefail
REPO=$(cd "$(dirname "$0")/.." && pwd); cd "$REPO" || exit 1

SCEN=${SCENARIO:-localdev-lean}
MANIFEST=crates/quake/scenarios/$SCEN.toml
LEAN_LANE_DIR=${LEAN_LANE_DIR:-$REPO/../lean-lane}
LEAN_BIN=${LEAN_BIN:-$LEAN_LANE_DIR/target/release/lean-lane-node}
LEAN_SPAMMER=${LEAN_SPAMMER:-$LEAN_LANE_DIR/target/release/spammer}
QUAKE=${QUAKE:-cargo run --release --bin quake --}
N=${LEAN_VALIDATORS:-5}
BASE=8560                      # lean node i listens on BASE+i
EL_BASE=8545                   # quake: validator i's EL RPC is 8545 + (i-1)*100
CHAIN=${LEAN_CHAIN_ID:-1338}
RUN=$REPO/.quake/lean-nodes    # lean datadirs, logs, fund file
FUND=${LEAN_FUND_FILE:-$RUN/fund.txt}
FUND_ACCOUNTS=${FUND_ACCOUNTS:-200}
LOAD_SECS=${LOAD_SECS:-120}
LOAD_RATE=${LOAD_RATE:-1000}
FANOUT=${FANOUT:-10}
POOL_TARGET=${POOL_TARGET:-0}  # spammer pauses while a pool holds more than this (0: open loop)

say(){ echo "[$(date +%H:%M:%S)] lean-testnet: $*"; }
die(){ echo "lean-testnet: FAIL: $*" >&2; exit 1; }
rpc(){ # rpc <url> <method> <json-params>; the body goes through a file (blocks are ~1 MB)
  local body; body=$(mktemp)
  printf '{"jsonrpc":"2.0","id":1,"method":"%s","params":%s}' "$2" "$3" > "$body"
  curl -s -m 20 -X POST "$1" -H 'content-type: application/json' --data-binary "@$body"; rm -f "$body"; }
jget(){ python3 -c 'import sys,json
d=json.load(sys.stdin)
for k in sys.argv[1].split("."): d=d[k]
print(d)' "$1" 2>/dev/null; }
lean_url(){ echo "http://127.0.0.1:$((BASE+$1))"; }
el_url(){ echo "http://127.0.0.1:$((EL_BASE+($1-1)*100))"; }
head_of(){ rpc "$(lean_url "$1")" arc_getHead '{}' | jget "result.$2"; }
lean_pids(){ for i in $(seq 1 "$N"); do
  ss -ltnp 2>/dev/null | grep ":$((BASE+i)) " | grep -oP 'pid=\K[0-9]+' | head -1; done; }
stop_lean_nodes(){ for p in $(lean_pids); do kill "$p" 2>/dev/null; done; sleep 1
  for p in $(lean_pids); do kill -9 "$p" 2>/dev/null; done; }

build_lean(){
  [ -d "$LEAN_LANE_DIR" ] || die "no lean-lane checkout at $LEAN_LANE_DIR (set LEAN_LANE_DIR)"
  if [ ! -x "$LEAN_BIN" ] || [ ! -x "$LEAN_SPAMMER" ]; then
    say "building lean-lane-node and spammer in $LEAN_LANE_DIR"
    cargo build --release --manifest-path "$LEAN_LANE_DIR/Cargo.toml" -p lean-lane-node -p spammer \
      || die "lean-lane build failed"
  fi
}

up(){
  [ -f "$MANIFEST" ] || die "no scenario $MANIFEST"
  build_lean
  stop_lean_nodes; rm -rf "$RUN"; mkdir -p "$RUN"
  [ -s "$FUND" ] || bash "$LEAN_LANE_DIR/scripts/gen-lean-fund.sh" "$FUND_ACCOUNTS" "$FUND" >/dev/null 2>&1 \
    || die "fund file generation failed (is foundry's cast on PATH?)"
  for i in $(seq 1 "$N"); do
    peers=""; for j in $(seq 1 "$N"); do [ "$j" = "$i" ] || peers="$peers$(lean_url "$j"),"; done
    (setsid nohup "$LEAN_BIN" run --datadir "$RUN/n$i" --port $((BASE+i)) --bind 0.0.0.0 \
        --chain-id "$CHAIN" --shim --peers "${peers%,}" \
        --fund-file "$FUND" --fund-balance 10000000000000000000 \
        > "$RUN/n$i.log" 2>&1 < /dev/null &)
  done
  for i in $(seq 1 "$N"); do
    for _ in $(seq 1 60); do [ -n "$(head_of "$i" commitment)" ] && break; sleep 1; done
    [ -n "$(head_of "$i" commitment)" ] || { tail -5 "$RUN/n$i.log"; die "lean node $i did not start"; }
    [ "$(head_of "$i" commitment)" = "$(head_of 1 commitment)" ] || die "lean genesis differs on node $i"
  done
  say "$N lean nodes up on ports $((BASE+1))..$((BASE+N))"

  # A fresh EVM chain: stale EL/CL data would put an old certified chain over
  # a new lean chain. The containers write as root, hence the docker rm.
  $QUAKE -f "$MANIFEST" stop > "$RUN/quake.log" 2>&1 || true
  $QUAKE -f "$MANIFEST" clean --all >> "$RUN/quake.log" 2>&1 || true
  [ -d ".quake/$SCEN" ] && docker run --rm -v "$REPO/.quake/$SCEN":/b --user root alpine \
    sh -c 'rm -rf /b/validator*/reth /b/validator*/malachite/store.db /b/validator*/malachite/wal' \
    >> "$RUN/quake.log" 2>&1
  $QUAKE -f "$MANIFEST" start --monitoring "${MONITORING:-false}" --force >> "$RUN/quake.log" 2>&1 \
    || { tail -20 "$RUN/quake.log"; die "quake start failed (see $RUN/quake.log)"; }
  for _ in $(seq 1 120); do [ "$(head_of 1 number)" -ge 3 ] 2>/dev/null && break; sleep 1; done
  [ "$(head_of 1 number)" -ge 3 ] 2>/dev/null || die "lean chain did not reach height 3 (docker logs validator1_cl)"
  el=$(rpc "$(el_url 1)" eth_blockNumber '[]' | jget result); el=$((el))
  [ "$el" -lt 60 ] || die "the EVM chain is not fresh (height $el); run '$0 down' and 'quake clean --all'"
  status
}

load(){
  build_lean
  targets=""; for i in $(seq 1 "$N"); do targets="${targets}ws://127.0.0.1:$((BASE+i)),"; done
  say "load: $LOAD_RATE tx/s for ${LOAD_SECS}s, $FANOUT outputs per tx, $FUND_ACCOUNTS accounts"
  "$LEAN_SPAMMER" ws --targets "${targets%,}" -r "$LOAD_RATE" -g 2 -a "$FUND_ACCOUNTS" -t "$LOAD_SECS" \
    --pool-target "$POOL_TARGET" --chain-id "$CHAIN" --mix fanout=100 --fanout-outputs "$FANOUT" -l \
    2>&1 | tail -5
  status
}

status(){
  echo "validator   EL height   lean height   head txs   lean head"
  min=""
  for i in $(seq 1 "$N"); do
    el=$(rpc "$(el_url "$i")" eth_blockNumber '[]' | jget result); el=$((el))
    n=$(head_of "$i" number); c=$(head_of "$i" commitment)
    # txs in the head block: a number without fullness measures delivery, not the chain
    txs=$(rpc "$(lean_url "$i")" arc_getBlockBytes "{\"number\":${n:-0}}" | python3 -c 'import sys,json,base64
b=base64.b64decode(json.load(sys.stdin)["result"]["blockBytes"] or ""); print(int.from_bytes(b[48:52],"little") if len(b)>=52 else 0)' 2>/dev/null)
    printf 'validator%-2s  %-10s  %-12s  %-9s  %s\n' "$i" "$el" "${n:-?}" "${txs:-?}" "${c:0:18}"
    [ -z "$min" ] || [ "${n:-0}" -lt "$min" ] && min=${n:-0}
  done
  [ "${min:-0}" -gt 0 ] || return 0
  distinct=$(for i in $(seq 1 "$N"); do rpc "$(lean_url "$i")" arc_getBlockBytes "{\"number\":$min}" \
    | jget result.blockBytes | sha256sum; done | sort -u | wc -l)
  [ "$distinct" = 1 ] && echo "agreement: all $N lean nodes identical at height $min" \
                      || echo "DIVERGENCE: $distinct distinct lean blocks at height $min"
}

down(){
  $QUAKE -f "$MANIFEST" stop > /dev/null 2>&1 || true
  stop_lean_nodes
  say "stopped; lean data kept in $RUN until the next 'up'"
}

restart(){ # restart <n>: the CL container only; its lean node stays up
  local i=${1:?validator index}
  docker restart "validator${i}_cl" > /dev/null || die "no container validator${i}_cl"
  for _ in $(seq 1 60); do
    max=0; for j in $(seq 1 "$N"); do h=$(head_of "$j" number); [ "${h:-0}" -gt "$max" ] && max=$h; done
    [ $((max - $(head_of "$i" number))) -le 3 ] && { say "validator$i within 3 heights of the tip"; return 0; }
    sleep 2
  done
  die "validator$i did not catch up after the restart"
}

case "${1:-}" in
  up) up ;; load) load ;; status) status ;; down) down ;; restart) restart "${2:-}" ;;
  *) echo "usage: $0 up|load|status|down|restart <n>"; exit 2 ;;
esac
