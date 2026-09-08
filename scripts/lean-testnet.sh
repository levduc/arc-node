#!/usr/bin/env bash
# lean-testnet.sh up|load|status|down
#
# Local testnet WITH the lean payment lane: quake's localdev-lean scenario
# (5 validators, ARC_PAYMENT_LEAN_LANE=1 via the manifest's `cl.env` table) plus one
# lean-lane node per validator running on the host (ports 8561..8565), which
# the CL containers reach as host.docker.internal.
#
#   make testnet-lean            # = up   (builds the docker images first)
#   make testnet-lean-load       # = load (fan-out transactions, LOAD_SECS)
#   make testnet-lean-status     # = status
#   make testnet-lean-down       # = down
#
# The lean node comes from the separate lean-lane repo: LEAN_LANE_DIR
# (default ../lean-lane next to this checkout). `up` builds it if needed.
#
# `up` is always FRESH on both chains: quake `start --force` wipes the EVM
# chain, and a lean chain must never outlive the consensus chain that
# certified it (docs/lean-lane-integration.md §4).
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
EL_BASE=8545                   # quake: validator i's EL http = 8545 + (i-1)*100
CHAIN=${LEAN_CHAIN_ID:-1338}
RUN=$REPO/.quake/lean-nodes    # lean datadirs, logs, fund file
FUND=${LEAN_FUND_FILE:-$RUN/fund.txt}
FUND_ACCOUNTS=${FUND_ACCOUNTS:-200}
LOAD_SECS=${LOAD_SECS:-120}
LOAD_RATE=${LOAD_RATE:-1000}
FANOUT=${FANOUT:-10}
POOL_TARGET=${POOL_TARGET:-0}   # spammer closed-loop governor: pause while txpool depth > this (0 = open loop)

say(){ echo "[$(date +%H:%M:%S)] lean-testnet: $*"; }
die(){ echo "lean-testnet: FAIL: $*" >&2; exit 1; }
rpc(){ # rpc <url> <method> <json-params>   (body via file: block bytes are ~1 MB)
  local body; body=$(mktemp)
  printf '{"jsonrpc":"2.0","id":1,"method":"%s","params":%s}' "$2" "$3" > "$body"
  curl -s -m 20 -X POST "$1" -H 'content-type: application/json' --data-binary "@$body"; rm -f "$body"; }
jget(){ python3 -c 'import sys,json
d=json.load(sys.stdin)
for k in sys.argv[1].split("."):
    d=d[k]
print(d)' "$1" 2>/dev/null; }
lean_url(){ echo "http://127.0.0.1:$((BASE+$1))"; }
el_url(){ echo "http://127.0.0.1:$((EL_BASE+($1-1)*100))"; }
lean_pids(){ for i in $(seq 1 "$N"); do
    ss -ltnp 2>/dev/null | grep ":$((BASE+i)) " | grep -oP 'pid=\K[0-9]+' | head -1; done; }
stop_lean_nodes(){ for p in $(lean_pids); do kill "$p" 2>/dev/null; done; sleep 1
  for p in $(lean_pids); do kill -9 "$p" 2>/dev/null; done; }

build_lean(){
  [ -d "$LEAN_LANE_DIR" ] || die "lean-lane repo not found at $LEAN_LANE_DIR (set LEAN_LANE_DIR)"
  if [ ! -x "$LEAN_BIN" ] || [ ! -x "$LEAN_SPAMMER" ]; then
    say "building lean-lane-node + spammer in $LEAN_LANE_DIR (first time is slow)"
    cargo build --release --manifest-path "$LEAN_LANE_DIR/Cargo.toml" -p lean-lane-node -p spammer \
      || die "lean-lane build failed"
  fi
  [ -x "$LEAN_BIN" ] || die "no lean node binary at $LEAN_BIN"
  [ -x "$LEAN_SPAMMER" ] || die "no spammer binary at $LEAN_SPAMMER"
}

up(){
  [ -f "$MANIFEST" ] || die "no scenario $MANIFEST"
  build_lean
  say "stopping any previous lean nodes and wiping $RUN"
  stop_lean_nodes; rm -rf "$RUN"; mkdir -p "$RUN"
  if [ ! -s "$FUND" ]; then
    say "generating $FUND_ACCOUNTS funded accounts (m/44'/60'/1'/0/i) -> $FUND"
    bash "$LEAN_LANE_DIR/scripts/gen-lean-fund.sh" "$FUND_ACCOUNTS" "$FUND" >/dev/null 2>&1 \
      || die "fund-file generation failed (foundry's cast on PATH?)"
  fi
  for i in $(seq 1 "$N"); do
    peers=""; for j in $(seq 1 "$N"); do [ "$j" = "$i" ] || peers="${peers}http://127.0.0.1:$((BASE+j)),"; done
    (setsid nohup "$LEAN_BIN" run --datadir "$RUN/n$i" --port $((BASE+i)) --bind 0.0.0.0 \
        --chain-id "$CHAIN" --shim --peers "${peers%,}" \
        --fund-file "$FUND" --fund-balance 10000000000000000000 \
        > "$RUN/n$i.log" 2>&1 < /dev/null &)
  done
  for i in $(seq 1 "$N"); do
    ok=0; for _ in $(seq 1 60); do
      rpc "$(lean_url "$i")" arc_getHead '{}' | grep -q commitment && { ok=1; break; }; sleep 1; done
    [ $ok = 1 ] || { tail -5 "$RUN/n$i.log"; die "lean node $i did not come up on :$((BASE+i))"; }
  done
  g1=$(rpc "$(lean_url 1)" arc_getHead '{}' | jget result.commitment)
  for i in $(seq 2 "$N"); do
    [ "$(rpc "$(lean_url "$i")" arc_getHead '{}' | jget result.commitment)" = "$g1" ] \
      || die "lean genesis mismatch on node $i (different fund file?)"
  done
  say "$N lean nodes up on $((BASE+1))..$((BASE+N)), genesis ${g1:0:18}..."

  # `start --force` keeps existing EL/CL data (which would put a fresh lean
  # chain under an old certified chain) and can reuse a compose file rendered
  # by an older quake. Wipe the whole generated testnet first, then verify
  # the EVM chain really is new.
  say "quake clean --all, then start -f $MANIFEST (fresh EVM chain)"
  # shellcheck disable=SC2086
  $QUAKE -f "$MANIFEST" stop > "$RUN/quake.log" 2>&1 || true
  # shellcheck disable=SC2086
  $QUAKE -f "$MANIFEST" clean --all >> "$RUN/quake.log" 2>&1 || true
  # The containers write their data as root; remove what `clean` could not.
  if [ -d ".quake/$SCEN" ]; then
    docker run --rm -v "$REPO/.quake/$SCEN":/b --user root alpine \
      sh -c 'rm -rf /b/validator*/reth /b/validator*/malachite/store.db /b/validator*/malachite/wal /b/full*/reth /b/full*/malachite/store.db /b/full*/malachite/wal' \
      >> "$RUN/quake.log" 2>&1 || true
  fi
  # shellcheck disable=SC2086
  $QUAKE -f "$MANIFEST" start --monitoring "${MONITORING:-false}" --force >> "$RUN/quake.log" 2>&1 \
    || { tail -20 "$RUN/quake.log"; die "quake start failed (see $RUN/quake.log)"; }
  say "waiting for the chains to move"
  ok=0; for _ in $(seq 1 120); do
    parked=$(for i in $(seq 1 "$N"); do docker logs "validator${i}_cl" 2>&1 | grep -c "Manual intervention"; done | paste -sd+ | bc)
    [ "${parked:-0}" = 0 ] || die "a CL parked at boot (lean node unreachable?) — docker logs validator*_cl"
    h=$(rpc "$(lean_url 1)" arc_getHead '{}' | jget result.number)
    [ "${h:-0}" -ge 3 ] 2>/dev/null && { ok=1; break; }
    sleep 1
  done
  [ $ok = 1 ] || die "lean chain did not reach height 3 within 120 s — see $RUN/quake.log and docker logs validator1_cl"
  el=$(rpc "$(el_url 1)" eth_blockNumber '[]' | jget result); el=$((el))
  [ "$el" -lt 60 ] || die "EVM chain is NOT fresh (height $el): the old certified chain would sit over a new lean chain. Run '$0 down' then 'quake clean --data' and retry"
  status
  say "up. next: '$0 load' (or make testnet-lean-load), then '$0 status'"
}

load(){
  build_lean
  targets=""; for i in $(seq 1 "$N"); do targets="${targets}ws://127.0.0.1:$((BASE+i)),"; done
  say "fan-out load: ${LOAD_RATE} tx/s x ${LOAD_SECS}s, N=${FANOUT} outputs/tx, ${FUND_ACCOUNTS} accounts, pool-target ${POOL_TARGET}, all $N nodes"
  "$LEAN_SPAMMER" ws --targets "${targets%,}" -r "$LOAD_RATE" -g 2 -a "$FUND_ACCOUNTS" -t "$LOAD_SECS" \
    --pool-target "$POOL_TARGET" \
    --chain-id "$CHAIN" --mix fanout=100 --fanout-outputs "$FANOUT" -l 2>&1 | tail -5
  status
}

status(){
  echo "validator   EL height   lean height   head txs   lean head"
  nums=()
  for i in $(seq 1 "$N"); do
    el=$(rpc "$(el_url "$i")" eth_blockNumber '[]' | jget result); el=$((el))
    head=$(rpc "$(lean_url "$i")" arc_getHead '{}')
    n=$(echo "$head" | jget result.number); c=$(echo "$head" | jget result.commitment)
    nums+=("${n:-0}")
    # tx count of the head block: a number without fullness measures delivery, not the chain
    txs=$(rpc "$(lean_url "$i")" arc_getBlockBytes "{\"number\":${n:-0}}" | python3 -c 'import sys,json,base64
b=base64.b64decode(json.load(sys.stdin)["result"]["blockBytes"] or ""); print(int.from_bytes(b[48:52],"little") if len(b)>=52 else 0)' 2>/dev/null)
    printf 'validator%-2s  %-10s  %-12s  %-9s  %s\n' "$i" "$el" "${n:-?}" "${txs:-?}" "${c:0:18}..."
  done
  # agreement: the block at the lowest common lean height must be byte-identical everywhere
  min=$(printf '%s\n' "${nums[@]}" | sort -n | head -1)
  if [ "${min:-0}" -gt 0 ] 2>/dev/null; then
    distinct=$(for i in $(seq 1 "$N"); do
      rpc "$(lean_url "$i")" arc_getBlockBytes "{\"number\":$min}" | jget result.blockBytes | sha256sum | cut -c1-16; done | sort -u | wc -l)
    [ "$distinct" = 1 ] && echo "agreement: all $N lean nodes identical at height $min" \
                        || echo "DIVERGENCE: $distinct distinct lean blocks at height $min"
  fi
}

down(){
  say "quake stop"
  # shellcheck disable=SC2086
  $QUAKE -f "$MANIFEST" stop > /dev/null 2>&1 || true
  stop_lean_nodes
  say "lean nodes stopped; data kept in $RUN (wiped by the next 'up')"
}

case "${1:-}" in
  up) up ;; load) load ;; status) status ;; down) down ;;
  *) echo "usage: $0 up|load|status|down"; exit 2 ;;
esac
