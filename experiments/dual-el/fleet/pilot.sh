#!/usr/bin/env bash
# Fleet PILOT: move validator4 (CL + EVM EL + payment EL) to papaduck over tailscale;
# validators 1-3 keep running on this host. Run against a VERIFIED single-machine testnet
# (demo-bloat.sh start must be up and healthy first).
#
# Steps: stop local val4 -> patch local compose (peers -> tailscale for val4; publish EL p2p)
# -> recreate val1-3 CL/EL -> rsync val4 tree + assets -> remote compose up -> remote payment EL
# -> cross-machine payment mesh -> bounce spammers -> verify heads/agreement incl. remote.
set -uxo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$REPO"
REMOTE=papaduck
REMOTE_TS=100.70.62.92
LOCAL_TS=100.124.148.61
RBASE=/home/papaduck/arc-fleet/soak4
LBASE="$REPO/.quake/soak4"

echo "==> [1/8] stop local validator4 stack"
docker rm -f validator4_cl validator4_el validator4_el_pay 2>/dev/null || true

echo "==> [2/8] rewrite topology (compose surgery)"
python3 experiments/dual-el/fleet/gen-remote-val4.py

echo "==> [3/8] recreate val1-3 with new peer args + published EL p2p"
docker compose -f "$LBASE/compose.yaml" up -d --force-recreate \
  validator1_cl validator2_cl validator3_cl validator1_el validator2_el validator3_el

echo "==> [4/8] ship validator4 tree + assets to $REMOTE"
timeout 30 tailscale ssh "$REMOTE" "mkdir -p $RBASE"
tar -C "$LBASE" -czf - assets validator4 | timeout 600 tailscale ssh "$REMOTE" "tar -C $RBASE -xzf -"
timeout 60 tailscale ssh "$REMOTE" "ls $RBASE/validator4; du -sh $RBASE/validator4/reth-pay 2>/dev/null"
scp -q "$LBASE/compose-val4.yaml" "$REMOTE:$RBASE/" 2>/dev/null || \
  cat "$LBASE/compose-val4.yaml" | timeout 30 tailscale ssh "$REMOTE" "cat > $RBASE/compose-val4.yaml"

echo "==> [5/8] start remote validator4 CL+EL"
timeout 120 tailscale ssh "$REMOTE" "docker compose -f $RBASE/compose-val4.yaml up -d"

echo "==> [6/8] remote payment EL (uncapped boot; same flags as local launcher + p2p published)"
timeout 60 tailscale ssh "$REMOTE" "docker rm -f validator4_el_pay 2>/dev/null; docker run -d --name validator4_el_pay --network arc_testnet_default \
  --entrypoint /app/assets/entrypoint_el.sh \
  -v $RBASE/validator4/reth-pay:/data/reth/execution-data -v $RBASE/assets:/app/assets \
  -p 19845:8545 -p 19846:8546 -p 19851:8551 -p 19301:9001 -p 30414:30303 \
  arc_execution:latest \
  node --datadir=/data/reth/execution-data --chain=/app/assets/payment-genesis.json \
  --http --http.addr=0.0.0.0 --http.port=8545 --http.corsdomain='*' --http.api=eth,net,web3,txpool,debug,admin \
  --ws --ws.addr=0.0.0.0 --ws.port=8546 --ws.origins='*' --ws.api=eth,net,web3,txpool \
  --authrpc.addr=0.0.0.0 --authrpc.port=8551 --authrpc.jwtsecret=/app/assets/payment-jwt.hex \
  --metrics=0.0.0.0:9001 --disable-discovery --ipcdisable --port 30303 \
  --arc.builder.deadline=2000 --arc.builder.wait-for-payload=true --txpool.nolocals \
  --txpool.pending-max-count=200000 --txpool.queued-max-count=200000"

echo "==> [7/8] cross-machine payment gossip mesh (tailscale enodes)"
sleep 30
declare -A EN
for i in 1 2 3; do
  h=$((19545+(i-1)*100))
  pub=$(curl -s -m4 -X POST http://127.0.0.1:$h -H 'content-type: application/json' \
    --data '{"jsonrpc":"2.0","id":1,"method":"admin_nodeInfo","params":[]}' \
    | python3 -c "import sys,json;print(json.load(sys.stdin)['result']['enode'].split('@')[0])" 2>/dev/null)
  EN[$i]="${pub}@${LOCAL_TS}:$((30310+i))"
done
# local pay ELs must publish p2p for the remote to dial: relaunch is heavy, so pilot uses
# one-way dialing instead: the REMOTE pay EL addPeers the local three via their container net
# is impossible cross-host - so local ELs publish nothing yet; remote dials NOTHING and the
# LOCAL ELs dial the REMOTE (outbound over tailscale works without local publishing):
p4=$(timeout 30 tailscale ssh "$REMOTE" "curl -s -m4 -X POST http://127.0.0.1:19845 -H 'content-type: application/json' --data '{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"admin_nodeInfo\",\"params\":[]}'" \
  | python3 -c "import sys,json;print(json.load(sys.stdin)['result']['enode'].split('@')[0])" 2>/dev/null)
for i in 1 2 3; do
  h=$((19545+(i-1)*100))
  curl -s -m4 -X POST http://127.0.0.1:$h -H 'content-type: application/json' \
    --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"admin_addPeer\",\"params\":[\"${p4}@${REMOTE_TS}:30414\"]}" >/dev/null
done

echo "==> [8/8] bounce spammers (nonce resync) and verify"
pkill -x spammer 2>/dev/null; sleep 45
for ep in "local-evm2 http://127.0.0.1:8645" "remote-evm4 http://$REMOTE_TS:8845" \
          "local-pay2 http://127.0.0.1:19645" "remote-pay4 http://$REMOTE_TS:19845"; do
  set -- $ep
  hh=$(curl -s -m5 -X POST "$2" -H 'content-type: application/json' --data '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' \
    | python3 -c "import sys,json;print(int(json.load(sys.stdin)['result'],16))" 2>/dev/null)
  echo "HEAD $1 = ${hh:-UNREACHABLE}"
done
echo "==> restart dashboard in fleet mode (val4 -> $REMOTE_TS)"
python3 -c "import json;json.dump({'val4':'$REMOTE_TS'},open('/tmp/dualel-pilot-fleet.json','w'))"
old=$(ss -ltnp 2>/dev/null | grep ':8080 ' | grep -oP 'pid=\K[0-9]+' | head -1); [ -n "$old" ] && kill "$old" 2>/dev/null
DUALEL_FLEET=/tmp/dualel-pilot-fleet.json nohup python3 experiments/dual-el/dashboard.py >/tmp/dualel-bloat/dashboard.log 2>&1 &
echo "PILOT SETUP DONE $(date +%F_%T) — dashboard http://localhost:8080 (fleet mode: val4 remote)"
