#!/usr/bin/env bash
# Soak the dual-EL testnet with CONTINUOUS dual-lane spam and check for consensus errors.
#   DUR  = seconds to run        (default 3600; use 86400 for 24h)
#   RATE = tx/s per lane         (default 300)
# Every 60s it checks:
#   - LIVENESS : every lane on every node strictly advanced since the last check (else consensus halt)
#   - AGREEMENT: at a settled height the block hash is identical across all validators, per lane
#   - HEALTH   : all 12 containers (4x CL+EVM-EL+payment-EL) are running
#   - DISK     : free space on the datadir filesystem
# Two spammers (EVM lane + payment lane) are kept running for the whole duration (respawned if they exit).
# Flags any failure but keeps running; prints a final PASS/FAIL tally.
# NOTE: validator1's EVM host port (8545) may be shadowed by a stray anvil; its EVM is read over the
# internal docker network. The other endpoints are read on the host.
export PATH="$HOME/.cargo/bin:$HOME/.foundry/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO"
DUR=${DUR:-3600}; RATE=${RATE:-300}
LOG=/tmp/dualel-soak.log; : > "$LOG"
STOP=/tmp/dualel-soak.stop; rm -f "$STOP"
START=$(date +%s)
fails=0; checks=0
declare -A prev

log(){ echo "[$(date +%H:%M:%S) +$(( $(date +%s)-START ))s] $*" | tee -a "$LOG"; }
bn(){ cast block-number --rpc-url http://127.0.0.1:$1 2>/dev/null; }
bn1evm(){ docker run --rm --network arc_testnet_default curlimages/curl:latest -s -X POST http://validator1_el:8545 \
  -H 'content-type: application/json' --data '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' 2>/dev/null \
  | python3 -c "import sys,json;print(int(json.load(sys.stdin)['result'],16))" 2>/dev/null; }
hashat(){ cast block $2 --rpc-url http://127.0.0.1:$1 --json 2>/dev/null \
  | python3 -c "import sys,json;print(json.load(sys.stdin)['hash'])" 2>/dev/null; }

# --- continuous spammers on both lanes (respawn until STOP exists) ---
spam_loop(){ local tag=$1 tgts=$2; while [ ! -f "$STOP" ]; do
  target/release/spammer ws --targets "$tgts" -r "$RATE" -t 600 -a 500 -g 4 --mix transfer=100 \
    >/tmp/soak_spam_${tag}.log 2>&1; sleep 1; done; }
spam_loop evm "ws://127.0.0.1:8646,ws://127.0.0.1:8746,ws://127.0.0.1:8846" &
spam_loop pay "ws://127.0.0.1:19646,ws://127.0.0.1:19746,ws://127.0.0.1:19846" &

log "SOAK START dur=${DUR}s rate=${RATE}tx/s/lane. 4 nodes, each CL+EVM-EL+payment-EL, continuous dual-lane spam."
while [ $(( $(date +%s) - START )) -lt "$DUR" ]; do
  checks=$((checks+1)); iterfail=0
  e2=$(bn 8645); e3=$(bn 8745); e4=$(bn 8845); e1=$(bn1evm)
  p1=$(bn 19545); p2=$(bn 19645); p3=$(bn 19745); p4=$(bn 19845)
  for k in e1 e2 e3 e4 p1 p2 p3 p4; do cur=${!k}
    [ -z "$cur" ] && { log "WARN $k unreachable"; iterfail=1; continue; }
    if [ -n "${prev[$k]}" ] && [ "$cur" -le "${prev[$k]}" ]; then log "FAIL liveness: $k stalled (${prev[$k]} -> $cur)"; iterfail=1; fi
    prev[$k]=$cur; done
  up=$(docker ps --format '{{.Names}}' | grep -cE 'validator[0-9]+_(cl|el|el_pay)$')
  [ "$up" -ne 12 ] && { log "FAIL health: only $up/12 containers up"; iterfail=1; }
  # agreement at min(payment heads)-10
  mn=$p1; for x in $p2 $p3 $p4; do [ -n "$x" ] && [ "$x" -lt "$mn" ] && mn=$x; done
  H=$((mn-10)); HE=0
  if [ "$H" -gt 1 ]; then
    a=$(hashat 19545 $H); b=$(hashat 19645 $H); c=$(hashat 19745 $H); d=$(hashat 19845 $H)
    if [ -n "$a" ] && { [ "$a" != "$b" ] || [ "$a" != "$c" ] || [ "$a" != "$d" ]; }; then log "FAIL agreement: payment@$H differs: $a $b $c $d"; iterfail=1; fi
    me=$e2; for x in $e3 $e4; do [ -n "$x" ] && [ "$x" -lt "$me" ] && me=$x; done; HE=$((me-10))
    eb=$(hashat 8645 $HE); ec=$(hashat 8745 $HE); ed=$(hashat 8845 $HE)
    if [ -n "$eb" ] && { [ "$eb" != "$ec" ] || [ "$eb" != "$ed" ]; }; then log "FAIL agreement: EVM@$HE differs: $eb $ec $ed"; iterfail=1; fi
  fi
  disk=$(df -BG --output=avail "$REPO" 2>/dev/null | tail -1 | tr -dc '0-9')
  [ -n "$disk" ] && [ "$disk" -lt 20 ] && { log "FAIL disk: only ${disk}G free"; iterfail=1; }
  [ "$iterfail" -ne 0 ] && fails=$((fails+1))
  log "ok=$([ $iterfail -eq 0 ] && echo Y || echo N) EVM[$e1 $e2 $e3 $e4] PAY[$p1 $p2 $p3 $p4] up=$up dagree@pay$H/evm$HE disk=${disk}G fails=$fails/$checks"
  sleep 60
done
touch "$STOP"; sleep 2
log "SOAK DONE. checks=$checks failed_checks=$fails"
[ "$fails" -eq 0 ] && log "RESULT: PASS (no liveness/agreement/health/disk failures over ${DUR}s of continuous dual-lane load)" \
                   || log "RESULT: FAIL ($fails/$checks checks had issues)"
