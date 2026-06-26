#!/usr/bin/env bash
# Baseline arc-node-execution Engine-API import-throughput benchmark.
#
# Flow (no Docker; isolated from any running testnet — uses its own ports/PIDs):
#   1. Run ONE dev node (SOURCE) from genesis and drive load into it so EVERY
#      block it mines is full of transactions.
#   2. Snapshot blocks 1..HEAD into a payload fixture (prepare-payload).
#   3. Replay that fixture into a FRESH node (TARGET, also at genesis) over the
#      Engine API (new-payload-fcu) and record latency + aggregate throughput.
#
# Output: target/engine-bench/baseline-run/summary.csv
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
. "$HOME/.cargo/env" 2>/dev/null || true
export NVM_DIR="$HOME/.nvm"; . "$NVM_DIR/nvm.sh" 2>/dev/null || true; nvm use 22 >/dev/null 2>&1 || true

BIN=target/release/arc-node-execution
BENCH=target/release/arc-engine-bench
SPAMMER=target/release/spammer
CHAIN=arc-localdev
WORK="$ROOT/.bench"; SRC_DIR="$WORK/src"; TGT_DIR="$WORK/tgt"
PAYLOAD_DIR="$ROOT/target/engine-bench/payload-fixture"
OUT="$ROOT/target/engine-bench/baseline-run"

# Ports chosen to NOT collide with a running quake testnet (8545/8645/.. , 31000+)
SRC_HTTP=7600; SRC_WS=7601; SRC_METRICS=19000
TGT_HTTP=7545; TGT_METRICS=19001
ENGINE_IPC="$TGT_DIR/auth.ipc"

RATE="${RATE:-3000}"; TIME="${TIME:-60}"; BLOCK_TIME="${BLOCK_TIME:-1s}"; MIX="${MIX:-transfer=100}"
RPC_S="http://127.0.0.1:$SRC_HTTP"; RPC_T="http://127.0.0.1:$TGT_HTTP"

SRC_PID=""; TGT_PID=""
log(){ echo -e "\n=== $* ==="; }
cleanup(){ [ -n "$SRC_PID" ] && kill "$SRC_PID" 2>/dev/null; [ -n "$TGT_PID" ] && kill "$TGT_PID" 2>/dev/null; }
trap cleanup EXIT
wait_rpc(){ for _ in $(seq 1 90); do cast block-number --rpc-url "$1" >/dev/null 2>&1 && return 0; sleep 1; done; return 1; }

rm -rf "$WORK"; mkdir -p "$SRC_DIR" "$TGT_DIR"

log "1/5  start SOURCE dev node (genesis, auto-mine every $BLOCK_TIME) on :$SRC_HTTP/:$SRC_WS"
"$BIN" node --chain "$CHAIN" --datadir "$SRC_DIR" \
  --dev --dev.block-time "$BLOCK_TIME" --disable-discovery --port 30401 \
  --http --http.addr 127.0.0.1 --http.port "$SRC_HTTP" --http.api eth,net,web3,txpool \
  --ws   --ws.addr   127.0.0.1 --ws.port   "$SRC_WS"   --ws.api   eth,net,web3,txpool \
  --authrpc.addr 127.0.0.1 --authrpc.port 7551 \
  --metrics 127.0.0.1:"$SRC_METRICS" > "$WORK/src.log" 2>&1 &
SRC_PID=$!
wait_rpc "$RPC_S" || { echo "SOURCE never came up:"; tail -30 "$WORK/src.log"; exit 1; }
echo "source up (pid $SRC_PID), head=$(cast block-number --rpc-url "$RPC_S")"

log "2/5  drive load: $MIX at $RATE TPS for ${TIME}s"
"$SPAMMER" ws --targets "127.0.0.1:$SRC_WS" -r "$RATE" -t "$TIME" --mix "$MIX" 2>&1 | tail -6 || true
sleep 3
HEAD="$(cast block-number --rpc-url "$RPC_S")"
echo "source head after load: $HEAD"
[ "$HEAD" -ge 2 ] || { echo "ERROR: no blocks mined; src.log:"; tail -30 "$WORK/src.log"; exit 1; }

log "3/5  snapshot blocks 1..$HEAD into fixture"
rm -rf "$PAYLOAD_DIR"
"$BENCH" prepare-payload --chain "$CHAIN" --source-rpc-url "$RPC_S" \
  --from 1 --to "$HEAD" --output-dir "$PAYLOAD_DIR" || { echo "prepare-payload failed"; exit 1; }
kill "$SRC_PID" 2>/dev/null; SRC_PID=""; sleep 2

log "4/5  start fresh TARGET node (genesis, engine via IPC) on :$TGT_HTTP"
"$BIN" node --chain "$CHAIN" --datadir "$TGT_DIR" \
  --dev --disable-discovery --port 30402 \
  --http --http.addr 127.0.0.1 --http.port "$TGT_HTTP" --http.api eth \
  --authrpc.addr 127.0.0.1 --authrpc.port 7552 \
  --metrics 127.0.0.1:"$TGT_METRICS" \
  --auth-ipc --auth-ipc.path "$ENGINE_IPC" > "$WORK/tgt.log" 2>&1 &
TGT_PID=$!
wait_rpc "$RPC_T" || { echo "TARGET never came up:"; tail -30 "$WORK/tgt.log"; exit 1; }
echo "target up (pid $TGT_PID), head=$(cast block-number --rpc-url "$RPC_T")"

log "5/5  replay fixture into target and measure"
rm -rf "$OUT"
"$BENCH" new-payload-fcu --engine-ipc "$ENGINE_IPC" \
  --target-eth-rpc-url "$RPC_T" --payload "$PAYLOAD_DIR" --output "$OUT" || { echo "replay failed"; tail -30 "$WORK/tgt.log"; exit 1; }

log "RESULT — summary.csv  (workload: $MIX, blocks 1..$HEAD)"
cat "$OUT/summary.csv"
echo; echo "per-block detail: $OUT/combined_latency.csv"
