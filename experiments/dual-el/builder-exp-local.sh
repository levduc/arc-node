#!/usr/bin/env bash
# Phase-1 builder separation — SINGLE-MACHINE validation (docs/deferred-exec-100k.md).
# Boots the 4-validator demo, adds a local builder EL, points every CL's payment build
# path at it (env via compose override), then verifies:
#   1. chain producing (RPC heads, never logs)
#   2. proposals actually routed via the builder (CL log counter as secondary evidence)
#   3. 100+ consecutive blocks with all 4 pay ELs identical on blockHash+stateRoot
#
#   ./builder-exp-local.sh start | verify | stop
set -uo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$DIR/../.." && pwd)"; cd "$REPO"
SCEN=demo2lane
COMPOSE="$REPO/.quake/$SCEN/compose.yaml"

start(){
  echo "==> [1/4] boot the single-machine demo"
  bash "$DIR/demo-metamask.sh" start

  echo "==> [2/4] launch the builder EL (5th pay EL, gossip-peered)"
  TESTNET=$SCEN bash "$DIR/builder-el.sh"

  echo "==> [3/4] point all CLs at the builder (compose override + recreate CLs)"
  # CLs reach the builder by container name on the shared docker network.
  cat > "$REPO/.quake/$SCEN/builder-override.yaml" <<EOF
services:
$(for i in 1 2 3 4; do cat <<S
  validator${i}_cl:
    environment:
      ARC_PAYMENT_BUILDER_ENGINE: "http://builder_el_pay:8551"
      ARC_PAYMENT_BUILDER_ETH_RPC: "http://builder_el_pay:8545"
S
done)
EOF
  docker compose -f "$COMPOSE" -f "$REPO/.quake/$SCEN/builder-override.yaml" up -d \
    validator1_cl validator2_cl validator3_cl validator4_cl

  echo "==> [4/4] wait for production to resume, then verify"
  sleep 20
  verify
}

verify(){
  python3 - <<'PY'
import json,urllib.request,time,sys
def rpc(port,m,p=[]):
    r=urllib.request.Request(f"http://127.0.0.1:{port}",data=json.dumps({"jsonrpc":"2.0","id":1,"method":m,"params":p}).encode(),headers={"content-type":"application/json"})
    return json.load(urllib.request.urlopen(r,timeout=10))["result"]
h1=int(rpc(19545,"eth_blockNumber"),16); time.sleep(6); h2=int(rpc(19545,"eth_blockNumber"),16)
print(f"producing: pay head {h2} (+{h2-h1}/6s)"); assert h2>h1, "chain NOT producing"
# agreement across the four pay ELs over the last 100 settled blocks
top=h2-2; lo=max(1,top-99); bad=0
for x in range(lo,top+1):
    ref=None
    for port in (19545,19645,19745,19845):
        b=rpc(port,"eth_getBlockByNumber",[hex(x),False])
        k=(b["hash"],b["stateRoot"])
        if ref is None: ref=k
        elif k!=ref: bad+=1; print(f"  !! divergence at {x} on :{port}")
print(f"agreement: {top-lo+1} blocks x 4 validators, {bad} divergences")
sys.exit(1 if bad else 0)
PY
  echo "-- builder routing evidence (CL logs, secondary):"
  for i in 1 2 3 4; do
    n=$(docker logs validator${i}_cl 2>&1 | grep -c "built via remote builder" || true)
    f=$(docker logs validator${i}_cl 2>&1 | grep -c "falling back to local build" || true)
    echo "   val$i: remote-built=$n fallbacks=$f"
  done
}

stop(){
  docker rm -f builder_el_pay >/dev/null 2>&1
  bash "$DIR/demo-metamask.sh" stop
}

case "${1:-start}" in start) start;; verify) verify;; stop) stop;; *) echo "usage: $0 start|verify|stop"; exit 1;; esac
