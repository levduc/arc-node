#!/usr/bin/env bash
# lane-bench.sh — ONE trigger to benchmark either lane on the 4-machine fleet.
#
#   ./lane-bench.sh evm  <gas>            [window_s]      # EVM lane (chainId 1337)
#   ./lane-bench.sh lean <gas> <fanout_N> [window_s]      # lean payment lane (1338)
#
# Examples:
#   ./lane-bench.sh evm 50000000                 # reth lane at 50M
#   ./lane-bench.sh lean 150000000 100 600       # lean lane, N=100, 10-min window
#
# Appends one JSON row per run to $OUT (default /tmp/lane-bench.jsonl) and
# prints it. Everything below is the protocol distilled from the 2026-08-22..24
# campaign — every step here exists because its absence produced a wrong number:
#
#   * per-machine container CENSUS (a validator whose EL never started, or whose
#     CL degenerated into a sync-follower, silently costs ~25% cadence and is
#     invisible to agreement checks)
#   * pool WIPE via staggered node restarts, retried until 4/4 empty (recovery
#     replay grows with chain length; a fixed sleep races it)
#   * chain STATIC check before corpus generation (a surviving feeder advances
#     nonces mid-generation => corpus born stale => queued-forever)
#   * corpus PROBE (submit one tx, require pending>=1) before mass feeding
#   * per-machine corpus GENERATION (never ship 90MB over tailscale)
#   * REMOTE-EFFECT verification (tailscale ssh exit codes lie when the session
#     check expires; verify the artifact, not the return code)
#   * FULLNESS reported with every number (a non-full block measures delivery,
#     not the chain)
#
# Notes: -a must be divisible by -g (spammer constraint). The EVM lane is loaded
# by local spammers (reth gossips txs fleet-wide); the lean lane has no tx
# gossip, so each machine feeds its own node from its own corpus partition.
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 1

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
LANE=${1:?usage: lane-bench.sh evm|lean <gas> [N] [window_s]}
GAS=${2:?gas budget, e.g. 150000000}
if [ "$LANE" = lean ]; then N=${3:-100}; WINDOW=${4:-600}; else N=1; WINDOW=${3:-600}; fi
# N may be a weighted mix spec "1:20,5:20,10:20,50:20,100:20" (weights = tx
# share; passed to the spammer verbatim). AVG_N drives corpus/budget sizing.
case "$N" in
  *:*) AVG_N=$(python3 -c "
import sys
pairs=[p.split(':') for p in '$N'.split(',')]
w=sum(int(b) for _,b in pairs)
print(f'{sum(int(a)*int(b) for a,b in pairs)/w:.2f}')") ;;
  *)   AVG_N=$N ;;
esac
OUT=${OUT:-/tmp/lane-bench.jsonl}
# Topology from fleet.env (copy fleet.env.example). Falls back to the original
# 4-machine fleet so existing invocations keep working.
CFG="$REPO_ROOT/experiments/dual-el/fleet.env"
if [ -f "$CFG" ]; then . "$CFG"; HOSTS=("${FLEET_HOSTS[@]:1}"); IPS=("${FLEET_IPS[@]}");
else HOSTS=(ginnythui papaduck papaduck-alien2); IPS=(100.124.148.61 100.85.150.119 100.70.62.92 100.86.97.40); fi
# Repo-local by default: the lean lane now lives in THIS workspace
# (crates/lean-lane-node), so `cargo build --release -p lean-lane-node` is all a
# fresh machine needs — no reth fork required. Override for an external build.
LEAN_BIN=${LEAN_BIN:-$REPO_ROOT/target/release/lean-lane-node}
FUND=${FUND:-$HOME/lean-fund.txt}
[ -x "$LEAN_BIN" ] || { echo "lean binary missing: $LEAN_BIN  (cargo build --release -p lean-lane-node)"; exit 1; }
[ -s "$FUND" ] || { echo "fund file missing: $FUND  (experiments/dual-el/gen-lean-fund.sh)"; exit 1; }
EVM_RPC=http://127.0.0.1:8545
PC=0x3600000000000000000000000000000000000001
CTRL_KEY=${CTRL_KEY:-0xdbda1821b80551c9d65939329250298aa3472ba22feea921c0cf5d620ea67b97}
export PATH="$PATH:$HOME/.foundry/bin"
say(){ echo "[$(date +%H:%M:%S)] lane-bench: $*"; }
die(){ say "ABORT — $*"; exit 1; }

# ---------------------------------------------------------------- census
say "census"
local_n=$(docker ps --format '{{.Names}}' | grep -c '^validator1_' || true)
[ "$local_n" -ge 2 ] || die "val1 containers: $local_n (need cl+el)"
for i in 2 3 4; do
  h=${HOSTS[$((i-2))]}
  n=$(timeout 45 tailscale ssh papaduck@"$h" "docker ps --format '{{.Names}}' | grep -c '^validator$i''_'" 2>/dev/null | tail -1 | tr -dc '0-9')
  [ -n "$n" ] || die "no ssh/effect from $h (tailscale check expired?)"
  [ "$n" -ge 2 ] || die "$h has $n validator$i containers (EL never started?)"
done
say "census ok (4 machines)"

# --------------------------------------------------------------- load
pkill -9 -f 'release/spammer' 2>/dev/null || true
pkill -9 -f 'lean-feeder.p[y]' 2>/dev/null || true
for h in "${HOSTS[@]}"; do
  timeout 45 tailscale ssh papaduck@"$h" "pkill -9 -f 'arc-spamme[r]' 2>/dev/null; pkill -9 -f 'lean-feeder.p[y]' 2>/dev/null; true" 2>/dev/null
done

if [ "$LANE" = lean ]; then
  # pool wipe (staggered restarts), retried until 4/4 empty
  say "wiping lean pools"
  pid=$(ss -ltnp 2>/dev/null | grep ':8560 ' | grep -oP 'pid=\K[0-9]+' | head -1)
  [ -n "$pid" ] && kill -9 "$pid"; sleep 3
  setsid $LEAN_BIN run --datadir /home/papaduck/lean-lane/val1 --port 8560 --bind 0.0.0.0 \
    --chain-id 1338 --shim --peers "http://${IPS[1]}:8560,http://${IPS[2]}:8560,http://${IPS[3]}:8560" \
    --fund-file $FUND --fund-balance 10000000000000000000 >>/tmp/lean-fleet-val1.log 2>&1 &
  disown
  until curl -s -m4 -X POST http://127.0.0.1:8560 -H 'content-type: application/json' \
      --data '{"jsonrpc":"2.0","id":1,"method":"arc_getHead","params":[]}' 2>/dev/null | grep -q commitment; do sleep 3; done
  for i in 2 3 4; do
    h=${HOSTS[$((i-2))]}; peers=""
    for k in 0 1 2 3; do [ $((k+1)) -eq "$i" ] || peers="${peers}http://${IPS[$k]}:8560,"; done
    timeout 40 tailscale ssh papaduck@"$h" "pkill -9 -f 'lean-lane-node ru[n]' 2>/dev/null; true" 2>/dev/null
    sleep 2
    timeout 75 tailscale ssh papaduck@"$h" "cd /home/papaduck && (setsid nohup ./lean-lane-node run --datadir /home/papaduck/lean-lane/val$i --port 8560 --bind 0.0.0.0 --chain-id 1338 --shim --peers '${peers%,}' --fund-file $FUND --fund-balance 10000000000000000000 >> lean-fleet.log 2>&1 < /dev/null &); sleep 4" 2>/dev/null
    sleep 2
  done
  ok=0; t2=$(date +%s)
  while [ $(( $(date +%s) - t2 )) -lt 300 ]; do
    ok=0
    for ip in "${IPS[@]}"; do
      s=$(curl -s -m8 -X POST http://$ip:8560 -H 'content-type: application/json' \
          --data '{"jsonrpc":"2.0","id":1,"method":"txpool_status","params":[]}' \
          | python3 -c 'import sys,json;r=json.load(sys.stdin)["result"];print(r["pending"]+r["queued"])' 2>/dev/null || echo 9)
      [ "$s" = "0" ] && ok=$((ok+1))
    done
    [ $ok -ge 4 ] && break
    sleep 10
  done
  say "pools wiped: $ok/4"; [ $ok -ge 4 ] || die "pools not empty"
fi

# --------------------------------------------------------- set the budget
if [ "$LANE" = lean ]; then
  say "lean budget -> $GAS"
  python3 -c "
import re,sys
p='.quake/soak4/compose.yaml'; s=open(p).read()
open(p,'w').write(re.sub(r'(ARC_PAYMENT_LEAN_BUDGET_GAS:\s*[\'\"]?)\d+([\'\"]?)', r'\g<1>$GAS\g<2>', s))" || die "compose rewrite"
  docker compose -f .quake/soak4/compose.yaml up -d --force-recreate validator1_cl >/dev/null 2>&1
  for i in 2 3 4; do
    h=${HOSTS[$((i-2))]}
    timeout 90 tailscale ssh papaduck@"$h" "python3 -c \"import re;p='/home/papaduck/arc-fleet/soak4/compose-val$i.yaml';s=open(p).read();open(p,'w').write(re.sub(r'BUDGET_GAS: .[0-9]+.','BUDGET_GAS: \\\"$GAS\\\"',s))\" && docker compose -f /home/papaduck/arc-fleet/soak4/compose-val$i.yaml up -d --force-recreate validator${i}_cl >/dev/null 2>&1" 2>/dev/null
  done
  sleep 12
  got=$(docker exec validator1_cl env 2>/dev/null | grep -o 'BUDGET_GAS=[0-9]*' | cut -d= -f2)
  [ "$got" = "$GAS" ] || die "val1 budget reads $got"
else
  say "evm gas limit -> $GAS (governance tx, priced above base fee)"
  bf=$(curl -s -m8 -X POST $EVM_RPC -H 'content-type: application/json' \
      --data '{"jsonrpc":"2.0","id":1,"method":"eth_getBlockByNumber","params":["latest",false]}' \
      | python3 -c "import sys,json;print(int(json.load(sys.stdin)['result']['baseFeePerGas'],16))")
  mx=$((bf*4))
  cast send $PC "updateFeeParams((uint64,uint64,uint64,uint256,uint256,uint256))" \
    "(20,200,5000,1,1000000000000,$GAS)" --private-key "$CTRL_KEY" --rpc-url $EVM_RPC \
    --timeout 90 --gas-price $mx --priority-gas-price $((mx/2)) >/dev/null 2>&1
  sleep 8
  got=$(curl -s -m8 -X POST $EVM_RPC -H 'content-type: application/json' \
      --data '{"jsonrpc":"2.0","id":1,"method":"eth_getBlockByNumber","params":["latest",false]}' \
      | python3 -c "import sys,json;print(int(json.load(sys.stdin)['result']['gasLimit'],16))")
  [ "$got" = "$GAS" ] || die "gasLimit reads $got"
fi


if [ "$LANE" = evm ]; then
  # reth gossips: local spammers suffice. 6 x 2500/s covers ~100M blocks.
  for i in 0 1 2 3 4 5; do
    setsid ./target/release/spammer ws --targets ws://127.0.0.1:8546 -r 2500 -g 4 -a 160 \
      --account-offset $((i*160)) -t $((WINDOW+300)) --chain-id 1337 --mix transfer=100 -l \
      >/tmp/lb-evm-$i.log 2>&1 &
    disown; sleep 1
  done
  say "6 evm spammers up; warming 60s"
  sleep 60
else

  # chain must be STATIC before -l corpus generation
  a=$(head -1 $FUND | awk '{print $1}')
  n1=$(curl -s -m8 -X POST http://127.0.0.1:8560 -H 'content-type: application/json' \
      --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_getTransactionCount\",\"params\":[\"$a\",\"latest\"]}" \
      | python3 -c 'import sys,json;print(int(json.load(sys.stdin)["result"],16))')
  sleep 12
  n2=$(curl -s -m8 -X POST http://127.0.0.1:8560 -H 'content-type: application/json' \
      --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_getTransactionCount\",\"params\":[\"$a\",\"latest\"]}" \
      | python3 -c 'import sys,json;print(int(json.load(sys.stdin)["result"],16))')
  [ "$n1" = "$n2" ] || die "chain not static ($n1 -> $n2): a feeder is alive"

  # corpus: sized to the window, generated ON each machine, verified by effect
  per_tx=$(python3 -c "print(int(21000+5000*$AVG_N))"); btx=$((GAS/per_tx))
  need=$(( btx*2*WINDOW/4 + 100000 )); gen=$(( need/8000 + 30 ))
  say "corpus target ~$need tx/partition (~${gen}s gen), block budget ${btx} tx"
  rm -f /tmp/lb-corp-0.txt
  ( SPAM_DUMP_FILE=/tmp/lb-corp-0.txt timeout $((gen+90)) ./target/release/spammer ws \
      --targets ws://127.0.0.1:8560 -r 9000 -g 4 -a 200 --account-offset 0 -t $gen \
      --chain-id 1338 --mix fanout=100 --fanout-outputs "$N" -l >/dev/null 2>&1
    cat /tmp/lb-corp-0.txt.[0-9]* > /tmp/lb-corp-0.txt 2>/dev/null; rm -f /tmp/lb-corp-0.txt.[0-9]* ) &
  g0=$!; gp=""
  for i in 1 2 3; do
    h=${HOSTS[$((i-1))]}
    ( timeout $((gen+200)) tailscale ssh papaduck@"$h" "rm -f /tmp/lb-corp.txt /tmp/lb-corp.txt.*; SPAM_DUMP_FILE=/tmp/lb-corp.txt timeout $((gen+90)) /home/papaduck/arc-spammer ws --targets ws://127.0.0.1:8560 -r 9000 -g 4 -a 200 --account-offset $((i*200)) -t $gen --chain-id 1338 --mix fanout=100 --fanout-outputs \"$N\" -l >/dev/null 2>&1; cat /tmp/lb-corp.txt.* > /tmp/lb-corp.txt 2>/dev/null; rm -f /tmp/lb-corp.txt.*; wc -l < /tmp/lb-corp.txt" 2>/dev/null | tail -1 ) &
    gp="$gp $!"
  done
  wait $g0 $gp 2>/dev/null
  for h in "${HOSTS[@]}"; do
    rl=$(timeout 45 tailscale ssh papaduck@"$h" "wc -l < /tmp/lb-corp.txt 2>/dev/null" 2>/dev/null | tail -1 | tr -dc '0-9')
    [ -n "$rl" ] && [ "$rl" -gt 1000 ] || die "remote corpus missing on $h (ssh expired mid-run?)"
  done
  say "corpora ready (local $(wc -l < /tmp/lb-corp-0.txt))"

  # probe: one tx must reach PENDING (not queued)
  tx=$(head -1 /tmp/lb-corp-0.txt)
  curl -s -m8 -X POST http://127.0.0.1:8560 -H 'content-type: application/json' \
    --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_sendRawTransaction\",\"params\":[\"$tx\"]}" >/dev/null
  p=$(curl -s -m8 -X POST http://127.0.0.1:8560 -H 'content-type: application/json' \
      --data '{"jsonrpc":"2.0","id":1,"method":"txpool_status","params":[]}' \
      | python3 -c 'import sys,json;print(json.load(sys.stdin)["result"]["pending"])' 2>/dev/null || echo 0)
  [ "${p:-0}" -ge 1 ] || die "corpus probe: tx did not go pending (stale nonces)"

  # per-node offer ≈ its share of 2 blk/s + headroom; pool target scales with
  # block size (a flat target throttles big-block/high-tx-count arms)
  rate=$(( btx*2/4 + btx/2 ))
  ptarget=$(( btx*4 ))
  [ $ptarget -lt 30000 ] && ptarget=30000
  setsid python3 experiments/dual-el/lean-feeder.py http://127.0.0.1:8560 /tmp/lb-corp-0.txt $ptarget $rate $((WINDOW+200)) >/tmp/lb-feed.log 2>&1 &
  disown
  for h in "${HOSTS[@]}"; do
    timeout 60 tailscale ssh papaduck@"$h" "(setsid nohup python3 /home/papaduck/lean-feeder.py http://127.0.0.1:8560 /tmp/lb-corp.txt $ptarget $rate $((WINDOW+200)) >> /home/papaduck/lb-feed.log 2>&1 < /dev/null &); true" 2>/dev/null
  done
  say "feeders up at ${rate}/s/node; warming 60s"
  sleep 60
fi

# ------------------------------------------------- health gate (pre-measure)
# A CL that booted while its lean node was down parks in "Manual intervention
# required" and never proposes again: agreement checks still pass, but its 1/4
# of rounds burn full timeouts (measured 2026-08-24: val1 failed 108/108 turns,
# cadence 0.68 vs 1.94). Catch it before spending a measurement window.
say "health gate"
for i in 1 2 3 4; do
  if [ $i -eq 1 ]; then
    parked=$(docker logs validator1_cl --since 20m 2>&1 | grep -c 'Manual intervention required' || true)
    live=$(docker logs validator1_cl --since 60s 2>&1 | wc -l)
  else
    h=${HOSTS[$((i-2))]}
    parked=$(timeout 45 tailscale ssh papaduck@"$h" "docker logs validator${i}_cl --since 20m 2>&1 | grep -c 'Manual intervention required'" 2>/dev/null | tail -1 | tr -dc '0-9')
    live=$(timeout 45 tailscale ssh papaduck@"$h" "docker logs validator${i}_cl --since 60s 2>&1 | wc -l" 2>/dev/null | tail -1 | tr -dc '0-9')
  fi
  [ "${parked:-0}" = "0" ] || die "val$i CL is PARKED (booted with its lean node down) — restart it"
  [ "${live:-0}" -gt 10 ] || die "val$i CL emitted ${live:-0} log lines in 60s (hung / not participating)"
done
say "health gate ok (4 CLs live, none parked)"

# ------------------------------------------------------------- measure
say "measuring ${WINDOW}s (silent)"
python3 - "$LANE" "$GAS" "$N" "$WINDOW" "$OUT" <<'PY'
import json, sys, time, urllib.request, base64
lane, gas, N, window, out = sys.argv[1], int(sys.argv[2]), sys.argv[3], int(sys.argv[4]), sys.argv[5]
# N may be a mix spec string; nothing below does arithmetic on it (evm lane
# still gets per-tx 21000 for its own fullness math).
# sample from an UNINVOLVED wired validator (val2) — never the loaded local node
def rpc(url, m, p):
    r = urllib.request.Request(url, data=json.dumps({"jsonrpc":"2.0","id":1,"method":m,"params":p}).encode(),
                               headers={"content-type":"application/json"})
    return json.load(urllib.request.urlopen(r, timeout=25))["result"]
if lane == "evm":
    U="http://127.0.0.1:8545"
    h0=int(rpc(U,"eth_blockNumber",[]),16); t0=time.time(); time.sleep(window)
    h1=int(rpc(U,"eth_blockNumber",[]),16); dt=time.time()-t0
    txs=full=b=0; gas_sum=0
    for n in range(h0+1,h1+1):
        blk=rpc(U,"eth_getBlockByNumber",[hex(n),False])
        txs+=len(blk["transactions"]); b+=1; gu=int(blk["gasUsed"],16); gas_sum+=gu
        full += 1 if gu >= 0.95*gas else 0
    ops=txs
else:
    U="http://100.85.150.119:8560"
    h0=rpc(U,"arc_getHead",{})["number"]; t0=time.time(); time.sleep(window)
    h1=rpc(U,"arc_getHead",{})["number"]; dt=time.time()-t0
    # Mix-safe accounting: fullness by GAS (sum 21000+5000*N_i per tx), which
    # is exact for any N distribution; sigs_blk = txs (one signature each).
    txs=ops=b=full=0; gas_sum=0
    for n in range(h0+1,h1+1):
        bb=base64.b64decode(rpc(U,"arc_getBlockBytes",{"number":n})["blockBytes"])
        ntx=int.from_bytes(bb[48:52],'little'); off=52; o=0; g=0
        for _ in range(ntx):
            l=int.from_bytes(bb[off:off+4],'little'); off+=4
            no=int.from_bytes(bb[off+5:off+7],'little'); o+=no; g+=21000+5000*no; off+=l
        txs+=ntx; ops+=o; b+=1; gas_sum+=g
        full += 1 if g >= 0.95*gas else 0
row=dict(lane=lane,N=N,gas=gas,blocks=b,secs=round(dt),cadence=round(b/dt,2),
         tps=round(txs/dt),ops=round(ops/dt),avg_txs_blk=txs//max(b,1),
         sigs_blk=txs//max(b,1),avg_n=round(ops/max(txs,1),1),
         fullness_pct=round(100*(gas_sum/max(b,1))/gas))
print(json.dumps(row))
open(out,"a").write(json.dumps(row)+"\n")
PY

pkill -9 -f 'release/spammer' 2>/dev/null || true
pkill -9 -f 'lean-feeder.p[y]' 2>/dev/null || true
for h in "${HOSTS[@]}"; do
  timeout 45 tailscale ssh papaduck@"$h" "pkill -9 -f 'arc-spamme[r]' 2>/dev/null; pkill -9 -f 'lean-feeder.p[y]' 2>/dev/null; true" 2>/dev/null
done
say "done (row appended to $OUT)"
