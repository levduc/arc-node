#!/bin/bash
# V7: fleet arms — no-regression pure100@150M, then bypay serial/parallel @225M.
cd /home/papaduck/arc-node-paymentlane
R=/tmp/v7/report.md; J=/tmp/v7/results.jsonl
log(){ echo "[$(date +%H:%M:%S)] V7 $*" >> "$R"; }
LOCAL_TS=100.124.148.61
declare -A RHOST=( [2]=ginnythui [3]=papaduck [4]=papaduck-alien2 )
declare -A RTS=( [2]=100.85.150.119 [3]=100.70.62.92 [4]=100.86.97.40 )
SPAM=target/release/spammer
WINDOW=600
MEAS=http://${RTS[2]}:8560     # val2's lean node — uninvolved wired validator

# arms: label|spec|avg_n|par|budget|gen_s|rate
ARMS=(
  "pure100-225M-r3|100|100|1|225000000|80|700"
)

flip_budget(){ # halt ALL CLs -> edit -> resume ALL (never a subset)
  local B=$1
  local cur=$(docker exec validator1_cl env 2>/dev/null | grep -oP 'BUDGET_GAS=\K[0-9]+')
  [ "$cur" = "$B" ] && { log "budget already $B"; return 0; }
  log "budget flip -> $B (halt-flip-resume)"
  docker stop validator1_cl > /dev/null 2>&1 &
  for n in 2 3 4; do timeout 90 tailscale ssh papaduck@${RHOST[$n]} "docker stop validator${n}_cl" > /dev/null 2>&1 & done
  wait
  python3 -c "import re;p='.quake/soak4/compose.yaml';s=open(p).read();open(p,'w').write(re.sub(r\"BUDGET_GAS: '[0-9]+'\",\"BUDGET_GAS: '$B'\",s))"
  for n in 2 3 4; do
    timeout 90 tailscale ssh papaduck@${RHOST[$n]} "python3 -c \"import re;p='/home/papaduck/arc-fleet/soak4/compose-val$n.yaml';s=open(p).read();open(p,'w').write(re.sub(r\\\"BUDGET_GAS: '[0-9]+'\\\",\\\"BUDGET_GAS: '$B'\\\",s))\"" 2>/dev/null
  done
  docker compose -f .quake/soak4/compose.yaml up -d validator1_cl > /dev/null 2>&1 &
  for n in 2 3 4; do timeout 120 tailscale ssh papaduck@${RHOST[$n]} "docker compose -f /home/papaduck/arc-fleet/soak4/compose-val$n.yaml up -d validator${n}_cl" > /dev/null 2>&1 & done
  wait
  sleep 20
  local got=$(docker exec validator1_cl env 2>/dev/null | grep -oP 'BUDGET_GAS=\K[0-9]+')
  log "budget now: ${got:-?}"
  [ "$got" = "$B" ]
}

health(){  # no parks, no sync wedges, chain advancing
  local bad=0
  for n in 1 2 3 4; do
    local c
    if [ $n = 1 ]; then c=$(docker logs validator1_cl --since 90s 2>&1 | grep -cE 'Manual intervention|got 0 values' || true)
    else c=$(timeout 45 tailscale ssh papaduck@${RHOST[$n]} "docker logs validator${n}_cl --since 90s 2>&1 | grep -cE 'Manual intervention|got 0 values'" 2>/dev/null | tail -1 | tr -dc '0-9'); fi
    if [ -z "$c" ]; then log "HEALTH: val$n UNREACHABLE (ssh expired?)"; bad=1
    elif [ "$c" != "0" ]; then log "HEALTH: val$n bad ($c)"; bad=1; fi
  done
  # chain-advance: PATIENT — after lean-node restarts the chain takes 30-60s
  # to resume (CLs reconnect to freshly-replayed nodes; replay grows with
  # chain length). One 15s sample false-aborted r3 on 2026-08-27.
  local adv=0 t0=$(date +%s)
  local h0=$(curl -s -m8 -X POST $MEAS -H 'content-type: application/json' --data '{"jsonrpc":"2.0","id":1,"method":"arc_getHead","params":[]}' | python3 -c 'import sys,json;print(json.load(sys.stdin)["result"]["number"])' 2>/dev/null)
  while [ $(( $(date +%s) - t0 )) -lt 240 ]; do
    sleep 20
    local h1=$(curl -s -m8 -X POST $MEAS -H 'content-type: application/json' --data '{"jsonrpc":"2.0","id":1,"method":"arc_getHead","params":[]}' | python3 -c 'import sys,json;print(json.load(sys.stdin)["result"]["number"])' 2>/dev/null)
    [ "${h1:-0}" -gt "${h0:-0}" ] && { adv=1; break; }
  done
  [ $adv = 1 ] || { log "HEALTH: chain not advancing (240s)"; bad=1; }
  return $bad
}

kill_load(){
  pkill -9 -f 'lean-feeder.p[y]' 2>/dev/null; pkill -9 -f 'release/spammer' 2>/dev/null
  for n in 2 3 4; do timeout 45 tailscale ssh papaduck@${RHOST[$n]} "pkill -9 -f 'lean-feeder.p[y]' 2>/dev/null; pkill -9 -f 'arc-spamme[r]' 2>/dev/null; true" 2>/dev/null; done
  true
}

run_arm(){ # label spec avg_n par budget gen rate
  local LABEL=$1 SPEC=$2 AVGN=$3 PAR=$4 B=$5 GEN=$6 RATE=$7
  log "=== ARM $LABEL (spec=$SPEC par=$PAR budget=$B)"
  # SSH preflight FIRST: an expired tailscale session silently no-ops remote
  # commands; proceeding would half-execute the budget flip (local CL restarts
  # alone = wedge risk). Cost two aborted arms on 2026-08-27.
  for n in 2 3 4; do
    local ok=$(timeout 30 tailscale ssh papaduck@${RHOST[$n]} "echo OK" 2>/dev/null | tail -1)
    [ "$ok" = "OK" ] || { log "ARM $LABEL ABORT: SSH EXPIRED (no effect from ${RHOST[$n]}) — reauth needed"; return 1; }
  done
  kill_load
  flip_budget $B || { log "ARM $LABEL ABORT (budget)"; return 1; }
  if [ "${SKIP_LEANUP:-0}" = 1 ]; then
    log "lean-up SKIPPED (nodes already in target mode; avoids restart-induced height splits)"
  else
    bash /tmp/v7/lean-up.sh $PAR $B >> "$R" 2>&1 || { log "ARM $LABEL ABORT (lean-up)"; return 1; }
  fi
  sleep 10
  health || { log "ARM $LABEL ABORT (health)"; return 1; }
  # chain-static for lean nonces: pools are fresh (nodes restarted); verify no feeders
  # corpus per machine (parallel, no ship)
  rm -f /tmp/v7-c1.txt
  ( SPAM_DUMP_FILE=/tmp/v7-c1.txt timeout $((GEN+80)) nice -n 10 $SPAM ws --targets ws://127.0.0.1:8560 -r 8000 -g 4 -a 200 --account-offset 0 -t $GEN --chain-id 1338 --mix fanout=100 --fanout-outputs "$SPEC" -l > /dev/null 2>&1
    cat /tmp/v7-c1.txt.[0-9]* > /tmp/v7-c1.txt 2>/dev/null; rm -f /tmp/v7-c1.txt.[0-9]* ) &
  local P1=$!
  local i=1
  for n in 2 3 4; do
    ( timeout $((GEN+200)) tailscale ssh papaduck@${RHOST[$n]} "rm -f /tmp/v7c.txt /tmp/v7c.txt.*; SPAM_DUMP_FILE=/tmp/v7c.txt timeout $((GEN+80)) nice -n 10 /home/papaduck/arc-spammer ws --targets ws://127.0.0.1:8560 -r 8000 -g 4 -a 200 --account-offset $((i*200)) -t $GEN --chain-id 1338 --mix fanout=100 --fanout-outputs '$SPEC' -l > /dev/null 2>&1; cat /tmp/v7c.txt.* > /tmp/v7c.txt 2>/dev/null; rm -f /tmp/v7c.txt.*" 2>/dev/null ) &
    i=$((i+1))
  done
  wait $P1 2>/dev/null; wait 2>/dev/null
  local L=$(wc -l < /tmp/v7-c1.txt 2>/dev/null || echo 0)
  local rok=0
  for n in 2 3 4; do
    local RL=$(timeout 45 tailscale ssh papaduck@${RHOST[$n]} "wc -l < /tmp/v7c.txt 2>/dev/null" 2>/dev/null | tail -1 | tr -dc '0-9')
    [ -n "$RL" ] && [ "$RL" -gt 1000 ] && rok=$((rok+1))
    log "corpus val$((n)): ${RL:-MISSING}"
  done
  log "corpus local: $L"
  [ "$L" -gt 1000 ] && [ $rok -ge 3 ] || { log "ARM $LABEL ABORT (gen)"; return 1; }
  # probe
  local TX=$(head -1 /tmp/v7-c1.txt)
  curl -s -m8 -X POST http://127.0.0.1:8560 -H 'content-type: application/json' --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_sendRawTransaction\",\"params\":[\"$TX\"]}" > /dev/null 2>&1
  local PB=$(curl -s -m8 -X POST http://127.0.0.1:8560 -H 'content-type: application/json' --data '{"jsonrpc":"2.0","id":1,"method":"txpool_status","params":[]}' | python3 -c 'import sys,json;print(json.load(sys.stdin)["result"]["pending"])' 2>/dev/null || echo 0)
  [ "${PB:-0}" -ge 1 ] || { log "ARM $LABEL ABORT (probe)"; return 1; }
  # feeders everywhere
  local PT=$(python3 -c "print(max(30000, int($B/(21000+5000*$AVGN))*4))")
  setsid nice -n 10 python3 experiments/dual-el/lean-feeder.py http://127.0.0.1:8560 /tmp/v7-c1.txt $PT $RATE $((WINDOW+180)) > /tmp/v7-feed1.log 2>&1 &
  disown
  for n in 2 3 4; do
    timeout 60 tailscale ssh papaduck@${RHOST[$n]} "(setsid nohup nice -n 10 python3 /home/papaduck/lean-feeder.py http://127.0.0.1:8560 /tmp/v7c.txt $PT $RATE $((WINDOW+180)) >> /home/papaduck/v7-feed.log 2>&1 < /dev/null &); true" 2>/dev/null
  done
  log "feeders up @$RATE/s/node; warm 60s"
  sleep 60
  log "measuring ${WINDOW}s"
  local T0=$(date +%s)
  python3 - "$LABEL" "$SPEC" "$PAR" "$B" "$MEAS" <<'PY' >> "$J"
import json, sys, time, urllib.request, base64
label, spec, par, budget, U = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4]), sys.argv[5]
def rpc(m,p):
    r=urllib.request.Request(U,data=json.dumps({"jsonrpc":"2.0","id":1,"method":m,"params":p}).encode(),headers={"content-type":"application/json"})
    return json.load(urllib.request.urlopen(r,timeout=25))["result"]
h0=rpc("arc_getHead",{})["number"]; t0=time.time(); time.sleep(600)
h1=rpc("arc_getHead",{})["number"]; dt=time.time()-t0
txs=ops=b=full=0; gas_sum=0
for n in range(h0+1,h1+1):
    bb=base64.b64decode(rpc("arc_getBlockBytes",{"number":n})["blockBytes"])
    ntx=int.from_bytes(bb[48:52],'little'); off=52; o=0; g=0
    for _ in range(ntx):
        l=int.from_bytes(bb[off:off+4],'little'); off+=4
        no=int.from_bytes(bb[off+5:off+7],'little'); o+=no; g+=21000+5000*no; off+=l
    txs+=ntx; ops+=o; b+=1; gas_sum+=g
    full += 1 if g >= 0.95*budget else 0
row=dict(arm=label,spec=spec,parallel=int(par),budget=budget,blocks=b,secs=round(dt),
         cadence=round(b/dt,2),tps=round(txs/dt),ops=round(ops/dt),
         sigs_blk=txs//max(b,1),avg_n=round(ops/max(txs,1),1),
         fullness_pct=round(100*(gas_sum/max(b,1))/budget))
print(json.dumps(row))
PY
  # burns + anchors from val2 over the window
  local SINCE=$(( $(date +%s) - T0 + 30 ))
  timeout 90 tailscale ssh papaduck@${RHOST[2]} "docker logs validator2_cl --since ${SINCE}s 2>&1 | grep -E 'decided on value|Successfully committed|round=[1-9] proposer'" 2>/dev/null > /tmp/v7-post.txt
  python3 - "$LABEL" <<'PY' >> "$R"
import re, sys, datetime, statistics, collections
ts=[]; burns=collections.Counter()
for line in open('/tmp/v7-post.txt'):
    m=re.match(r'^([0-9T:.\-]+Z)', line)
    if 'round=' in line and 'proposer' in line:
        pm=re.search(r'proposer=(0x[a-f0-9]{4})', line)
        if pm: burns[pm.group(1)]+=1
        continue
    if not m: continue
    t=datetime.datetime.fromisoformat(m.group(1).replace('Z','+00:00'))
    ts.append(('d' if 'decided on value' in line else 'c', t))
g=sorted((ts[i+1][1]-ts[i][1]).total_seconds() for i in range(len(ts)-1) if ts[i][0]=='d' and ts[i+1][0]=='c')
lab=sys.argv[1]
if g:
    n=len(g)
    print(f"[anchor] {lab}: n={n} p50={statistics.median(g)*1000:.0f}ms p90={g[int(n*0.9)]*1000:.0f}ms")
print(f"[burns] {lab}: {sum(burns.values())} total {dict(burns)}")
PY
  kill_load
  log "=== ARM $LABEL done"; tail -1 "$J" >> "$R"
}

log "V7 START"
for arm in "${ARMS[@]}"; do
  IFS='|' read -r LABEL SPEC AVGN PAR B GEN RATE <<< "$arm"
  run_arm "$LABEL" "$SPEC" "$AVGN" "$PAR" "$B" "$GEN" "$RATE" || log "arm $LABEL failed; continuing"
done
log "V7 COMPLETE"
