#!/usr/bin/env bash
# Congestion demo: spam both lanes so the EVM lane visibly congests while the payment lane stays cheap.
#
#   ./congest-demo.sh start     # start both loads (EVM oversubscribed, payment light transfers)
#   ./congest-demo.sh watch     # live table: fullness / fee-vs-floor / mempool backlog per lane
#   ./congest-demo.sh status    # one-shot of the same table
#   ./congest-demo.sh stop       # stop the spammers
#
# What each load does and why the lanes diverge:
#   EVM lane      ~200 tx/s of ~1.6M-gas "guzzler" storage writes. That's ~320M gas/s of demand
#                 against a 30M-gas block (~120M gas/s of capacity) -> the block pins ~full, the
#                 EIP-1559 base fee ratchets up 12.5%/full block (compounds ~2x every ~6 blocks),
#                 and unincluded txs pile up in the mempool.
#   Payment lane  ~300 tx/s of plain transfers BETWEEN EXISTING accounts. 300 * 21k = ~6M gas/s
#                 against a 200M-gas block -> <1% full, base fee sits at the floor, backlog drains
#                 every block. Same chain, same validators, same consensus round -- just sized for
#                 payments.
#
# This is real EIP-1559 behaviour, not a mock. Point at `watch` (or the dashboard /product page)
# while it runs: the EVM fee/backlog run away, the payment numbers stay flat.
#
# Rates/targets are overridable by env: EVM_RATE, PAY_RATE, EVM_WS, PAY_WS, GUZZLER (intensity @N).
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SPAMMER="$REPO/target/release/spammer"
RUN="/tmp/congest-demo"; mkdir -p "$RUN"

EVM_WS="${EVM_WS:-ws://127.0.0.1:8546}"
PAY_WS="${PAY_WS:-ws://127.0.0.1:19546}"
EVM_RATE="${EVM_RATE:-200}"       # oversubscribe the 30M EVM block
PAY_RATE="${PAY_RATE:-300}"       # comfortable payment throughput
GUZZLER="${GUZZLER:-80}"          # storage-write intensity: @80 ~ 1.6M gas/tx (fits under 30M, ~19 fill a block)

# ws://host:PORT -> http://host:(PORT-1)   (Arc convention: EL http = ws - 1)
http_of(){ local u="${1#ws://}"; local h="${u%:*}" p="${u##*:}"; echo "http://$h:$((p-1))"; }
EVM_HTTP="$(http_of "$EVM_WS")"; PAY_HTTP="$(http_of "$PAY_WS")"

rpc(){ curl -s -m6 -X POST "$1" -H 'content-type: application/json' \
        --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$2\",\"params\":${3:-[]}}" 2>/dev/null; }
chainid(){ rpc "$1" eth_chainId | python3 -c "import sys,json;print(int(json.load(sys.stdin)['result'],16))" 2>/dev/null; }

start(){
  [ -x "$SPAMMER" ] || { echo "!! spammer not built: $SPAMMER  (run: cargo build --release -p spammer)"; exit 1; }
  local ecid pcid
  ecid="$(chainid "$EVM_HTTP")"; pcid="$(chainid "$PAY_HTTP")"
  [ -n "$ecid" ] || { echo "!! EVM lane not reachable at $EVM_HTTP"; exit 1; }
  [ -n "$pcid" ] || { echo "!! payment lane not reachable at $PAY_HTTP"; exit 1; }
  stop_quiet

  echo "==> EVM lane   ${EVM_WS}  (chainId $ecid): congesting with ${EVM_RATE} tx/s of ~1.6M-gas guzzlers"
  nohup "$SPAMMER" ws --targets "$EVM_WS" --chain-id "$ecid" \
    -r "$EVM_RATE" -t 100000 -g 4 -a 48 -l --fresh-recipients \
    --mix guzzler=90,transfer=10 --guzzler-fn-weights "storage-write=100@${GUZZLER}" \
    >"$RUN/evm.log" 2>&1 & echo $! >"$RUN/evm.pid"; disown 2>/dev/null || true

  echo "==> PAY lane   ${PAY_WS}  (chainId $pcid): ${PAY_RATE} tx/s transfers between existing accounts"
  nohup "$SPAMMER" ws --targets "$PAY_WS" --chain-id "$pcid" \
    -r "$PAY_RATE" -t 100000 -g 8 -a 1000 -l --mix transfer=100 \
    >"$RUN/pay.log" 2>&1 & echo $! >"$RUN/pay.pid"; disown 2>/dev/null || true

  sleep 3
  local n; n=$(pgrep -xc spammer 2>/dev/null || echo 0)
  echo "   spammers running: $n"
  echo
  echo "   watch it diverge:   $0 watch      (or the dashboard: http://localhost:8080/product)"
  echo "   stop:               $0 stop"
}

# one row of the divergence table for a lane
row(){ # $1=http $2=label
  local blk pool
  blk="$(rpc "$1" eth_getBlockByNumber '["latest",false]')"
  pool="$(rpc "$1" txpool_status)"
  echo "$blk|$pool" | LBL="$2" python3 -c "
import sys,os,json
b,p=sys.stdin.read().split('|',1)
try:
 d=json.loads(b)['result']; gu=int(d['gasUsed'],16); gl=int(d['gasLimit'],16); bf=int(d.get('baseFeePerGas','0x0'),16)
 full=100*gu/gl if gl else 0
except Exception:
 print(f\"  {os.environ['LBL']:<8}  (no data)\"); sys.exit()
try:
 pp=json.loads(p)['result']; q=int(pp['pending'],16)+int(pp.get('queued','0x0'),16)
except Exception:
 q='?'
bar=int(round(full/5)); bar=min(bar,20)
print(f\"  {os.environ['LBL']:<8} [{'#'*bar}{'.'*(20-bar)}] {full:5.1f}%  baseFee={bf:>12,} wei  waiting={q}\")
"
}

status(){
  local ecid pcid ebf pbf
  ecid="$(chainid "$EVM_HTTP")"; pcid="$(chainid "$PAY_HTTP")"
  echo "  lane      [ block fullness    ]         EIP-1559 base fee        mempool backlog"
  row "$EVM_HTTP" "EVM $ecid"
  row "$PAY_HTTP" "PAY $pcid"
  # fee multiple (EVM vs the floor the payment lane sits at)
  ebf="$(rpc "$EVM_HTTP" eth_getBlockByNumber '["latest",false]' | python3 -c "import sys,json;print(int(json.load(sys.stdin)['result'].get('baseFeePerGas','0x0'),16))" 2>/dev/null)"
  pbf="$(rpc "$PAY_HTTP" eth_getBlockByNumber '["latest",false]' | python3 -c "import sys,json;print(int(json.load(sys.stdin)['result'].get('baseFeePerGas','0x0'),16))" 2>/dev/null)"
  if [ -n "${ebf:-}" ] && [ -n "${pbf:-}" ] && [ "${pbf:-0}" -gt 0 ]; then
    echo "  -> EVM fee is $((ebf/pbf))x the payment-lane floor"
  fi
}

watch_(){
  echo "live congestion (Ctrl-C to stop the display; the load keeps running).  refresh 3s"
  while true; do
    printf '\033[H\033[2J'   # clear
    date '+%H:%M:%S'
    status
    sleep 3
  done
}

stop_quiet(){
  for f in evm pay; do [ -f "$RUN/$f.pid" ] && kill "$(cat "$RUN/$f.pid")" 2>/dev/null; done
  pkill -f "targets $EVM_WS" 2>/dev/null || true
  pkill -f "targets $PAY_WS" 2>/dev/null || true
  rm -f "$RUN"/*.pid
}
stop(){ stop_quiet; echo "  stopped. spammers running: $(pgrep -xc spammer 2>/dev/null || echo 0)"; }

case "${1:-}" in
  start)  start ;;
  stop)   stop ;;
  status) status ;;
  watch)  watch_ ;;
  *) echo "usage: $0 {start|watch|status|stop}"; echo "  env: EVM_RATE PAY_RATE EVM_WS PAY_WS GUZZLER"; exit 1 ;;
esac
