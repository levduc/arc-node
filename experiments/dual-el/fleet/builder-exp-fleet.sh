#!/usr/bin/env bash
# Phase-1 builder separation — FLEET experiment (docs/deferred-exec-100k.md):
# 1 dedicated builder (papaduck NVMe) + 4 validators. Expected: capacity 16-19k -> >=22k.
#
#   ./builder-exp-fleet.sh start     # ship images -> boot fleet w/ builder env -> builder ->
#                                    # restart CLs -> verify routing
#   ./builder-exp-fleet.sh measure   # drain campaign 150M/300M vs canonical control
#   ./builder-exp-fleet.sh stop      # full teardown (fleet stop + builder rm)
set -uo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$DIR/../../.." && pwd)"; cd "$REPO"
SCEN=soak4
LOCAL_TS=100.124.148.61
BUILDER_HOST=papaduck
BUILDER_TS=100.70.62.92
BUILDER_BASE=/mnt/blockchain.ssd/arc-fleet/$SCEN
CL_ENV_JSON='{"ARC_PAYMENT_BUILDER_ENGINE":"http://'"$BUILDER_TS"':19951","ARC_PAYMENT_BUILDER_ETH_RPC":"http://'"$BUILDER_TS"':19945"}'
tss(){ local h=$1; shift; timeout -k 10 "${TSS_TMO:-120}" tailscale ssh "$h" "$@" </dev/null; }

preflight(){
  for h in ginnythui papaduck papaduck-alien2; do
    out=$(tss "$h" "echo ok" 2>&1 | tail -1)
    [ "$out" = "ok" ] || { echo "!! tailscale ssh to $h broken: $out"; exit 1; }
  done
  echo "preflight: all remotes reachable"
}

start(){
  preflight
  echo "==> [1/5] ship images (new CL image is required on every machine)"
  bash "$DIR/ship-images.sh"

  echo "==> [2/5] boot fleet with builder env on every CL (builder not up yet -> CLs run stock)"
  CL_EXTRA_ENV="$CL_ENV_JSON" bash "$DIR/demo-fleet-metamask.sh" start

  echo "==> [3/5] launch the builder EL on $BUILDER_HOST (NVMe)"
  B64=$(base64 -w0 "$DIR/../builder-el.sh")
  tss "$BUILDER_HOST" "echo $B64 | base64 -d > /tmp/builder-el.sh"
  TSS_TMO=300 tss "$BUILDER_HOST" "cd $BUILDER_BASE && \
    BASE=$BUILDER_BASE BUILDER_ADDR=$BUILDER_TS BUILDER_P2P=30419 \
    PEERS='$LOCAL_TS:19545:30411 100.85.150.119:19645:30412 $BUILDER_TS:19745:30413 100.86.97.40:19845:30414' \
    bash /tmp/builder-el.sh"

  echo "==> [4/5] restart all CLs so they connect to the (now-live) builder"
  docker restart validator1_cl >/dev/null
  for h in ginnythui papaduck papaduck-alien2; do
    n=$([ "$h" = ginnythui ] && echo 2 || { [ "$h" = papaduck ] && echo 3 || echo 4; })
    tss "$h" "docker restart validator${n}_cl" >/dev/null
  done
  sleep 25

  echo "==> [5/5] verify"
  verify
}

verify(){
  python3 - <<'PY'
import json,urllib.request,time,sys
def rpc(m,p=[]):
    r=urllib.request.Request("http://127.0.0.1:19545",data=json.dumps({"jsonrpc":"2.0","id":1,"method":m,"params":p}).encode(),headers={"content-type":"application/json"})
    return json.load(urllib.request.urlopen(r,timeout=10))["result"]
h1=int(rpc("eth_blockNumber"),16); time.sleep(8); h2=int(rpc("eth_blockNumber"),16)
print(f"producing: pay head {h2} (+{h2-h1}/8s)")
sys.exit(0 if h2>h1 else 1)
PY
  echo "-- builder engagement: build counter on builder metrics (should rise with heights)"
  c1=$(curl -s -m 8 "http://$BUILDER_TS:19901/metrics" 2>/dev/null | grep -m1 "^reth_arc_payload_total_duration_seconds_count" | awk '{print $2}')
  sleep 10
  c2=$(curl -s -m 8 "http://$BUILDER_TS:19901/metrics" 2>/dev/null | grep -m1 "^reth_arc_payload_total_duration_seconds_count" | awk '{print $2}')
  echo "   builder builds: $c1 -> $c2 (delta over 10s; ~2/s when engaged at 500ms cadence)"
  echo "-- val1 CL routing evidence:"
  docker logs validator1_cl 2>&1 | grep -cE "built via remote builder" | xargs echo "   remote-built:"
  docker logs validator1_cl 2>&1 | grep -cE "falling back to local build" | xargs echo "   fallbacks:"
}

measure(){
  rm -f /tmp/builder-exp.jsonl
  SIZES="${SIZES:-150 300}" FILL_TARGET=190000 OUT=/tmp/builder-exp.jsonl \
    bash "$DIR/../drain-campaign.sh"
  echo "== builder-exp results (control: 150M 444ms/16.1k · 300M 773ms/18.5k) =="
  cat /tmp/builder-exp.jsonl
}

stop(){
  tss "$BUILDER_HOST" "docker rm -f builder_el_pay" >/dev/null 2>&1 || true
  bash "$DIR/demo-fleet-metamask.sh" stop
}

case "${1:-start}" in
  start) start;; verify) verify;; measure) measure;; stop) stop;;
  *) echo "usage: $0 start|verify|measure|stop"; exit 1;;
esac
