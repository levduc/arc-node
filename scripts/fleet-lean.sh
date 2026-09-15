#!/usr/bin/env bash
# fleet-lean.sh up|load|status|health|restart-cl <n>|kill-lean <n> <secs>|down
#
# The multi-host sibling of scripts/lean-testnet.sh: the same lean payment lane,
# but validator1 runs here and validators 2..4 each run on their own machine over
# tailscale. One lean-lane node per machine on :8560 (a host process, NOT a
# container), reached from that machine's CL container through the docker bridge
# gateway 172.17.0.1.
#
#   cp scripts/fleet.env.example scripts/fleet.env   # hosts, IPs, load shape
#   scripts/fleet-lean.sh up          # ship + boot + health gate
#   scripts/fleet-lean.sh load        # LOAD_SECS of fan-out load, 60 s sampler
#   DISTRIBUTED=1 GENERATORS=8 scripts/fleet-lean.sh load   # one spammer per machine
#   scripts/fleet-lean.sh status
#   scripts/fleet-lean.sh kill-lean 3 60     # Track A: lean node outage
#   scripts/fleet-lean.sh restart-cl 3       # Track A: CL restart
#   scripts/fleet-lean.sh down
#
# EVERY byte a run writes on a machine lives under $FLEET_ROOT/<run-id>/ :
#   quake/  compose + assets + validatorN config      lean/   lean datadir
#   logs/   lean.log, spammer.log                     fund.txt, lean-lane-node, spammer
# Nothing else in $HOME is created, modified or removed, and `down` deletes no
# data — removing a run is the operator's `rm -rf`, never the script's.
# Validator 2's machine runs the owner's personal services: the only containers
# this script ever touches are the validatorN_cl / validatorN_el of its own
# compose files, and it never prunes anything.
#
# Ops rules baked in (CLAUDE.md §6): lean nodes start BEFORE the CLs (a CL that
# boots while its lean node is down parks forever); tailscale ssh exit codes lie,
# so every remote effect is verified by reading the effect back; kill and launch
# are separate ssh calls; remote daemons are `(setsid nohup .. &) < /dev/null`.
set -uo pipefail
REPO=$(cd "$(dirname "$0")/.." && pwd); cd "$REPO" || exit 1

CFG=${FLEET_ENV:-$REPO/scripts/fleet.env}
[ -f "$CFG" ] || CFG=$REPO/scripts/fleet.env.example
# shellcheck disable=SC1090
source "$CFG"

SCEN=${SCENARIO:-fleet4-lean}
MANIFEST=crates/quake/scenarios/$SCEN.toml
QDIR=$REPO/.quake/$SCEN
RUNS=$REPO/.quake/fleet-runs
QUAKE=${QUAKE:-$REPO/target/release/quake}
N=4
CHAIN=${LEAN_CHAIN_ID:-1338}
LEAN_PORT=8560
BUDGET_TXS=${BUDGET_TXS:-553}     # 150 M gas / (21000 + 5000*50) at N=50 = a 100 %-full block
LEAN_SRC=${LEAN_BIN:-$LEAN_LANE_DIR/target/release/lean-lane-node}
SPAM_SRC=${SPAMMER_BIN:-$LEAN_LANE_DIR/target/release/spammer}
IMAGES=${IMAGES:-"arc_consensus:latest arc_execution:latest"}
SSH_TMO=${SSH_TMO:-120}
GENERATORS=${GENERATORS:-2}

ACTION=${1:-}; shift 2>/dev/null || true
if [ -z "${RUN_ID:-}" ]; then
  if [ "$ACTION" = up ] || [ ! -s "$RUNS/latest" ]; then RUN_ID=v02-$(date +%m%d-%H%M)
  else RUN_ID=$(cat "$RUNS/latest"); fi
fi
ROOT=$FLEET_ROOT/$RUN_ID          # identical path on all four machines
LOCAL=$RUNS/$RUN_ID               # this machine's own artefacts (samples, logs, fund)

say(){ echo "[$(date +%H:%M:%S)] fleet($RUN_ID): $*"; }
die(){ echo "fleet: FAIL: $*" >&2; exit 1; }

ip_of(){ echo "${FLEET_IPS[$1]}"; }
name_of(){ echo "${FLEET_NAMES[$1]}"; }
# rsh <n> <command-string>  — validator 1 is this machine
rsh(){ local n=$1; shift
  if [ "$n" = 1 ]; then bash -c "$*"
  else timeout "$SSH_TMO" tailscale ssh "$REMOTE_USER@$(ip_of "$n")" "$*" 2>/dev/null; fi; }

rpc(){ # rpc <url> <method> <json-params> — body through a FILE (a full block is ~1 MB)
  local body; body=$(mktemp)
  printf '{"jsonrpc":"2.0","id":1,"method":"%s","params":%s}' "$2" "$3" > "$body"
  curl -s -m "${RPC_TMO:-25}" -X POST "$1" -H 'content-type: application/json' --data-binary "@$body"; rm -f "$body"; }
jget(){ python3 -c 'import sys,json
d=json.load(sys.stdin)
for k in sys.argv[1].split("."):
    d=d[k]
print(d)' "$1" 2>/dev/null; }
lean_url(){ echo "http://$(ip_of "$1"):$LEAN_PORT"; }
el_url(){ echo "http://$(ip_of "$1"):$((8545 + ($1 - 1) * 100))"; }
compose_of(){ if [ "$1" = 1 ]; then echo "$QDIR/compose.yaml"; else echo "$ROOT/compose-val$1.yaml"; fi; }

lean_head(){ rpc "$(lean_url "$1")" arc_getHead '{}'; }
lean_height(){ local h; h=$(lean_head "$1" | jget result.number); echo "${h:-0}"; }
el_height(){ local h; h=$(rpc "$(el_url "$1")" eth_blockNumber '[]' | jget result); [ -n "$h" ] && echo $((h)) || echo 0; }
# tx count of a lean block: bytes 48..52 of the block wire format, little endian
block_txs(){ rpc "$(lean_url "$1")" arc_getBlockBytes "{\"number\":$2}" | python3 -c 'import sys,json,base64
try:
    b=base64.b64decode(json.load(sys.stdin)["result"]["blockBytes"] or "")
    print(int.from_bytes(b[48:52],"little") if len(b)>=52 else 0)
except Exception: print(0)' 2>/dev/null; }
pool_depth(){ rpc "$(lean_url "$1")" txpool_status '{}' | python3 -c 'import sys,json
try:
    r=json.load(sys.stdin)["result"]; v=r.get("pending",0)
    print(int(v,16) if isinstance(v,str) else int(v))
except Exception: print(-1)' 2>/dev/null; }
cl_restarts(){ rsh "$1" "docker inspect --format '{{.RestartCount}}' validator$1_cl 2>/dev/null" | tr -dc '0-9'; }
cl_parked(){ rsh "$1" "docker logs validator$1_cl 2>&1 | grep -c 'Manual intervention'" | tr -dc '0-9'; }
# docker does NOT increment RestartCount for a MANUAL `docker restart` (only for
# restart-policy restarts), so the evidence that a container really bounced is
# State.StartedAt; RestartCount is the evidence that it did NOT crash.
cl_started(){ rsh "$1" "docker inspect --format '{{.State.StartedAt}}' validator$1_cl 2>/dev/null" | tr -d ' \r'; }

peers_for(){ local n=$1 p="" j; for j in $(seq 1 $N); do
    [ "$j" = "$n" ] || p="${p}http://$(ip_of "$j"):$LEAN_PORT,"; done; echo "${p%,}"; }
lean_launch_cmd(){ local n=$1
  echo "(setsid nohup $ROOT/lean-lane-node run --datadir $ROOT/lean --port $LEAN_PORT --bind 0.0.0.0 --chain-id $CHAIN --shim --peers '$(peers_for "$n")' --fund-file $ROOT/fund.txt --fund-balance 10000000000000000000 >> $ROOT/logs/lean.log 2>&1 < /dev/null &)"; }
# the lean pid on machine n, matched by the BINARY PATH in argv[1] (never `pkill -f`,
# which matches the shell carrying the pattern — CLAUDE.md §6)
lean_pid(){ rsh "$1" "ps -eo pid=,args= | awk -v b=$ROOT/lean-lane-node '\$2==b{print \$1}'" | tr -dc '0-9 ' | awk '{print $1}'; }
lean_kill(){ local n=$1 p; p=$(lean_pid "$n"); [ -n "$p" ] || return 0
  rsh "$n" "kill $p 2>/dev/null; true"; sleep 2
  p=$(lean_pid "$n"); [ -n "$p" ] && rsh "$n" "kill -9 $p 2>/dev/null; true"; return 0; }
lean_wait(){ local n=$1 t=${2:-90} i
  for i in $(seq 1 "$t"); do lean_head "$n" | grep -q commitment && return 0; sleep 1; done; return 1; }

# ---------------------------------------------------------------- shipping
ship_file(){ # ship_file <n> <local> <remote>  — sha-verified AFTER transfer
  local n=$1 src=$2 dst=$3 want have
  want=$(sha256sum "$src" | cut -c1-16)
  have=$(rsh "$n" "sha256sum $dst 2>/dev/null | cut -c1-16" | tail -1 | tr -dc '0-9a-f')
  if [ "$have" = "$want" ]; then echo "    $(basename "$dst"): current"; return 0; fi
  gzip -c "$src" | timeout 900 tailscale ssh "$REMOTE_USER@$(ip_of "$n")" \
    "cat > $dst.gz && gunzip -f $dst.gz && chmod +x $dst 2>/dev/null; true" >/dev/null 2>&1
  have=$(rsh "$n" "sha256sum $dst 2>/dev/null | cut -c1-16" | tail -1 | tr -dc '0-9a-f')
  [ "$have" = "$want" ] && { echo "    $(basename "$dst"): shipped ok"; return 0; }
  echo "    $(basename "$dst"): FAILED (have ${have:-none}, want $want)"; return 1; }

img_probe(){ case "$1" in arc_execution*) echo /usr/local/bin/arc-node-execution;; *) echo /usr/local/bin/arc-node-consensus;; esac; }
ship_image(){ # ship_image <n> <image> — verified by the sha of the BINARY INSIDE the image
  local n=$1 img=$2 probe want have
  probe=$(img_probe "$img")
  want=$(docker run --rm --entrypoint sha256sum "$img" "$probe" 2>/dev/null | awk '{print $1}')
  [ -n "$want" ] || { echo "    $img: cannot hash locally"; return 1; }
  have=$(rsh "$n" "docker run --rm --entrypoint sha256sum $img $probe 2>/dev/null | awk '{print \$1}'" | tr -dc '0-9a-f')
  if [ "$have" = "$want" ]; then echo "    $img: current (${want:0:12})"; return 0; fi
  echo "    $img: shipping (local ${want:0:12}, remote ${have:0:12})..."
  docker save "$img" | gzip -1 | timeout 3600 tailscale ssh "$REMOTE_USER@$(ip_of "$n")" "gunzip | docker load" >/dev/null 2>&1
  have=$(rsh "$n" "docker run --rm --entrypoint sha256sum $img $probe 2>/dev/null | awk '{print \$1}'" | tr -dc '0-9a-f')
  [ "$have" = "$want" ] && { echo "    $img: shipped ok (${want:0:12})"; return 0; }
  echo "    $img: CONTENT MISMATCH (remote ${have:0:12} != local ${want:0:12}) — truncated pipe?"; return 1; }

# ---------------------------------------------------------------- up
up(){
  [ -f "$MANIFEST" ] || die "no scenario $MANIFEST"
  [ -x "$LEAN_SRC" ]  || die "no lean node binary at $LEAN_SRC"
  [ -x "$SPAM_SRC" ]  || die "no spammer at $SPAM_SRC"
  [ -x "$QUAKE" ]     || die "no quake at $QUAKE"
  for img in $IMAGES; do docker image inspect "$img" >/dev/null 2>&1 || die "local image $img missing (make build-docker)"; done
  mkdir -p "$LOCAL" "$RUNS"; echo "$RUN_ID" > "$RUNS/latest"

  say "reachability + run root on all $N machines"
  for n in $(seq 1 $N); do
    rsh "$n" "mkdir -p $ROOT/quake $ROOT/lean $ROOT/logs && echo OK" | grep -q OK \
      || die "validator$n ($(name_of "$n") $(ip_of "$n")) unreachable or cannot write $ROOT"
    echo "    validator$n $(name_of "$n") $(ip_of "$n"): $ROOT ready"
  done

  # ---- one fund file, shared by all four (the spammer signs exactly these accounts)
  FUND=$LOCAL/fund.txt
  if [ ! -s "$FUND" ]; then
    say "generating $FUND_ACCOUNTS funded accounts (m/44'/60'/1'/0/i)"
    PATH="$PATH:$HOME/.foundry/bin" bash "$LEAN_LANE_DIR/scripts/gen-lean-fund.sh" "$FUND_ACCOUNTS" "$FUND" >/dev/null 2>&1 \
      || die "fund-file generation failed (foundry's cast on PATH?)"
  fi
  say "fund file: $(wc -l < "$FUND") accounts, sha $(sha256sum "$FUND" | cut -c1-16)"

  # ---- generate the testnet files locally (setup renders, it does NOT start containers)
  say "quake setup -f $MANIFEST --force (render only)"
  rm -rf "$QDIR"
  "$QUAKE" -f "$MANIFEST" setup --force > "$LOCAL/quake-setup.log" 2>&1 \
    || { tail -20 "$LOCAL/quake-setup.log"; die "quake setup failed (see $LOCAL/quake-setup.log)"; }
  [ -f "$QDIR/compose.yaml" ] || die "quake setup produced no compose.yaml"
  say "fleet surgery"
  python3 scripts/fleet-split-compose.py --scenario "$SCEN" --run-id "$RUN_ID" \
    --fleet-root "$FLEET_ROOT" --ips "$(ip_of 1),$(ip_of 2),$(ip_of 3),$(ip_of 4)" || die "compose split failed"
  cp "$QDIR"/compose-val*.yaml "$LOCAL/" 2>/dev/null; cp "$QDIR/compose.yaml" "$LOCAL/compose-val1.yaml"

  # ---- ship everything
  rc=0
  for n in $(seq 2 $N); do
    say "shipping to validator$n ($(name_of "$n"))"
    for img in $IMAGES; do ship_image "$n" "$img" || rc=1; done
    tar cz -C "$QDIR" assets "validator$n" logs 2>/dev/null \
      | timeout 600 tailscale ssh "$REMOTE_USER@$(ip_of "$n")" "tar xz -C $ROOT/quake" >/dev/null 2>&1
    rsh "$n" "test -f $ROOT/quake/assets/genesis.json && test -f $ROOT/quake/validator$n/malachite/config/priv_validator_key.json && echo OK" \
      | grep -q OK || { echo "    quake/: FAILED to unpack"; rc=1; }
    gs=$(sha256sum "$QDIR/assets/genesis.json" | cut -c1-16)
    rgs=$(rsh "$n" "sha256sum $ROOT/quake/assets/genesis.json | cut -c1-16" | tail -1 | tr -dc '0-9a-f')
    [ "$gs" = "$rgs" ] && echo "    quake/: unpacked, genesis $gs" || { echo "    quake/: genesis MISMATCH ($rgs != $gs)"; rc=1; }
    ship_file "$n" "$QDIR/compose-val$n.yaml" "$ROOT/compose-val$n.yaml" || rc=1
    ship_file "$n" "$LEAN_SRC" "$ROOT/lean-lane-node" || rc=1
    ship_file "$n" "$SPAM_SRC" "$ROOT/spammer" || rc=1
    ship_file "$n" "$FUND"     "$ROOT/fund.txt" || rc=1
  done
  say "staging validator1 locally"
  cp -f "$LEAN_SRC" "$ROOT/lean-lane-node"; cp -f "$SPAM_SRC" "$ROOT/spammer"; cp -f "$FUND" "$ROOT/fund.txt"
  [ $rc = 0 ] || die "one or more transfers failed — nothing was started"

  # ---- lean nodes FIRST (a CL that boots while its lean node is down parks)
  say "starting lean nodes (fresh datadirs)"
  for n in $(seq 1 $N); do lean_kill "$n"; done
  for n in $(seq 1 $N); do rsh "$n" "rm -rf $ROOT/lean && mkdir -p $ROOT/lean $ROOT/logs"; done
  for n in $(seq 1 $N); do rsh "$n" "$(lean_launch_cmd "$n")"; done
  for n in $(seq 1 $N); do lean_wait "$n" 90 || { rsh "$n" "tail -5 $ROOT/logs/lean.log"; die "lean node $n did not answer on $(lean_url "$n")"; }; done
  g1=$(lean_head 1 | jget result.commitment)
  for n in $(seq 2 $N); do
    [ "$(lean_head "$n" | jget result.commitment)" = "$g1" ] || die "lean genesis mismatch on validator$n (different fund file?)"; done
  say "$N lean nodes up, genesis ${g1:0:18}..."

  # ---- containers
  say "starting CL+EL containers"
  docker compose -f "$QDIR/compose.yaml" up -d >/dev/null 2>&1 || die "local compose up failed"
  up_ct=$(docker ps --format '{{.Names}}' | grep -c '^validator1') 
  [ "${up_ct:-0}" = 2 ] || die "validator1: $up_ct/2 containers running after compose up"
  for n in $(seq 2 $N); do
    rsh "$n" "docker compose -f $ROOT/compose-val$n.yaml up -d" >/dev/null 2>&1
    up_ct=$(rsh "$n" "docker ps --format '{{.Names}}' | grep -c '^validator$n'" | tr -dc '0-9')
    [ "${up_ct:-0}" = 2 ] || die "validator$n: $up_ct/2 containers running after compose up"
  done
  health_gate
  status
  say "up. next: $0 load"
}

# ---------------------------------------------------------------- health gate
health_gate(){
  say "health gate"
  local bad=0 n ct parked h1 h2
  for n in $(seq 1 $N); do
    ct=$(rsh "$n" "docker ps --format '{{.Names}}' | grep -c '^validator$n'" | tr -dc '0-9')
    parked=$(cl_parked "$n")
    printf '    validator%s  containers %s/2  "Manual intervention" %s\n' "$n" "${ct:-0}" "${parked:-?}"
    [ "${ct:-0}" = 2 ] || bad=1
    [ "${parked:-1}" = 0 ] || bad=1
  done
  [ $bad = 0 ] || { for n in $(seq 1 $N); do echo "--- validator${n}_cl"; rsh "$n" "docker logs validator${n}_cl --tail 30 2>&1"; done; die "census/park gate failed"; }
  say "waiting for both chains to advance on all $N (up to 180 s)"
  local ok=0 i
  for i in $(seq 1 60); do
    ok=1
    for n in $(seq 1 $N); do
      [ "$(lean_height "$n")" -ge 3 ] 2>/dev/null || ok=0
      [ "$(el_height "$n")" -ge 3 ] 2>/dev/null || ok=0
    done
    [ $ok = 1 ] && break; sleep 3
  done
  [ $ok = 1 ] || { for n in $(seq 1 $N); do echo "validator$n lean=$(lean_height "$n") el=$(el_height "$n")"; done; die "chains did not reach height 3 on all $N"; }
  # advancing, not merely non-zero
  declare -a a b
  for n in $(seq 1 $N); do a[$n]=$(lean_height "$n"); done
  sleep 15
  for n in $(seq 1 $N); do b[$n]=$(lean_height "$n")
    printf '    validator%s lean %s -> %s   EL %s\n' "$n" "${a[$n]}" "${b[$n]}" "$(el_height "$n")"
    [ "${b[$n]}" -gt "${a[$n]}" ] || bad=1; done
  [ $bad = 0 ] || die "a lean chain is not advancing"
  say "health gate PASS"
}

# ---------------------------------------------------------------- status
status(){
  echo "validator  host             EL height   lean height   head txs   lean head"
  local n nums=() el h c txs
  for n in $(seq 1 $N); do
    el=$(el_height "$n"); h=$(lean_head "$n"); c=$(echo "$h" | jget result.commitment); h=$(echo "$h" | jget result.number)
    nums+=("${h:-0}"); txs=$(block_txs "$n" "${h:-0}")
    printf 'validator%-1s  %-15s  %-10s  %-12s  %-9s  %s\n' "$n" "$(name_of "$n")" "$el" "${h:-?}" "${txs:-?}" "${c:0:18}..."
  done
  local min; min=$(printf '%s\n' "${nums[@]}" | sort -n | head -1)
  if [ "${min:-0}" -gt 0 ] 2>/dev/null; then
    local distinct; distinct=$(for n in $(seq 1 $N); do
      rpc "$(lean_url "$n")" arc_getBlockBytes "{\"number\":$min}" | jget result.blockBytes | sha256sum | cut -c1-16; done | sort -u | wc -l)
    [ "$distinct" = 1 ] && echo "agreement: all $N lean nodes byte-identical at height $min" \
                        || echo "DIVERGENCE: $distinct distinct lean blocks at height $min"
  fi
}

# ---------------------------------------------------------------- load
load(){
  mkdir -p "$LOCAL"
  local n
  say "  full block at this budget = $BUDGET_TXS txs (a number without fullness is not a result)"
  # Ingress is PER LEAN NODE (the lane does not propagate transactions between
  # nodes — guide §6: "the proposer packs what it has"), so DISTRIBUTED=1 puts
  # one spammer on each machine against its own node over loopback, with a
  # disjoint account range each. One spammer here driving all four over the
  # tailnet is delivery-bound long before the chain is: in backpressure mode the
  # send rate is (generators / RTT), and the tailnet RTT is not the loopback's.
  if [ "${DISTRIBUTED:-0}" = 1 ]; then
    local per=$((FUND_ACCOUNTS / N)) off
    say "load (DISTRIBUTED): one spammer per machine, ${LOAD_RATE} tx/s x ${LOAD_SECS}s each,"
    say "  N=${FANOUT} outputs/tx, $per accounts each (disjoint ranges), -g ${GENERATORS}, pool-target ${POOL_TARGET}"
    for n in $(seq 1 $N); do
      off=$(( (n - 1) * per ))
      rsh "$n" "(setsid nohup $ROOT/spammer ws --targets ws://127.0.0.1:$LEAN_PORT -r $LOAD_RATE -g $GENERATORS -a $per --account-offset $off -t $LOAD_SECS --pool-target $POOL_TARGET --chain-id $CHAIN --mix fanout=100 --fanout-outputs $FANOUT -l > $ROOT/logs/spammer.log 2>&1 < /dev/null &)"
      echo "    validator$n: accounts [$off,$((off + per - 1))] -> ws://127.0.0.1:$LEAN_PORT"
    done
    ( sleep "$LOAD_SECS" ) & local sp=$!
    sample_loop "$sp"
    wait "$sp" 2>/dev/null
    for n in $(seq 1 $N); do echo "--- validator$n spammer"; rsh "$n" "tail -2 $ROOT/logs/spammer.log"; done
    status; return 0
  fi
  local targets=""
  for n in $(seq 1 $N); do targets="${targets}ws://$(ip_of "$n"):$LEAN_PORT,"; done
  targets=${targets%,}
  say "load: ${LOAD_RATE} tx/s x ${LOAD_SECS}s, N=${FANOUT} outputs/tx, ${FUND_ACCOUNTS} accounts, -g ${GENERATORS}, pool-target ${POOL_TARGET}"
  ( "$SPAM_SRC" ws --targets "$targets" -r "$LOAD_RATE" -g "$GENERATORS" -a "$FUND_ACCOUNTS" -t "$LOAD_SECS" \
      --pool-target "$POOL_TARGET" --chain-id "$CHAIN" --mix fanout=100 --fanout-outputs "$FANOUT" -l \
      > "$LOCAL/spammer.log" 2>&1 ) & local sp=$!
  sample_loop "$sp"
  wait "$sp" 2>/dev/null
  tail -5 "$LOCAL/spammer.log"
  status
}

sample_loop(){ # sample every 60 s while the spammer (pid $1) runs
  # Two separate per-validator columns, both labelled: pool1/2/3/4 is the PENDING
  # txpool depth of each lean node (ingress health — the lane does not propagate
  # transactions, so a proposer can only pack what its OWN pool holds, and one
  # shallow pool is the thing that silently caps fullness), and restarts1/2/3/4 is
  # each CL container's RestartCount (0 throughout = no CL ever crash-restarted).
  local sp=$1 jf=$LOCAL/run.jsonl t0 prev_h=-1 prev_t=0 s=0
  t0=$(date +%s)
  printf '%-6s %-7s %-9s %-9s %-7s %-9s %-9s %-22s %s\n' \
    "t(s)" "leanH" "blk/min" "headTxs" "full%" "tx/s" "pay/s" "pool1/2/3/4" "restarts1/2/3/4"
  while kill -0 "$sp" 2>/dev/null; do
    sleep 60
    kill -0 "$sp" 2>/dev/null || break
    s=$((s+1)); local t; t=$(( $(date +%s) - t0 ))
    local h txs mean sum=0 k cnt=0 i n
    local -a pool=() rst=()
    local pd rst_c pools rsts
    h=$(lean_height 1); txs=$(block_txs 1 "$h")
    # mean fullness over the last 5 blocks (node 1 is local, so this is loopback)
    for i in 0 1 2 3 4; do k=$(block_txs 1 $((h-i))); [ "${k:-0}" -gt 0 ] 2>/dev/null && { sum=$((sum+k)); cnt=$((cnt+1)); }; done
    mean=0; [ $cnt -gt 0 ] && mean=$((sum/cnt))
    # pool depth AND restart count on every validator, not just node 1
    for n in $(seq 1 $N); do
      pd=$(pool_depth "$n"); rst_c=$(cl_restarts "$n")
      pool+=("${pd:--1}"); rst+=("${rst_c:--1}")
    done
    pools=$(IFS=/; echo "${pool[*]}"); rsts=$(IFS=/; echo "${rst[*]}")
    local bpm=0 tps=0 pps=0 full=0
    if [ "$prev_h" -ge 0 ] && [ $((t-prev_t)) -gt 0 ]; then
      bpm=$(python3 -c "print(f'{($h-$prev_h)*60/($t-$prev_t):.2f}')")
      tps=$(python3 -c "print(f'{($h-$prev_h)*$mean/($t-$prev_t):.0f}')")
      pps=$(python3 -c "print(f'{($h-$prev_h)*$mean*$FANOUT/($t-$prev_t):.0f}')")
    fi
    full=$(python3 -c "print(f'{100*$mean/$BUDGET_TXS:.0f}')")
    printf '%-6s %-7s %-9s %-9s %-7s %-9s %-9s %-22s %s\n' \
      "$t" "$h" "$bpm" "${txs:-?}" "$full" "$tps" "$pps" "$pools" "$rsts"
    python3 -c "
import json
print(json.dumps({'t':$t,'sample':$s,'lean_height':$h,'blk_per_min':'$bpm','head_txs':${txs:-0},
 'mean_txs_5blk':$mean,'full_pct':'$full','tx_per_s':'$tps','payments_per_s':'$pps',
 'pool':[$(IFS=,; echo "${pool[*]}")],'restarts':[$(IFS=,; echo "${rst[*]}")],
 'budget_txs':$BUDGET_TXS,'fanout':$FANOUT}))" >> "$jf"
    prev_h=$h; prev_t=$t
  done
  say "samples -> $jf"
}

# ---------------------------------------------------------------- Track A legs
restart_cl(){ # restart-cl <n>: bounce the CL container; the lean node stays up
  local n=${1:?validator index} i max me before started0
  before=$(cl_restarts "$n"); started0=$(cl_started "$n")
  say "leg restart-cl $n (restarts=${before:-?}, StartedAt=$started0)"
  rsh "$n" "docker restart validator${n}_cl" >/dev/null 2>&1
  for i in $(seq 1 60); do
    max=0; me=0; local j h
    for j in $(seq 1 $N); do h=$(lean_height "$j"); [ "${h:-0}" -gt "$max" ] && max=$h; [ "$j" = "$n" ] && me=${h:-0}; done
    if [ $((max - me)) -le 3 ] && [ "$me" -gt 0 ]; then
      say "validator$n rejoined within 3 of the tip ($me/$max) after $((i*2)) s; parked=$(cl_parked "$n"); restarts $before -> $(cl_restarts "$n"); StartedAt $started0 -> $(cl_started "$n")"
      status; return 0; fi
    sleep 2
  done
  rsh "$n" "docker logs validator${n}_cl --tail 40 2>&1"; die "validator$n did not rejoin within 120 s"
}

kill_lean(){ # kill-lean <n> <secs>: the lean node dies; its CL must survive and re-sign
  local n=${1:?validator index} secs=${2:-60} i max me
  local r0 h0 hmax0; r0=$(cl_restarts "$n"); h0=$(lean_height "$n"); hmax0=$(lean_height 1)
  say "leg kill-lean $n for ${secs}s (lean h=$h0, val1 h=$hmax0, CL restarts=$r0)"
  lean_kill "$n"
  [ -z "$(lean_pid "$n")" ] || die "lean node $n still alive after kill"
  say "  lean node $n down; chain should keep advancing on the other three"
  sleep "$secs"
  local hmax1; hmax1=$(lean_height 1)
  say "  during the outage val1 lean went $hmax0 -> $hmax1 (+$((hmax1-hmax0)))"
  rsh "$n" "$(lean_launch_cmd "$n")"
  lean_wait "$n" 90 || { rsh "$n" "tail -10 $ROOT/logs/lean.log"; die "lean node $n did not come back"; }
  for i in $(seq 1 90); do
    max=0; me=0; local j h
    for j in $(seq 1 $N); do h=$(lean_height "$j"); [ "${h:-0}" -gt "$max" ] && max=$h; [ "$j" = "$n" ] && me=${h:-0}; done
    if [ $((max - me)) -le 3 ] && [ "$me" -gt "$h0" ]; then
      local r1; r1=$(cl_restarts "$n")
      say "validator$n lean caught up ($me/$max) after $((i*2)) s; CL restarts $r0 -> $r1; parked=$(cl_parked "$n")"
      status; return 0; fi
    sleep 2
  done
  rsh "$n" "tail -20 $ROOT/logs/lean.log; docker logs validator${n}_cl --tail 30 2>&1"
  die "validator$n lean did not catch up after the outage"
}

# ---------------------------------------------------------------- down
down(){
  local n
  for n in $(seq 1 $N); do
    say "validator$n: compose down + lean node stop"
    rsh "$n" "docker compose -f $(compose_of "$n") down" >/dev/null 2>&1
    lean_kill "$n"
    local left; left=$(rsh "$n" "docker ps --format '{{.Names}}' | grep -c '^validator$n'" | tr -dc '0-9')
    echo "    containers left: ${left:-?}  lean pid left: '$(lean_pid "$n")'"
  done
  say "down. ALL data kept under $ROOT on every machine (removing a run is yours: rm -rf $ROOT)"
}

case "$ACTION" in
  up) up ;;
  load) load ;;
  status) status ;;
  health) health_gate ;;
  restart-cl) restart_cl "$@" ;;
  kill-lean) kill_lean "$@" ;;
  down) down ;;
  *) echo "usage: $0 up|load|status|health|restart-cl <n>|kill-lean <n> <secs>|down"; exit 2 ;;
esac
