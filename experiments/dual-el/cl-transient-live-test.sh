#!/bin/bash
# Track A acceptance: a lean-node outage must NOT kill or park a CL.
# Single machine, 5 validators, lean lane on from height 1, mixed load.
#   S1: kill val3's lean node for 60s under load  -> CL3 stays up, chain moves,
#       transient warn logged, metric transient_dependency_skips{source=sync} > 0
#   S2: restart CL3 while its lean node is STILL down -> logs "retrying", never parks;
#       bring the node back -> CL3 rejoins (heights carry its signature again)
set -uo pipefail
cd /home/papaduck/arc-node-paymentlane
R=/tmp/a-live/report.md; : > "$R"
log(){ echo "[$(date +%H:%M:%S)] LIVE $*" | tee -a "$R"; }
fail(){ log "❌ FAIL: $*"; exit 1; }
BIN=target/release/lean-lane-node; SPAM=target/release/spammer
FUND=/home/papaduck/lean-fund.txt
rpc(){ curl -s -m6 -X POST "http://127.0.0.1:$1" -H 'content-type: application/json' --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$2\",\"params\":$3}"; }
lean_head(){ rpc $1 arc_getHead '[]' | python3 -c 'import sys,json;print(json.load(sys.stdin)["result"]["number"])' 2>/dev/null || echo -1; }
lean_up(){ # $1 = index 1..5
  # NOTE: separate statements — `local i=$1 P=$((8560+i))` expands the
  # arithmetic BEFORE assigning i (bash), so it used the stale global i=5 and
  # relaunched "val3" on val5's port. Cost two live-test runs (2026-09-07).
  local i=$1
  local P=$((8560+i))
  local PEERS=""
  for k in 1 2 3 4 5; do [ $k -ne $i ] && PEERS="${PEERS}http://127.0.0.1:$((8560+k)),"; done
  setsid $BIN run --datadir /home/papaduck/lean-lane/live/val$i --port $P --bind 0.0.0.0 \
    --chain-id 1338 --shim --peers "${PEERS%,}" --fund-file $FUND \
    --fund-balance 10000000000000000000 >> /tmp/a-live/lean-$i.log 2>&1 &
  disown
}
lean_kill(){ local pid=$(ss -ltnp 2>/dev/null | grep ":$((8560+$1)) " | grep -oP 'pid=\K[0-9]+' | head -1)
  [ -n "$pid" ] || { log "lean_kill $1: no listener on :$((8560+$1))"; return 0; }
  kill -9 $pid; local t=0
  while kill -0 $pid 2>/dev/null && [ $t -lt 20 ]; do sleep 0.5; t=$((t+1)); done
  kill -0 $pid 2>/dev/null && fail "lean_kill $1: pid $pid survived kill -9"; log "lean_kill $1: pid $pid dead"; }
wait_lean(){ # $1 = index, $2 = max seconds — the node must come back on its own (bind retry)
  local t=0; until [ "$(lean_head $((8560+$1)))" != "-1" ]; do sleep 2; t=$((t+2)); [ $t -ge $2 ] && fail "val$1 lean node not back after ${2}s (tail: $(tail -1 /tmp/a-live/lean-$1.log | cut -c1-100))"; done; }

# ---------------------------------------------------------------- boot
log "boot: quake localdev (5 validators)"
docker compose -f .quake/localdev/compose.yaml down > /dev/null 2>&1 || true
for i in 1 2 3 4 5; do lean_kill $i; done
rm -rf /home/papaduck/lean-lane/live
cargo run --release --bin quake -- -f crates/quake/scenarios/localdev.toml start --monitoring false --force > /tmp/a-live/quake.log 2>&1 || true
[ "$(docker ps --format '{{.Names}}' | grep -cE 'validator[0-9]_(cl|el)$')" -ge 10 ] || fail "quake did not start 5 validators"
docker stop full1_cl full1_el > /dev/null 2>&1; docker rm full1_cl full1_el > /dev/null 2>&1
python3 - <<'PY'
import re
p='.quake/localdev/compose.yaml'; s=open(p).read()
s=re.sub(r'^.*--payment-(eth-rpc-endpoint|execution-endpoint|execution-ws-endpoint|execution-jwt)=.*\n','',s,flags=re.M)
peers=",".join(f"http://host.docker.internal:{8560+k}" for k in range(1,6))
for i in range(1,6):
    svc=f"  validator{i}_cl:\n    container_name: validator{i}_cl"
    if 'host.docker.internal' not in s.split(svc,1)[1][:400]:
        s=s.replace(svc, svc+"\n    extra_hosts:\n      - host.docker.internal:host-gateway",1)
    a=f"ARC_LOG_FILE: /var/log/arc/validator{i}_cl.log"
    if f"ARC_PAYMENT_LEAN_RPC: 'http://host.docker.internal:{8560+i}'" not in s:
        s=s.replace(a, a+f"\n      ARC_PAYMENT_LEAN_LANE: '1'\n      ARC_PAYMENT_LEAN_RPC: 'http://host.docker.internal:{8560+i}'\n      ARC_PAYMENT_LEAN_BUDGET_GAS: '100000000'\n      ARC_PAYMENT_LEAN_PEER_RPCS: '{peers}'",1)
open(p,'w').write(s)
PY
# lean nodes FIRST, then recreate CLs (boot-park rule)
docker stop validator1_cl validator2_cl validator3_cl validator4_cl validator5_cl > /dev/null 2>&1
for i in 1 2 3 4 5; do lean_up $i; done
sleep 6
for i in 1 2 3 4 5; do [ "$(lean_head $((8560+i)))" = "0" ] || fail "lean node $i not at genesis"; done
# wipe CL state so lean is on from height 1 (never mid-chain), keep keys
for i in 1 2 3 4 5; do
  docker run --rm -v "$PWD/.quake/localdev":/b --user root alpine sh -c "rm -rf /b/validator$i/malachite/store.db /b/validator$i/malachite/wal" 2>/dev/null
  docker stop validator${i}_el > /dev/null 2>&1
  docker run --rm -v "$PWD/.quake/localdev":/b --user root alpine sh -c "rm -rf /b/validator$i/reth" 2>/dev/null
done
docker compose -f .quake/localdev/compose.yaml up -d --force-recreate validator1_el validator2_el validator3_el validator4_el validator5_el validator1_cl validator2_cl validator3_cl validator4_cl validator5_cl > /dev/null 2>&1
sleep 45
for i in 1 2 3 4 5; do
  [ "$(docker logs validator${i}_cl 2>&1 | grep -c 'Manual intervention')" = "0" ] || fail "val$i parked at boot"
done
H0=$(lean_head 8562); sleep 20; H1=$(lean_head 8562)
[ "$H1" -gt "$H0" ] || fail "lean chain not advancing at boot ($H0 -> $H1)"
log "boot ok: dual-lane chain at lean height $H1, no parks"

# ---------------------------------------------------------------- load
$SPAM ws --targets ws://127.0.0.1:8561 -r 1500 -g 4 -a 200 --account-offset 0 -t 400 --chain-id 1338 \
  --mix fanout=100 --fanout-outputs "1:20,5:20,10:20,50:20,100:20" > /tmp/a-live/spam1.log 2>&1 &
disown
$SPAM ws --targets ws://127.0.0.1:8563 -r 1500 -g 4 -a 200 --account-offset 200 -t 400 --chain-id 1338 \
  --mix fanout=100 --fanout-outputs "1:20,5:20,10:20,50:20,100:20" > /tmp/a-live/spam3.log 2>&1 &
disown
sleep 30
log "load running (mixed N, 2 feeders)"

# ---------------------------------------------------------------- S1
RC0=$(docker inspect validator3_cl --format '{{.RestartCount}}')
HK0=$(lean_head 8562)
log "S1: killing val3 lean node for 60s (CL3 restarts so far: $RC0, chain at $HK0)"
lean_kill 3
sleep 60
lean_up 3
wait_lean 3 150
log "S1: val3 lean node back (head $(lean_head 8563)); observing 120s"
sleep 120
RC1=$(docker inspect validator3_cl --format '{{.RestartCount}}')
PARK=$(docker logs validator3_cl --since 5m 2>&1 | grep -c 'Manual intervention')
DEAD=$(docker logs validator3_cl --since 5m 2>&1 | grep -c 'Error handling consensus message')
HK1=$(lean_head 8562)
TRANS=$(docker logs validator3_cl --since 5m 2>&1 | grep -c 'transient dependency error')
ANCH=$(docker logs validator3_cl --since 5m 2>&1 | grep -c 'node unreachable at anchor')
MET=$(curl -s -m5 http://127.0.0.1:29002/metrics | grep -E '^transient_dependency_skips' | head -3)
log "S1 result: CL3 restarts $RC0->$RC1 parked=$PARK fatal=$DEAD chain $HK0->$HK1 transient_warns=$TRANS anchor_waits=$ANCH"
log "S1 metric: ${MET:-<none>}"
[ "$RC1" = "$RC0" ] || fail "S1: CL3 restarted ($RC0 -> $RC1) — process still dies"
[ "$PARK" = "0" ] || fail "S1: CL3 parked"
[ "$DEAD" = "0" ] || fail "S1: fatal consensus error logged"
[ "$HK1" -gt $((HK0+60)) ] || fail "S1: chain did not keep moving ($HK0 -> $HK1)"
V3=$(lean_head 8563); [ "$V3" -gt $((HK1-10)) ] || fail "S1: val3 lean node did not catch up ($V3 vs $HK1)"
log "✅ S1 PASS — outage tolerated, CL3 alive, val3 caught up to $V3"

# ---------------------------------------------------------------- S2
log "S2: kill val3 lean node, then restart CL3 while it is down"
lean_kill 3
sleep 3
docker restart validator3_cl > /dev/null 2>&1
sleep 40
RETRY=$(docker logs validator3_cl --since 60s 2>&1 | grep -c 'unreachable at boot.*retrying')
PARK=$(docker logs validator3_cl --since 60s 2>&1 | grep -c 'Manual intervention')
log "S2: while down: retry_lines=$RETRY parked=$PARK"
[ "$RETRY" -ge 1 ] || fail "S2: CL3 did not log boot retries"
[ "$PARK" = "0" ] || fail "S2: CL3 PARKED at boot (old behaviour)"
lean_up 3
sleep 90
CONN=$(docker logs validator3_cl --since 100s 2>&1 | grep -c 'Connected to LEAN payment lane node')
SIG=$(docker logs validator2_cl --since 30s 2>&1 | grep -oE 'signatures=[0-9]' | sort | uniq -c | tr '\n' ' ')
V3=$(lean_head 8563); HK=$(lean_head 8562)
log "S2 result: connected=$CONN sigs(last 30s)=[$SIG] val3 lean $V3 vs chain $HK"
[ "$CONN" -ge 1 ] || fail "S2: CL3 never connected after node returned"
[ "$V3" -gt $((HK-10)) ] || fail "S2: val3 did not catch up"
log "✅ S2 PASS — boot patience works, CL3 rejoined"

# ---------------------------------------------------------------- teardown
pkill -9 -f 'release/spammer' 2>/dev/null || true
docker compose -f .quake/localdev/compose.yaml down > /dev/null 2>&1
for i in 1 2 3 4 5; do lean_kill $i; done
rm -rf /home/papaduck/lean-lane/live
log "✅ ALL PASS — teardown done"
