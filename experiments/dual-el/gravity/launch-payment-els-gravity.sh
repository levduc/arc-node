#!/usr/bin/env bash
# Dual-EL payment lane on GRAVITY-RETH (github.com/Galxe/gravity-reth, reth-1.8.3 fork with
# grevm + parallel merklization, all optimizations ON by default).
#
# Same contract as launch-payment-els.sh (ports, network, names, JWT, genesis, gossip mesh),
# but the EL image is gravity_reth:latest and the flags are STOCK reth (no --arc.builder.*).
# The CL drives it over the standard Engine API, so validatorN_el_pay just has to speak
# fcU/getPayload/newPayload. Run AFTER `quake start`.
#
# Env: TESTNET (default soak4), PAYMENT_GENESIS (default payment-genesis.json), STAGGER secs.
set -u
NET=arc_testnet_default
HOSTNET=arc_testnet_host-access
IMG=gravity_reth:latest
TESTNET="${TESTNET:-soak4}"
ASSETS="$(pwd)/.quake/$TESTNET/assets"
BASE="$(pwd)/.quake/$TESTNET"
[ -d "$ASSETS" ] || { echo "no testnet assets at $ASSETS (start the testnet first)"; exit 1; }
[ -f "$ASSETS/payment-jwt.hex" ] || { echo "missing $ASSETS/payment-jwt.hex"; exit 1; }

for i in 1 2 3 4; do sleep ${STAGGER:-0};
  name="validator${i}_el_pay"
  http=$((19545 + (i-1)*100)); ws=$((19546 + (i-1)*100)); auth=$((19551 + (i-1)*100)); met=$((19001 + (i-1)*100))
  dd="$BASE/validator${i}/reth-pay"; mkdir -p "$dd"
  docker rm -f "$name" >/dev/null 2>&1
  docker run -d --name "$name" --network "$NET" \
    -v "$dd":/data/reth/execution-data \
    -v "$ASSETS":/app/assets \
    -p ${http}:8545 -p ${ws}:8546 -p ${auth}:8551 -p ${met}:9001 \
    "$IMG" \
    node --datadir=/data/reth/execution-data --chain=/app/assets/${PAYMENT_GENESIS:-genesis.json} \
      --http --http.addr=0.0.0.0 --http.port=8545 --http.corsdomain='*' --http.api=eth,net,web3,txpool,debug,admin \
      --ws --ws.addr=0.0.0.0 --ws.port=8546 --ws.origins='*' --ws.api=eth,net,web3,txpool \
      --authrpc.addr=0.0.0.0 --authrpc.port=8551 --authrpc.jwtsecret=/app/assets/payment-jwt.hex \
      --gravity.disable-pipe-execution --gravity.cache.capacity 500000 \
      --metrics=0.0.0.0:9001 --disable-discovery --ipcdisable \
      --port 30303 \
      --txpool.pending-max-count=200000 --txpool.queued-max-count=200000 >/dev/null \
    && { docker network connect "$HOSTNET" "$name" 2>/dev/null; \
         echo "launched $name (gravity-reth) on $NET+$HOSTNET (RPC http://127.0.0.1:${http})"; } \
    || echo "FAILED $name"
done

echo "peering payment ELs for tx gossip…"
sleep 5
declare -A ENODE
for i in 1 2 3 4; do
  name="validator${i}_el_pay"; http=$((19545 + (i-1)*100))
  ip=$(docker inspect -f "{{(index .NetworkSettings.Networks \"$NET\").IPAddress}}" "$name" 2>/dev/null)
  pub=$(curl -s -m 4 -X POST "http://127.0.0.1:$http" -H 'content-type: application/json' \
        --data '{"jsonrpc":"2.0","id":1,"method":"admin_nodeInfo","params":[]}' 2>/dev/null \
        | python3 -c "import sys,json;print(json.load(sys.stdin)['result']['enode'].split('@')[0])" 2>/dev/null)
  ENODE[$i]="${pub}@${ip}:30303"
done
for i in 1 2 3 4; do
  http=$((19545 + (i-1)*100))
  for j in 1 2 3 4; do
    [ "$i" -eq "$j" ] && continue
    curl -s -m 4 -X POST "http://127.0.0.1:$http" -H 'content-type: application/json' \
      --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"admin_addPeer\",\"params\":[\"${ENODE[$j]}\"]}" >/dev/null 2>&1
  done
done
sleep 3
for i in 1 2 3 4; do
  http=$((19545 + (i-1)*100))
  peers=$(curl -s -m 4 -X POST "http://127.0.0.1:$http" -H 'content-type: application/json' \
    --data '{"jsonrpc":"2.0","id":1,"method":"admin_peers","params":[]}' 2>/dev/null \
    | python3 -c "import sys,json;print(len(json.load(sys.stdin)['result']))" 2>/dev/null)
  echo "  validator${i}_el_pay peers: ${peers:-0}/3"
done
