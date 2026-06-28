#!/usr/bin/env bash
# Step 2 of the dual-EL payment lane: launch a SECOND reth EL ("payment lane") per validator
# alongside the running quake testnet. Reuses arc_execution:latest, own datadir/genesis/ports,
# discovery off (isolated from the EVM chain). Idle (block 0) until the CL drives it (step 4) --
# this is the "node boots with 2 ELs" milestone. Real reth, no simulation.
set -u
NET=arc_testnet_host-access
IMG=arc_execution:latest
ASSETS="$(pwd)/.quake/localdev4/assets"
BASE="$(pwd)/.quake/localdev4"
[ -d "$ASSETS" ] || { echo "no testnet assets at $ASSETS (start the testnet first)"; exit 1; }

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
      --authrpc.addr=0.0.0.0 --authrpc.port=8551 \
      --metrics=0.0.0.0:9001 --disable-discovery --ipcdisable >/dev/null \
    && echo "launched $name  (RPC http://127.0.0.1:${http})" || echo "FAILED $name"
done
