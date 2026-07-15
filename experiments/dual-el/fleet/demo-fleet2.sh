#!/usr/bin/env bash
# 2-MACHINE dual-EL testnet: validators 1-3 on this box, validator4 on papaduck (tailscale).
# Self-contained: verified single-machine start (demo-bloat.sh, 10M preseed, fail-loud)
# then the val4 migration (fleet/pilot.sh) incl. fleet-mode dashboard.
#   ./demo-fleet2.sh start | stop | status
# Fallbacks on demo day: fleet/demo-fleet.sh (4 machines) | ../demo-bloat.sh | ../demo-empty.sh
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"; cd "$REPO"
case "${1:-}" in
  start)
    ./experiments/dual-el/demo-bloat.sh start || { echo "!! single-machine baseline failed"; exit 1; }
    sleep 60
    b=$(curl -s -m5 -X POST http://127.0.0.1:19545 -H 'content-type: application/json' --data '{"jsonrpc":"2.0","id":1,"method":"eth_getBalance","params":["0x00000000000000000000000000000020004c4b40","latest"]}' | python3 -c "import sys,json;print(int(json.load(sys.stdin)['result'],16))" 2>/dev/null)
    [ "$b" = "1000000000000000000" ] || { echo "!! preseed verification failed — aborting migration"; exit 1; }
    bash experiments/dual-el/fleet/pilot.sh
    ;;
  stop)
    timeout 60 tailscale ssh papaduck "ids=\$(docker ps -aq --filter name=validator); [ -n \"\$ids\" ] && docker rm -f \$ids; docker run --rm -v /home/papaduck/arc-fleet:/f --user root alpine rm -rf /f/soak4 2>/dev/null" || true
    ./experiments/dual-el/demo-bloat.sh stop
    ;;
  status)
    for ep in "val1(local) 127.0.0.1 19545" "val2(local) 127.0.0.1 19645" "val3(local) 127.0.0.1 19745" "val4(papaduck) 100.70.62.92 19845"; do
      set -- $ep
      h=$(curl -s -m5 -X POST "http://$2:$3" -H 'content-type: application/json' --data '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' | python3 -c "import sys,json;print(int(json.load(sys.stdin)['result'],16))" 2>/dev/null)
      echo "$1 pay head: ${h:-UNREACHABLE}"
    done
    ;;
  *) echo "usage: $0 {start|stop|status}"; exit 1;;
esac
