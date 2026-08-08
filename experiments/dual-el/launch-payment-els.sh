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

for i in 1 2 3 4; do sleep ${STAGGER:-0};
  name="validator${i}_el_pay"
  http=$((19545 + (i-1)*100)); ws=$((19546 + (i-1)*100)); auth=$((19551 + (i-1)*100)); met=$((19001 + (i-1)*100))
  dd="$BASE/validator${i}/reth-pay"; mkdir -p "$dd"
  # Extra node args: PAY_EL_EXTRA_ARGS applies to every payment EL; PAY_EL<i>_EXTRA_ARGS to one
  # validator only (node-local flags like --engine.state-root-fallback are consensus-safe per
  # node, so a single-validator A/B on identical blocks is valid).
  eval "per_val_extra=\${PAY_EL${i}_EXTRA_ARGS:-}"
  extra_args="${PAY_EL_EXTRA_ARGS:-} ${per_val_extra}"
  # Same split for docker -e: PAY_EL_ENV applies to every payment EL, PAY_EL<i>_ENV to one.
  # Per-validator env is what makes a same-box A/B possible (one EL differs, same blocks).
  eval "per_val_env=\${PAY_EL${i}_ENV:-}"
  docker rm -f "$name" >/dev/null 2>&1
  docker run -d --name "$name" --network "$NET" \
    ${PAY_EL_ENV:+$PAY_EL_ENV} ${per_val_env:+$per_val_env} \
    --entrypoint /app/assets/entrypoint_el.sh \
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
      --arc.builder.deadline=500 --arc.builder.wait-for-payload=true --txpool.nolocals \
      --txpool.pending-max-count=200000 --txpool.queued-max-count=200000 ${extra_args} >/dev/null \
    && { docker network connect "$HOSTNET" "$name" 2>/dev/null; \
         echo "launched $name on $NET+$HOSTNET (RPC http://127.0.0.1:${http})"; } \
    || echo "FAILED $name"
done

# --- Peer the payment ELs so they GOSSIP transactions to each other (static peers; discovery stays
# off). Without this, each validator only sees txs sent directly to it, so an un-fed proposer builds
# empty payment blocks. This makes the payment lane behave like the EVM lane. ---
echo "peering payment ELs for tx gossip…"
sleep 5
declare -A ENODE
for i in 1 2 3 4; do sleep ${STAGGER:-0};
  name="validator${i}_el_pay"; http=$((19545 + (i-1)*100))
  ip=$(docker inspect -f "{{(index .NetworkSettings.Networks \"$NET\").IPAddress}}" "$name" 2>/dev/null)
  pub=$(curl -s -m 4 -X POST "http://127.0.0.1:$http" -H 'content-type: application/json' \
        --data '{"jsonrpc":"2.0","id":1,"method":"admin_nodeInfo","params":[]}' 2>/dev/null \
        | python3 -c "import sys,json;print(json.load(sys.stdin)['result']['enode'].split('@')[0])" 2>/dev/null)
  ENODE[$i]="${pub}@${ip}:30303"
done
for i in 1 2 3 4; do sleep ${STAGGER:-0};
  http=$((19545 + (i-1)*100))
  for j in 1 2 3 4; do
    [ "$i" -eq "$j" ] && continue
    curl -s -m 4 -X POST "http://127.0.0.1:$http" -H 'content-type: application/json' \
      --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"admin_addPeer\",\"params\":[\"${ENODE[$j]}\"]}" >/dev/null 2>&1
  done
done
sleep 3
for i in 1 2 3 4; do sleep ${STAGGER:-0};
  http=$((19545 + (i-1)*100))
  peers=$(curl -s -m 4 -X POST "http://127.0.0.1:$http" -H 'content-type: application/json' \
    --data '{"jsonrpc":"2.0","id":1,"method":"admin_peers","params":[]}' 2>/dev/null \
    | python3 -c "import sys,json;print(len(json.load(sys.stdin)['result']))" 2>/dev/null)
  echo "  validator${i}_el_pay peers: ${peers:-0}/3"
done
