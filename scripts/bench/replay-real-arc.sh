#!/usr/bin/env bash
# Replay REAL Arc testnet blocks into a reth-2.3 arc-node-execution node.
#
# This is the "real data" counterpart to engine-baseline.sh: instead of a dev
# node fed by the dummy spammer, it boots the node on a downloaded arc-testnet
# SNAPSHOT (real deep state) and replays a handful of REAL blocks pulled from
# the public testnet RPC over the Engine API, reporting per-block import /
# state-root / throughput.
#
# COLD measurement: this replays each block for the FIRST time on deep state, so
# the trie pages it needs are read from disk (the realistic tip-of-chain case on
# a >RAM state). The per-stage split (EVM execution / state-root / persistence)
# comes from reth's Prometheus histograms on the --metrics endpoint: the script
# scrapes /metrics immediately before and after the replay and diffs the _sum of
# reth_sync_execution_execution_histogram (EVM), reth_sync_block_validation_state_
# root_histogram (state-root), and reth_consensus_engine_beacon_persistence_duration
# (persistence). These are reported as WALL-CLOCK totals per stage. The stages
# run in parallel (persistence is async and overlaps execution of later blocks),
# so the totals overlap — they are NOT summed and NOT turned into percentages.
# (The per-block `reth::slow_block` log is NOT used: it does not fire on the
# deep-state async state-root path.)
#
# Self-contained: missing release binaries (arc-node-execution, arc-engine-bench,
# arc-snapshots) are built on demand, and the arc-testnet snapshot is downloaded
# if not already present. A fresh checkout can run this and get numbers at the end
# (at the cost of a long first run: full build + ~169 GB snapshot download).
#
# Tunables (env):
#   EL_DATADIR   snapshot datadir to boot on          [~/.arc/execution]
#   SOURCE_RPC   public RPC to fetch real blocks       [https://rpc.testnet.arc.network]
#   BLOCKS       how many real blocks to replay        [100]
#   DOWNLOAD     1 = auto-download snapshot if missing; 0 = require pre-existing  [1]
#   MIGRATE      1 = db migrate-v2 (Storage V2) first; 0 = open V1 legacy in place [0]
#   DROP_CACHES  1 = drop the OS page cache before replay for a genuinely cold run [1]
#                (macOS `purge`; Linux `drop_caches` via sudo). Best-effort:
#                warns and continues if it lacks privileges. 0 = skip (softer cold).
#   PBUF         engine in-memory block buffer target [20000]  (keeps replay in RAM,
#                so state-root is the synchronous measured path and the pruned-snapshot
#                senders/persistence bug can't crash the run)
#
# Output: target/engine-bench/real-arc-run/
#           summary.csv           engine-bench aggregate (latency percentiles, throughput)
#           combined_latency.csv  per-block new_payload/fcu/total latency
#           report.txt            provenance header + cold per-stage split (also printed)
#         plus .bench-real/metrics_before.prom / metrics_after.prom (raw scrapes)
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"; cd "$ROOT"
. "$HOME/.cargo/env" 2>/dev/null || true

BIN=target/release/arc-node-execution
BENCH=target/release/arc-engine-bench
SNAP=target/release/arc-snapshots
CHAIN=arc-testnet
EL_DATADIR="${EL_DATADIR:-$HOME/.arc/execution}"
SOURCE_RPC="${SOURCE_RPC:-https://rpc.quicknode.testnet.arc.network}"
BLOCKS="${BLOCKS:-100}"
MIGRATE="${MIGRATE:-0}"
PBUF="${PBUF:-20000}"

WORK="$ROOT/.bench-real"
PAYLOAD_DIR="$ROOT/target/engine-bench/real-arc-payload"
OUT="$ROOT/target/engine-bench/real-arc-run"
ENGINE_IPC="$WORK/auth.ipc"
TGT_HTTP=7545; TGT_METRICS=19001; P2P_PORT=30403
RPC_T="http://127.0.0.1:$TGT_HTTP"
METRICS_URL="http://127.0.0.1:$TGT_METRICS"

# Exact reth Prometheus histograms for the per-stage split (all record seconds):
M_EXEC=reth_sync_execution_execution_histogram                 # EVM execution
M_ROOT=reth_sync_block_validation_state_root_histogram         # state-root / trie
M_PERSIST=reth_consensus_engine_beacon_persistence_duration    # disk persistence

TGT_PID=""
log(){ echo -e "\n=== $* ==="; }
cleanup(){ [ -n "$TGT_PID" ] && kill "$TGT_PID" 2>/dev/null; }   # kill ONLY our node, never pkill
trap cleanup EXIT

# Query a node's head via raw RPC (cast in a loop times out on a busy node).
rpc_head_dec(){
  local hex
  hex="$(curl -s -m 15 -X POST "$1" -H 'content-type: application/json' \
        --data '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' \
        | sed -E 's/.*"result":"0x([0-9a-fA-F]+)".*/\1/')"
  [ -n "$hex" ] && printf '%d\n' "$((16#$hex))" || return 1
}
wait_rpc(){ for _ in $(seq 1 600); do rpc_head_dec "$1" >/dev/null 2>&1 && return 0; sleep 1; done; return 1; }

# Build a release binary on demand if its artifact is missing, so a fresh
# checkout can run this end-to-end without a manual `cargo build` first.
ensure_bin(){  # $1 = artifact path, $2 = cargo bin name
  [ -x "$1" ] && return 0
  log "building $2 (missing $1) — one-time, may take a while"
  cargo build --release --bin "$2" || { echo "failed to build $2"; exit 1; }
  [ -x "$1" ] || { echo "still missing $1 after building $2"; exit 1; }
}

# Drop the OS page cache so the replay reads trie pages from disk (a real cold
# run). Best-effort: warns and continues if it can't (e.g. no sudo on Linux).
drop_os_cache(){
  case "$(uname -s)" in
    Darwin) if purge 2>/dev/null; then echo "  dropped OS cache (purge)"; else
              echo "  WARN: purge failed — cold may be soft (OS cache not dropped)"; fi ;;
    Linux)  if sync && echo 3 | sudo -n tee /proc/sys/vm/drop_caches >/dev/null 2>&1; then
              echo "  dropped OS cache (drop_caches)"; else
              echo "  WARN: could not drop_caches (need root) — cold may be soft"; fi ;;
    *)      echo "  WARN: unknown OS — skipping cache drop (cold may be soft)" ;;
  esac
}

# Sum a Prometheus metric's value across all label series in a scrape file.
# Strips the {labels} so `name` and `name{...}` both match the given base name.
metric_val(){  # $1 = scrape file, $2 = exact metric name (no labels/suffix)
  awk -v p="$2" '{ n=$1; sub(/\{.*/,"",n); if(n==p) s+=$2 } END{ printf "%.9f", s+0 }' "$1" 2>/dev/null
}

# ---- preflight -------------------------------------------------------------
log "preflight"
ensure_bin "$BIN"   arc-node-execution
ensure_bin "$BENCH" arc-engine-bench

# Snapshot: real arc-testnet deep state. Download+extract it if not already
# present so a fresh machine can run this end-to-end. This is a LARGE download
# (EL ~169 GB, plus the CL archive) and can take ~1-2h; it is skipped whenever
# $EL_DATADIR/db already exists. Set DOWNLOAD=0 to require a pre-existing
# snapshot instead (fail fast rather than pull 169 GB).
DOWNLOAD="${DOWNLOAD:-1}"
if [ ! -d "$EL_DATADIR/db" ]; then
  [ "$DOWNLOAD" = "1" ] || { echo "no snapshot db at $EL_DATADIR/db and DOWNLOAD=0 — run arc-snapshots download first"; exit 1; }
  ensure_bin "$SNAP" arc-snapshots
  log "no snapshot at $EL_DATADIR/db — downloading arc-testnet snapshot (LARGE: EL ~169 GB, ~1-2h)"
  "$SNAP" download --chain "$CHAIN" --execution-path "$EL_DATADIR" \
    || { echo "snapshot download failed"; exit 1; }
  [ -d "$EL_DATADIR/db" ] || { echo "download finished but no db at $EL_DATADIR/db"; exit 1; }
else
  echo "snapshot present at $EL_DATADIR/db — skipping download"
fi

SRC_TIP="$(rpc_head_dec "$SOURCE_RPC")" || { echo "source RPC unreachable: $SOURCE_RPC"; exit 1; }
echo "source tip: $SRC_TIP   datadir: $EL_DATADIR   blocks: $BLOCKS   migrate-v2: $MIGRATE"
mkdir -p "$WORK"

# ---- optional V1 -> V2 migration ------------------------------------------
if [ "$MIGRATE" = "1" ]; then
  log "db migrate-v2 (V1 MDBX -> V2 static-files+RocksDB) — slow (~tens of min)"
  "$BIN" db migrate-v2 --chain "$CHAIN" --datadir "$EL_DATADIR" || { echo "migrate-v2 failed"; exit 1; }
fi

# ---- boot the target node on the snapshot ---------------------------------
# No --dev: this is a real chain. reth-2.3 opens the datadir with the storage
# version recorded in its metadata (V1 snapshot => legacy V1 unless migrated).
log "boot arc-node-execution on snapshot (http :$TGT_HTTP, engine via IPC, metrics :$TGT_METRICS) — first boot may take minutes"
# The per-stage split (EVM exec / state-root / persistence) is read from reth's
# Prometheus histograms on --metrics (scraped before/after replay below), which
# are always recorded — unlike the slow-block log, which does not fire on the
# deep-state async state-root path.
"$BIN" node --chain "$CHAIN" --datadir "$EL_DATADIR" \
  --disable-discovery --port "$P2P_PORT" \
  --http --http.addr 127.0.0.1 --http.port "$TGT_HTTP" --http.api eth,net,web3 \
  --metrics 127.0.0.1:"$TGT_METRICS" \
  --auth-ipc --auth-ipc.path "$ENGINE_IPC" \
  --engine.persistence-threshold "$PBUF" --engine.memory-block-buffer-target "$PBUF" \
  > "$WORK/node.log" 2>&1 &
TGT_PID=$!
wait_rpc "$RPC_T" || { echo "target node never came up:"; tail -40 "$WORK/node.log"; exit 1; }
H="$(rpc_head_dec "$RPC_T")"
echo "target up (pid $TGT_PID) at snapshot head H=$H"

# ---- pick the real block range to replay ----------------------------------
FROM=$((H + 1)); TO=$((H + BLOCKS))
if [ "$TO" -gt "$SRC_TIP" ]; then TO="$SRC_TIP"; fi
[ "$TO" -ge "$FROM" ] || { echo "nothing to replay (H=$H, source tip=$SRC_TIP)"; exit 1; }
echo "replaying real blocks $FROM..$TO from $SOURCE_RPC"

# ---- fetch the real payloads ----------------------------------------------
# batch-size 1 + retry: the public RPC is load-balanced across backends with
# inconsistent prune state, so batched fetches occasionally return a transient
# null for a block that actually exists. One block per request + retry avoids it.
log "prepare-payload $FROM..$TO"
prepared=0
for attempt in 1 2 3 4 5; do
  rm -rf "$PAYLOAD_DIR"
  if "$BENCH" prepare-payload --chain "$CHAIN" --source-rpc-url "$SOURCE_RPC" \
       --from "$FROM" --to "$TO" --batch-size 1 --eth-rpc-timeout-ms 30000 \
       --output-dir "$PAYLOAD_DIR" && [ -f "$PAYLOAD_DIR/metadata.json" ]; then
    prepared=1; break
  fi
  echo "prepare-payload attempt $attempt failed; retrying in 5s"; sleep 5
done
[ "$prepared" = 1 ] || { echo "prepare-payload failed after retries"; exit 1; }

# ---- drop OS cache for a cold read (best-effort) --------------------------
# Do this AFTER boot + prepare-payload (neither of which should stay cached) and
# immediately BEFORE replay, so the trie pages the replay touches fault in from
# disk. The node's own in-process caches are already cold on a fresh boot.
DROP_CACHES="${DROP_CACHES:-1}"
if [ "$DROP_CACHES" = "1" ]; then
  log "drop OS page cache (cold run)"; drop_os_cache
  DROPPED=yes
else
  DROPPED=no
fi

# ---- snapshot Prometheus metrics BEFORE replay ----------------------------
BEFORE="$WORK/metrics_before.prom"; AFTER="$WORK/metrics_after.prom"
curl -s -m 15 "$METRICS_URL" > "$BEFORE" || { echo "could not scrape $METRICS_URL"; exit 1; }
[ -s "$BEFORE" ] || { echo "empty metrics scrape from $METRICS_URL"; exit 1; }

# ---- replay into the target and measure -----------------------------------
log "new-payload-fcu replay -> $OUT"
rm -rf "$OUT"
"$BENCH" new-payload-fcu --engine-ipc "$ENGINE_IPC" \
  --target-eth-rpc-url "$RPC_T" --payload "$PAYLOAD_DIR" --output "$OUT" \
  || { echo "replay failed"; tail -40 "$WORK/node.log"; exit 1; }

# ---- snapshot Prometheus metrics AFTER replay (before the node is killed) --
# Persistence is async; pause briefly so in-flight commits flush into the
# histogram before we read it.
sleep 3
curl -s -m 15 "$METRICS_URL" > "$AFTER" || { echo "could not scrape $METRICS_URL (post)"; exit 1; }

# ---- per-stage split from Prometheus histogram deltas ---------------------
# Each histogram records SECONDS; the _sum delta over the run is the total time
# spent in that stage, and the ratio of the three sums is the split. Auto-detect
# whether the exporter used `<base>_sum` or `<base>_seconds_sum`.
log "compute per-stage split from Prometheus histogram deltas"
# stage_delta BASE -> "<delta_sum_seconds> <delta_count> <detected_metric_name>"
stage_delta(){
  local sname
  sname="$(awk -v b="$1" '{n=$1;sub(/\{.*/,"",n); if(n==b"_sum"||n==b"_seconds_sum"){print n; exit}}' "$AFTER")"
  [ -z "$sname" ] && { echo "0 0 (not-found:$1)"; return; }
  awk -v s="$sname" -v c="${sname%_sum}_count" '
    FNR==NR{ n=$1; sub(/\{.*/,"",n); if(n==s)bs+=$2; if(n==c)bc+=$2; next }
           { n=$1; sub(/\{.*/,"",n); if(n==s)as+=$2; if(n==c)ac+=$2 }
    END{ printf "%.6f %d %s", (as-bs), (ac-bc), s }' "$BEFORE" "$AFTER"
}
read -r EXEC_S EXEC_N EXEC_M <<EOF
$(stage_delta "$M_EXEC")
EOF
read -r ROOT_S ROOT_N ROOT_M <<EOF
$(stage_delta "$M_ROOT")
EOF
read -r PERS_S PERS_N PERS_M <<EOF
$(stage_delta "$M_PERSIST")
EOF

# ---- provenance ------------------------------------------------------------
case "$(uname -s)" in
  Darwin) CPU="$(sysctl -n machdep.cpu.brand_string 2>/dev/null)"; CORES="$(sysctl -n hw.ncpu 2>/dev/null)"
          RAM_GB="$(awk -v b="$(sysctl -n hw.memsize 2>/dev/null)" 'BEGIN{printf "%.0f", b/1073741824}')" ;;
  *)      CPU="$(awk -F: '/model name/{print $2; exit}' /proc/cpuinfo 2>/dev/null | sed 's/^ //')"; CORES="$(nproc 2>/dev/null)"
          RAM_GB="$(awk '/MemTotal/{printf "%.0f", $2/1048576}' /proc/meminfo 2>/dev/null)" ;;
esac
if [ -d "$EL_DATADIR/static_files" ] && [ -d "$EL_DATADIR/rocksdb" ]; then STORAGE="v2 (static_files+rocksdb present)"; else STORAGE="v1 (legacy MDBX)"; fi
RETH_VER="$("$BIN" --version 2>/dev/null | head -1)"
NB=$((TO - FROM + 1))

# ---- report ----------------------------------------------------------------
REPORT="$OUT/report.txt"
{
  echo "=== Arc real-block execution benchmark (COLD) ==="
  echo "chain          : $CHAIN"
  echo "blocks         : $FROM..$TO ($NB blocks; BLOCKS=$BLOCKS)"
  echo "datadir        : $EL_DATADIR"
  echo "storage        : $STORAGE   (MIGRATE=$MIGRATE)"
  echo "cache          : cold run; OS page cache dropped = $DROPPED (DROP_CACHES=$DROP_CACHES)"
  echo "node           : $RETH_VER"
  echo "host           : $CPU / ${CORES} cores / ${RAM_GB} GB RAM"
  echo "source RPC     : $SOURCE_RPC (tip $SRC_TIP)"
  echo
  echo "--- end-to-end block import (wall-clock) ---"
  awk -F, 'NR==1{for(i=1;i<=NF;i++)h[$i]=i; next} NR==2{
      printf "  run wall-clock   : %.2f s for %d blocks, %d gas, %d txs  ->  %.0f Mgas/s overall\n",
        $h["wall_clock_ms"]/1000, $h["samples"], $h["total_gas"], $h["total_txs"],
        ($h["total_gas"]/1e6)/($h["wall_clock_ms"]/1000)
      printf "  newPayload       : avg %.1f ms  p50 %.1f ms  p95 %.1f ms  p99 %.1f ms\n",
        $h["avg_new_payload_ms"], $h["p50_new_payload_ms"], $h["p95_new_payload_ms"], $h["p99_new_payload_ms"]
    }' "$OUT/summary.csv"
  if [ -f "$OUT/combined_latency.csv" ]; then
    awk -F, 'NR==1{for(i=1;i<=NF;i++)h[$i]=i; next}
      { g=$h["gas_used"]+0; if(g>0){ n++; np+=$h["new_payload_ms"]; gas+=g } }
      END{ if(n>0) printf "  non-empty blocks : %d  avg newPayload %.1f ms  avg gas %.0f  ->  %.0f Mgas/s (sum gas / sum newPayload)\n", n, np/n, gas/n, (gas/1e6)/(np/1000) }' "$OUT/combined_latency.csv"
  fi
  echo "  full percentiles + per-block CSV: summary.csv / combined_latency.csv"
  echo
  echo "--- per-stage wall-clock (Prometheus histogram deltas over the run) ---"
  echo "    stages run in PARALLEL (persistence is async, overlaps execution of later"
  echo "    blocks) — these totals overlap; do NOT sum them or read them as a split."
  awk -v es="$EXEC_S" -v en="$EXEC_N" -v em="$EXEC_M" \
      -v rs="$ROOT_S" -v rn="$ROOT_N" -v rm="$ROOT_M" \
      -v ps="$PERS_S" -v pn="$PERS_N" -v pm="$PERS_M" -v nb="$NB" '
    BEGIN{
      printf "  EVM execution    : %8.3f s total | %7.2f ms/block   [%s, n=%d]\n", es, 1000*es/nb, em, en
      printf "  state-root/trie  : %8.3f s total | %7.2f ms/block   [%s, n=%d]\n", rs, 1000*rs/nb, rm, rn
      if(pn>0) printf "  disk persistence : %8.3f s total | %7.2f ms/block   [%s, n=%d ops]\n", ps, 1000*ps/nb, pm, pn
      else     printf "  disk persistence : %8.3f s total | %7.2f ms/block   [%s]\n", ps, 1000*ps/nb, pm
    }'
  echo
  echo "--- diagnostic: histograms whose count rose during replay (top 15) ---"
  awk 'FNR==NR{ if($1 ~ /_count(\{|$)/){n=$1; sub(/\{.*/,"",n); b[n]=$2} next }
       { if($1 ~ /_count(\{|$)/){n=$1; sub(/\{.*/,"",n); d=$2-b[n]; if(d>0.5) print d"\t"n} }' \
       "$BEFORE" "$AFTER" | sort -rn | head -15 | awk -F'\t' '{printf "  +%-8d %s\n", $1, $2}'
} | tee "$REPORT"

echo
echo "artifacts:"
echo "  $REPORT"
echo "  $OUT/summary.csv          (end-to-end aggregate + percentiles)"
echo "  $OUT/combined_latency.csv (per-block new_payload/fcu/total)"
echo "  $BEFORE / $AFTER (raw Prometheus scrapes for the stage deltas)"
