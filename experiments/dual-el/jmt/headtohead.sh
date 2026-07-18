#!/usr/bin/env bash
# JMT-vs-MPT head-to-head, bare-metal (no Docker — host binary, dodges the glibc image issue).
# Two identical arc-node-execution ELs on the SAME localdev genesis, driven by identical mock-CL
# block loops + identical transfer load. Only difference: ARC_PAYMENT_ROOT=jmt on one.
# Measures per-block state-root latency from each EL's prometheus, so the delta IS JMT vs MPT.
#   ./headtohead.sh start [DURATION_SECS]   (default 7200 = 2h)
#   ./headtohead.sh report                  (avg root_ms + block count each lane)
#   ./headtohead.sh stop
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"; cd "$REPO"
BIN="$REPO/target/release/arc-node-execution"
SPAM="$REPO/target/release/spammer"
GEN="${GEN:-/tmp/jmt-h2h/genesis-prague.json}"
RUN=/tmp/jmt-h2h; mkdir -p "$RUN"
JWT="$RUN/jwt.hex"; [ -f "$JWT" ] || openssl rand -hex 32 > "$JWT"
# lane: name http ws auth metrics p2p  (MPT then JMT)
MPT=(mpt 7545 7546 7551 7001 30501)
JMT=(jmt 7645 7646 7651 7002 30502)
RATE=${RATE:-800}     # transfers/sec into each lane
BLOCK_MS=${BLOCK_MS:-250}

boot(){ # boot <name> <http> <ws> <auth> <met> <p2p> <jmt?>
  local n=$1 http=$2 ws=$3 auth=$4 met=$5 p2p=$6 isjmt=$7
  local dd="$RUN/data-$n"; rm -rf "$dd" "$RUN/store-$n"
  "$BIN" init --datadir "$dd" --chain "$GEN" >"$RUN/init-$n.log" 2>&1
  local env=""
  [ "$isjmt" = 1 ] && env="ARC_PAYMENT_ROOT=jmt ARC_JMT_STORE_PATH=$RUN/store-$n"
  env $env "$BIN" node --datadir "$dd" --chain "$GEN" \
    --http --http.addr 127.0.0.1 --http.port "$http" --http.api eth,net,web3,debug \
    --ws --ws.addr 127.0.0.1 --ws.port "$ws" --ws.api eth,net,web3,txpool \
    --authrpc.addr 127.0.0.1 --authrpc.port "$auth" --authrpc.jwtsecret "$JWT" \
    --metrics 127.0.0.1:"$met" --disable-discovery --ipcdisable --port "$p2p" \
    --engine.state-root-fallback --engine.disable-parallel-sparse-trie \
    --txpool.pending-max-count 400000 --txpool.queued-max-count 400000 \
    >"$RUN/node-$n.log" 2>&1 &
  echo $! > "$RUN/pid-$n"
}

mockcl(){ # mockcl <name> <http> <auth> — build/validate a block every BLOCK_MS from mempool
  local n=$1 http=$2 auth=$3
  nohup python3 "$REPO/experiments/dual-el/jmt/mockcl.py" "$http" "$auth" "$JWT" "$BLOCK_MS" \
    >"$RUN/mockcl-$n.log" 2>&1 & echo $! > "$RUN/pid-mockcl-$n"
}

case "${1:-}" in
  start)
    DUR=${2:-7200}
    echo "==> booting MPT + JMT ELs (bare-metal host binary)"
    boot "${MPT[@]}" 0
    boot "${JMT[@]}" 1
    for a in 7551 7651; do for i in $(seq 1 30); do curl -s -m2 http://127.0.0.1:$((a-6)) >/dev/null 2>&1 && break; sleep 2; done; done
    sleep 15
    echo "==> mock-CL block loops"
    mockcl mpt 7545 7551
    mockcl jmt 7645 7651
    sleep 8
    FRESHFLAG=""; [ "${FRESH:-1}" = 1 ] && FRESHFLAG="--fresh-recipients"  # grow state (default on)
    echo "==> identical transfer load ($RATE/s each) fresh='$FRESHFLAG'"
    rm -f "$RUN/spam.stop"
    ( while [ ! -f "$RUN/spam.stop" ]; do "$SPAM" ws --targets ws://127.0.0.1:7546 -r "$RATE" -t 600 -g 8 -a 1000 -l $FRESHFLAG --mix transfer=100 >"$RUN/spam-mpt.log" 2>&1; sleep 1; done ) & echo $! >"$RUN/pid-spam-mpt"
    ( while [ ! -f "$RUN/spam.stop" ]; do "$SPAM" ws --targets ws://127.0.0.1:7646 -r "$RATE" -t 600 -g 8 -a 1000 -l $FRESHFLAG --mix transfer=100 >"$RUN/spam-jmt.log" 2>&1; sleep 1; done ) & echo $! >"$RUN/pid-spam-jmt"
    # time-series sampler: windowed root latency per lane every 30s -> series.csv (proves flat vs growing)
    echo "epoch,lane,head,cum_root_ms,win_root_ms" > "$RUN/series.csv"
    ( ps=(0 0); pc=(0 0)
      while [ ! -f "$RUN/spam.stop" ]; do
        i=0
        for L in "MPT 7001 7545" "JMT 7002 7645"; do set -- $L
          h=$(curl -s -m3 -X POST http://127.0.0.1:$3 -H 'content-type: application/json' --data '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' 2>/dev/null | python3 -c "import sys,json;print(int(json.load(sys.stdin)['result'],16))" 2>/dev/null)
          read s c < <(curl -s -m3 http://127.0.0.1:$2/metrics 2>/dev/null | awk '/^reth_sync_block_validation_state_root_histogram_sum/{s=$2}/^reth_sync_block_validation_state_root_histogram_count/{c=$2}END{print s+0, c+0}')
          cum=$(python3 -c "print(f'{($s/$c*1000) if $c else 0:.4f}')")
          win=$(python3 -c "ds=$s-${ps[$i]}; dc=$c-${pc[$i]}; print(f'{(ds/dc*1000) if dc>0 else 0:.4f}')")
          echo "$(date +%s),$1,${h:-0},$cum,$win" >> "$RUN/series.csv"
          ps[$i]=$s; pc[$i]=$c; i=$((i+1))
        done
        sleep 30
      done ) & echo $! >"$RUN/pid-sampler"
    echo "==> running ${DUR}s. Monitor: $0 report | series: $RUN/series.csv"
    echo "$(date +%s) $DUR" > "$RUN/started"
    ;;
  report)
    for L in "MPT 7001 7545" "JMT 7002 7645"; do set -- $L
      h=$(curl -s -m3 -X POST http://127.0.0.1:$3 -H 'content-type: application/json' --data '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' | python3 -c "import sys,json;print(int(json.load(sys.stdin)['result'],16))" 2>/dev/null)
      curl -s -m3 http://127.0.0.1:$2/metrics 2>/dev/null | python3 -c "
import sys
s=c=0.0
for ln in sys.stdin:
    if ln.startswith('reth_sync_block_validation_state_root_histogram_sum'): s=float(ln.split()[1])
    if ln.startswith('reth_sync_block_validation_state_root_histogram_count'): c=float(ln.split()[1])
avg=(s/c*1000) if c else 0
print(f'$1 lane: head=${h:-?}  blocks_rooted={int(c)}  avg_state_root={avg:.3f} ms')"
    done
    ;;
  stop)
    touch "$RUN/spam.stop" 2>/dev/null; sleep 2
    for f in "$RUN"/pid-*; do kill "$(cat "$f")" 2>/dev/null; done
    pkill -x spammer 2>/dev/null
    echo "stopped"
    ;;
  *) echo "usage: $0 {start [secs]|report|stop}"; exit 1;;
esac
