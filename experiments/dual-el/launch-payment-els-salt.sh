#!/usr/bin/env bash
# Launch the 4 payment-lane ELs with SALT (MegaETH) as the state commitment instead of reth's MPT.
#
# Identical to launch-payment-els.sh except:
#   - image  : arc_execution_salt:latest (built from deployments/Dockerfile.execution on this
#              branch, so the binary has the SALT commitment wired into reth's two seams).
#              A host-built binary CANNOT be dropped into the stock image: host glibc is 2.39,
#              the arc_execution image is Debian 12 / glibc 2.36.
#   - env    : ARC_PAYMENT_ROOT=salt selects the SALT commitment at runtime.
#
# The EVM lane is untouched and still uses the MPT, so the demo shows both side by side under one
# consensus certificate.
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"; cd "$REPO"
TESTNET="${TESTNET:-soak4}"
BASE="$REPO/.quake/$TESTNET/data"
ASSETS="$REPO/.quake/$TESTNET/assets"
NET="arc_testnet_default"
HOSTNET="arc_testnet_host-access"
IMG="${IMG:-arc_execution_salt:latest}"

docker image inspect "$IMG" >/dev/null 2>&1 || {
  echo "!! image $IMG not found. Build it first:"
  echo "   DOCKER_BUILDKIT=1 docker build -f deployments/Dockerfile.execution --target dev-runtime \\"
  echo "     --build-context certs=deployments/certs -t arc_execution_salt:latest ."
  exit 1
}
[ -f "$ASSETS/payment-jwt.hex" ] || { echo "missing $ASSETS/payment-jwt.hex"; exit 1; }

for i in 1 2 3 4; do sleep ${STAGGER:-0};
  name="validator${i}_el_pay"
  http=$((19545 + (i-1)*100)); ws=$((19546 + (i-1)*100)); auth=$((19551 + (i-1)*100)); met=$((19001 + (i-1)*100))
  dd="$BASE/validator${i}/reth-pay"; mkdir -p "$dd"
  docker rm -f "$name" >/dev/null 2>&1
  docker run -d --name "$name" --network "$NET" \
    --entrypoint /app/assets/entrypoint_el.sh \
    -e ARC_PAYMENT_ROOT=salt \
    -e ARC_JMT_TRACE="${ARC_JMT_TRACE:-}" \
    -v "$dd":/data/reth/execution-data \
    -v "$ASSETS":/app/assets \
    -p ${http}:8545 -p ${ws}:8546 -p ${auth}:8551 -p ${met}:9001 \
    "$IMG" \
    node --datadir=/data/reth/execution-data --chain=/app/assets/${PAYMENT_GENESIS:-genesis.json} \
      --http --http.addr=0.0.0.0 --http.port=8545 --http.corsdomain='*' --http.api=eth,net,web3,txpool,debug,admin \
      --ws --ws.addr=0.0.0.0 --ws.port=8546 --ws.origins='*' --ws.api=eth,net,web3,txpool \
      --authrpc.addr=0.0.0.0 --authrpc.port=8551 --authrpc.jwtsecret=/app/assets/payment-jwt.hex \
      --metrics=0.0.0.0:9001 --disable-discovery --ipcdisable \
      --port 30303 \
      --engine.state-root-fallback --engine.disable-parallel-sparse-trie \
      --arc.builder.deadline=2000 --arc.builder.wait-for-payload=true --txpool.nolocals \
      --txpool.pending-max-count=200000 --txpool.queued-max-count=200000 >/dev/null \
    && { docker network connect "$HOSTNET" "$name" 2>/dev/null; \
         echo "launched $name (SALT) on $NET+$HOSTNET (RPC http://127.0.0.1:${http})"; } \
    || echo "FAILED $name"
done

# Peer the payment ELs so they gossip transactions (static peers; discovery stays off). Without
# this an un-fed proposer builds empty payment blocks on its round-robin turn.
echo "peering payment ELs for tx gossip…"
sleep 5
declare -A ENODE
for i in 1 2 3 4; do
  e=$(curl -s -m5 -X POST "http://127.0.0.1:$((19545 + (i-1)*100))" -H 'content-type: application/json' \
      --data '{"jsonrpc":"2.0","id":1,"method":"admin_nodeInfo","params":[]}' 2>/dev/null \
      | python3 -c "import sys,json;print(json.load(sys.stdin)['result']['enode'])" 2>/dev/null)
  ENODE[$i]="$e"
done
for i in 1 2 3 4; do
  for j in 1 2 3 4; do
    [ "$i" = "$j" ] && continue
    ip=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{if .IPAddress}}{{.IPAddress}} {{end}}{{end}}' "validator${j}_el_pay" 2>/dev/null | awk '{print $1}')
    en=$(echo "${ENODE[$j]}" | sed -E "s#@[^:]+:#@${ip}:#")
    [ -z "$en" ] && continue
    curl -s -m5 -X POST "http://127.0.0.1:$((19545 + (i-1)*100))" -H 'content-type: application/json' \
      --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"admin_addPeer\",\"params\":[\"$en\"]}" >/dev/null 2>&1
  done
done
echo "payment lane up with SALT commitment (ARC_PAYMENT_ROOT=salt)"
