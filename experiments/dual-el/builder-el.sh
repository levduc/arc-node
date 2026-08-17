#!/usr/bin/env bash
# Phase-1 builder separation (docs/deferred-exec-100k.md): launch the dedicated payment-lane
# BUILDER EL — a 5th payment EL whose pool fills via gossip and whose getPayload serves every
# proposer. Mirrors launch-payment-els.sh exactly (same image, genesis, pool-caps trio) so the
# builder's build semantics are byte-identical to a validator's.
#
#   TESTNET=soak4 ./builder-el.sh                      # local (single-machine smoke)
#   PEERS="127.0.0.1:19545 127.0.0.1:19645 ..."        # pay-EL http endpoints to peer with
#
# Ports: http 19945, ws 19946, authrpc 19951, metrics 19901.
set -u
NET=${NET:-arc_testnet_default}
HOSTNET=${HOSTNET:-arc_testnet_host-access}
IMG=arc_execution:latest
TESTNET="${TESTNET:-soak4}"
BASE="${BASE:-$(pwd)/.quake/$TESTNET}"
ASSETS="${ASSETS:-$BASE/assets}"
DD="${DD:-$BASE/builder/reth-pay}"
NAME=builder_el_pay
PEERS="${PEERS:-127.0.0.1:19545 127.0.0.1:19645 127.0.0.1:19745 127.0.0.1:19845}"

[ -f "$ASSETS/payment-jwt.hex" ] || { echo "missing $ASSETS/payment-jwt.hex"; exit 1; }
mkdir -p "$DD"

docker rm -f "$NAME" >/dev/null 2>&1
docker run -d --name "$NAME" --network "$HOSTNET" \
  ${BUILDER_ENV:-} \
  --entrypoint /app/assets/entrypoint_el.sh \
  -v "$DD":/data/reth/execution-data \
  -v "$ASSETS":/app/assets \
  -p 19945:8545 -p 19946:8546 -p 19951:8551 -p 19901:9001 -p 30419:30303 \
  "$IMG" \
  node --datadir=/data/reth/execution-data --chain=/app/assets/${PAYMENT_GENESIS:-payment-genesis.json} \
    --http --http.addr=0.0.0.0 --http.port=8545 --http.corsdomain='*' --http.api=eth,net,web3,txpool,debug,admin \
    --ws --ws.addr=0.0.0.0 --ws.port=8546 --ws.origins='*' --ws.api=eth,net,web3,txpool \
    --authrpc.addr=0.0.0.0 --authrpc.port=8551 --authrpc.jwtsecret=/app/assets/payment-jwt.hex \
    --metrics=0.0.0.0:9001 --disable-discovery --ipcdisable \
    --port 30303 \
    --arc.builder.deadline=500 --arc.builder.wait-for-payload=true --txpool.nolocals \
    --txpool.pending-max-count=200000 --txpool.queued-max-count=200000 \
    --txpool.pending-max-size=512 --txpool.queued-max-size=512 \
    --txpool.basefee-max-count=200000 --txpool.basefee-max-size=512 \
    --txpool.max-account-slots=${PAY_ACCOUNT_SLOTS:-256} \
    --rpc-cache.max-blocks=200 --rpc-cache.max-receipts=200 \
    ${BUILDER_EXTRA_ARGS:-} >/dev/null \
  && { docker network connect "$NET" "$NAME" 2>/dev/null; echo "launched $NAME (http :19945, authrpc :19951)"; } \
  || { echo "FAILED $NAME"; exit 1; }

# Peer the builder with the validators' pay ELs (both directions) so its pool fills via the
# existing tx gossip — spam tooling needs no changes.
echo "peering builder with pay ELs…"
sleep 4
BENODE=$(curl -s -m 5 -X POST http://127.0.0.1:19945 -H 'content-type: application/json' \
  --data '{"jsonrpc":"2.0","id":1,"method":"admin_nodeInfo","params":[]}' \
  | python3 -c "import sys,json;print(json.load(sys.stdin)['result']['enode'].split('@')[0])")
BIP=$(docker inspect -f "{{(index .NetworkSettings.Networks \"$NET\").IPAddress}}" "$NAME" 2>/dev/null)
[ -n "$BIP" ] || BIP=$(docker inspect -f "{{(index .NetworkSettings.Networks \"$HOSTNET\").IPAddress}}" "$NAME")
for hp in $PEERS; do
  # tell the peer about the builder
  curl -s -m 5 -X POST "http://$hp" -H 'content-type: application/json' \
    --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"admin_addPeer\",\"params\":[\"${BENODE}@${BUILDER_ADDR:-$BIP}:${BUILDER_P2P:-30303}\"]}" >/dev/null
  # tell the builder about the peer
  PENODE=$(curl -s -m 5 -X POST "http://$hp" -H 'content-type: application/json' \
    --data '{"jsonrpc":"2.0","id":1,"method":"admin_nodeInfo","params":[]}' \
    | python3 -c "import sys,json;print(json.load(sys.stdin)['result']['enode'])" 2>/dev/null)
  [ -n "$PENODE" ] && curl -s -m 5 -X POST http://127.0.0.1:19945 -H 'content-type: application/json' \
    --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"admin_addPeer\",\"params\":[\"$PENODE\"]}" >/dev/null
done
sleep 3
NPEERS=$(curl -s -m 5 -X POST http://127.0.0.1:19945 -H 'content-type: application/json' \
  --data '{"jsonrpc":"2.0","id":1,"method":"admin_peers","params":[]}' \
  | python3 -c "import sys,json;print(len(json.load(sys.stdin)['result']))" 2>/dev/null)
echo "builder peers: ${NPEERS:-0}"
