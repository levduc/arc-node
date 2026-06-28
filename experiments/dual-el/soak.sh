#!/usr/bin/env bash
# 1-hour soak of the dual-EL testnet. Every 60s checks:
#  - LIVENESS: every lane's height strictly increased since last check (else consensus halt)
#  - AGREEMENT: at a settled height, the block hash is identical across all validators per lane
#  - HEALTH: all 12 containers (4x CL+EVM-EL+payment-EL) are running
# Every ~10 min: a 40s dual-lane spam burst. Flags any failure but keeps running; prints a tally.
# NOTE: validator1's EVM host port (8545) is taken by a stray anvil, so validator1 EVM is read
# over the internal docker network; the other endpoints are read on the host.
export PATH="$HOME/.cargo/bin:$HOME/.foundry/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
cd /home/papaduck/arc-node
LOG=/tmp/dualel-soak.log
: > "$LOG"
DUR=3600
START=$(date +%s)
fails=0; checks=0; spambursts=0
declare -A prev

bn() { # block number (decimal) for a host port
  cast block-number --rpc-url http://127.0.0.1:$1 2>/dev/null
}
bn1evm() { # validator1 EVM via internal network
  docker run --rm --network arc_testnet_default curlimages/curl:latest -s -X POST http://validator1_el:8545 \
    -H 'content-type: application/json' --data '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' 2>/dev/null \
    | python3 -c "import sys,json;print(int(json.load(sys.stdin)['result'],16))" 2>/dev/null
}
hashat() { # block hash at height $2 on host port $1
  cast block $2 --rpc-url http://127.0.0.1:$1 --json 2>/dev/null \
    | python3 -c "import sys,json;print(json.load(sys.stdin)['hash'])" 2>/dev/null
}

log() { echo "[$(date +%H:%M:%S) +$(( $(date +%s)-START ))s] $*" | tee -a "$LOG"; }

log "SOAK START (1h). 4 nodes, each CL+EVM-EL+payment-EL."
while [ $(( $(date +%s) - START )) -lt $DUR ]; do
  checks=$((checks+1))
  iterfail=0

  # --- heights (EVM 2/3/4 host + v1 internal; payment 1..4 host) ---
  e2=$(bn 8645); e3=$(bn 8745); e4=$(bn 8845); e1=$(bn1evm)
  p1=$(bn 19545); p2=$(bn 19645); p3=$(bn 19745); p4=$(bn 19845)

  # --- liveness: each must increase vs previous check ---
  for k in e1 e2 e3 e4 p1 p2 p3 p4; do
    cur=${!k}
    [ -z "$cur" ] && { log "WARN $k unreachable"; continue; }
    if [ -n "${prev[$k]}" ] && [ "$cur" -le "${prev[$k]}" ]; then
      log "FAIL liveness: $k did not advance (${prev[$k]} -> $cur)"; iterfail=1
    fi
    prev[$k]=$cur
  done

  # --- containers health ---
  up=$(docker ps --format '{{.Names}}' | grep -cE 'validator[0-9]+_(cl|el|el_pay)$')
  [ "$up" -ne 12 ] && { log "FAIL health: only $up/12 containers up"; iterfail=1; }

  # --- differential agreement: settled height = min(payment heads)-10 ---
  mn=$p1; for x in $p2 $p3 $p4; do [ -n "$x" ] && [ "$x" -lt "$mn" ] && mn=$x; done
  H=$((mn-10))
  if [ "$H" -gt 1 ]; then
    ph1=$(hashat 19545 $H); ph2=$(hashat 19645 $H); ph3=$(hashat 19745 $H); ph4=$(hashat 19845 $H)
    if [ -n "$ph1" ] && { [ "$ph1" != "$ph2" ] || [ "$ph1" != "$ph3" ] || [ "$ph1" != "$ph4" ]; }; then
      log "FAIL agreement: payment blockHash@$H differs: $ph1 $ph2 $ph3 $ph4"; iterfail=1
    fi
    # EVM agreement across 2/3/4 (validator1 EVM not host-published)
    me=$e2; for x in $e3 $e4; do [ -n "$x" ] && [ "$x" -lt "$me" ] && me=$x; done
    HE=$((me-10))
    eh2=$(hashat 8645 $HE); eh3=$(hashat 8745 $HE); eh4=$(hashat 8845 $HE)
    if [ -n "$eh2" ] && { [ "$eh2" != "$eh3" ] || [ "$eh2" != "$eh4" ]; }; then
      log "FAIL agreement: EVM blockHash@$HE differs: $eh2 $eh3 $eh4"; iterfail=1
    fi
  fi

  [ "$iterfail" -ne 0 ] && fails=$((fails+1))
  log "ok=$([ $iterfail -eq 0 ] && echo Y || echo N) EVM[v1=$e1 v2=$e2 v3=$e3 v4=$e4] PAY[v1=$p1 v2=$p2 v3=$p3 v4=$p4] containers=$up dagree@pay$H/evm$HE fails=$fails/$checks"

  # --- periodic dual-lane spam (~ every 10 checks = 10 min) ---
  if [ $((checks % 10)) -eq 2 ]; then
    spambursts=$((spambursts+1))
    log "spam burst #$spambursts (40s, both lanes)"
    target/release/spammer ws --targets ws://127.0.0.1:8646,ws://127.0.0.1:8746,ws://127.0.0.1:8846 -r 250 -t 40 -a 250 -g 4 --mix transfer=100 >/tmp/soak_spam_evm.log 2>&1 &
    target/release/spammer ws --targets ws://127.0.0.1:19646,ws://127.0.0.1:19746,ws://127.0.0.1:19846 -r 250 -t 40 -a 250 -g 4 --mix transfer=100 >/tmp/soak_spam_pay.log 2>&1 &
  fi

  sleep 60
done
log "SOAK DONE. checks=$checks failed_checks=$fails spam_bursts=$spambursts"
[ "$fails" -eq 0 ] && log "RESULT: PASS (no liveness/agreement/health failures over 1h)" || log "RESULT: FAIL ($fails/$checks checks had issues)"
