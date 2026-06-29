#!/usr/bin/env bash
# Dual-EL payment lane: launch a SECOND reth EL ("payment lane") per validator on the running
# quake testnet. Reuses arc_execution:latest, own datadir + the (100M-gas) genesis, discovery off
# (isolated from the EVM chain), authrpc secured with the shared payment JWT.
#
# Network: arc_testnet_default so each CL reaches its payment EL by name (validatorN_el_pay),
# plus host-access so the RPC is reachable from the host for inspection.
#
# The CL (built with the dual-EL code) connects here via --payment-execution-endpoint and drives
# this EL every block: builds a payment payload, all validators re-execute it, both roots are
# committed. Run AFTER `quake start` (the CL retries the payment connection until this is up).
set -u
NET=arc_testnet_default
HOSTNET=arc_testnet_host-access
IMG=arc_execution:latest
TESTNET="${TESTNET:-localdev4}"
ASSETS="$(pwd)/.quake/$TESTNET/assets"
BASE="$(pwd)/.quake/$TESTNET"
[ -d "$ASSETS" ] || { echo "no testnet assets at $ASSETS (start the testnet first)"; exit 1; }
[ -f "$ASSETS/payment-jwt.hex" ] || { echo "missing $ASSETS/payment-jwt.hex"; exit 1; }

for i in 1 2 3 4; do
  name="validator${i}_el_pay"
  http=$((19545 + (i-1)*100)); ws=$((19546 + (i-1)*100)); auth=$((19551 + (i-1)*100)); met=$((19001 + (i-1)*100))
  dd="$BASE/validator${i}/reth-pay"; mkdir -p "$dd"
  docker rm -f "$name" >/dev/null 2>&1
  docker run -d --name "$name" --network "$NET" \
    --entrypoint /app/assets/entrypoint_el.sh \
    -v "$dd":/data/reth/execution-data \
    -v "$ASSETS":/app/assets \
    -p ${http}:8545 -p ${ws}:8546 -p ${auth}:8551 -p ${met}:9001 \
    "$IMG" \
    node --datadir=/data/reth/execution-data --chain=/app/assets/genesis.json \
      --http --http.addr=0.0.0.0 --http.port=8545 --http.corsdomain='*' --http.api=eth,net,web3,txpool,debug \
      --ws --ws.addr=0.0.0.0 --ws.port=8546 --ws.origins='*' --ws.api=eth,net,web3,txpool \
      --authrpc.addr=0.0.0.0 --authrpc.port=8551 --authrpc.jwtsecret=/app/assets/payment-jwt.hex \
      --metrics=0.0.0.0:9001 --disable-discovery --ipcdisable \
      --arc.builder.deadline=2000 --arc.builder.wait-for-payload=true --txpool.nolocals \
      --txpool.pending-max-count=200000 --txpool.queued-max-count=200000 >/dev/null \
    && { docker network connect "$HOSTNET" "$name" 2>/dev/null; \
         echo "launched $name on $NET+$HOSTNET (RPC http://127.0.0.1:${http})"; } \
    || echo "FAILED $name"
done
