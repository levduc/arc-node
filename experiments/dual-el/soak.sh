#!/usr/bin/env bash
# Soak the dual-EL testnet with CONTINUOUS dual-lane spam and check for consensus errors.
#   DUR       = seconds to run                       (default 3600; use 86400 for 24h)
# Load mimics demo-bloat.sh: EVM lane = GasGuzzler storage-write bloat (fresh slots, state growth),
# payment lane = pool-update transfers over the preseeded accounts + slow organic growth.
#   EVM_RATE  = guzzler tx/s                          (default 6; ~18 tx fill a 100M block)
#   SLOTS     = fresh storage slots per guzzler call  (default 250)
#   PAY_RATE  = payment transfers tx/s                (default 1500)
#   POOL      = recipient pool base:size              (default 0x2000000000:10000000 = the 10M preseed)
#   GROW_RATE = fresh-recipient transfers tx/s        (default 50; new account per tx)
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
DUR=${DUR:-3600}
EVM_RATE=${EVM_RATE:-6}; SLOTS=${SLOTS:-250}
PAY_RATE=${PAY_RATE:-1500}; POOL=${POOL:-0x2000000000:10000000}; GROW_RATE=${GROW_RATE:-50}
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

# --- continuous spammers, demo-bloat profile (respawn until STOP exists; -l re-syncs nonces
# on every respawn so an EL restart mid-soak can not nonce-wedge the load) ---
# Override for multi-machine fleets, e.g.:
#   EVM_TGTS="ws://127.0.0.1:8546,ws://100.85.150.119:8646,ws://100.70.62.92:8746,ws://100.86.97.40:8846"
#   PAY_TGTS="ws://127.0.0.1:19546,ws://100.85.150.119:19646,ws://100.70.62.92:19746,ws://100.86.97.40:19846"
EVM_TGTS=${EVM_TGTS:-"ws://127.0.0.1:8646,ws://127.0.0.1:8746,ws://127.0.0.1:8846"}
PAY_TGTS=${PAY_TGTS:-"ws://127.0.0.1:19546,ws://127.0.0.1:19646,ws://127.0.0.1:19746,ws://127.0.0.1:19846"}
bloat_loop(){ while [ ! -f "$STOP" ]; do
  target/release/spammer ws --targets "$EVM_TGTS" -r "$EVM_RATE" -t 600 -g 2 -a 200 -l \
    --mix guzzler=100 --guzzler-fn-weights "storage-write=100@${SLOTS}" \
    >/tmp/soak_spam_evm.log 2>&1; sleep 1; done; }
pay_loop(){ while [ ! -f "$STOP" ]; do
  target/release/spammer ws --targets "$PAY_TGTS" -r "$PAY_RATE" -t 600 -g 8 -a 1000 -l \
    --recipient-pool "$POOL" --mix transfer=100 \
    >/tmp/soak_spam_pay.log 2>&1; sleep 1; done; }
grow_loop(){ while [ ! -f "$STOP" ]; do
  target/release/spammer ws --targets "$PAY_TGTS" -r "$GROW_RATE" -t 600 -g 2 -a 200 -l \
    --fresh-recipients --mix transfer=100 \
    >/tmp/soak_spam_grow.log 2>&1; sleep 1; done; }
bloat_loop & pay_loop & grow_loop &

log "SOAK START dur=${DUR}s evm=${EVM_RATE}tx/s guzzler@${SLOTS} pay=${PAY_RATE}tx/s pool=${POOL} grow=${GROW_RATE}tx/s. 4 nodes, demo-bloat load profile."
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
