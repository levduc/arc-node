#!/usr/bin/env bash
# FAIR MPT-vs-SALT comparison: identical workload on both lanes, only the commitment differs.
#
#   ./fair-compare.sh [DURATION_SECS] [RATE_PER_LANE]     defaults: 1800s, 300 tx/s
#
# WHY THIS SCRIPT EXISTS
# Every casual both-lane reading we took was invalid for a different reason, and each failure is
# now a guard rail below:
#   - stale `spam-fleet.sh` / `headtohead.sh` respawn loops kept driving 6500 tx/s into the payment
#     lane while the EVM lane got 300, producing 4761-vs-12 txs/block. pkill did not stop them
#     because the harnesses respawn their children; they must be killed at the PARENT.
#   - the demo-bloat profile runs guzzler on EVM and transfers on PAY. That is a great DEMO (589x
#     volume, 261x lower per-tx cost) but it compares WORKLOADS, not commitments.
#   - a lane whose blocks are fuller looks cheaper per tx purely because per-block fixed costs
#     amortize -- and for SALT specifically, MSM batching means fullness genuinely changes its
#     per-account cost (13.7 us/acct at ~400/block -> 3.65 us/acct at ~4250/block).
#
# WHAT IS CONTROLLED
#   same genesis (both lanes use assets/genesis.json: 100M gas limit, same alloc, chainId 1337)
#   same tx type (native transfers), same rate, same recipient mode, same machines, same duration
#   exec_ms and persist_ms are REPORTED AS CONTROLS: if they track each other, the root_ms delta is
#   attributable to the commitment. If they diverge, the run is not clean and the script says so.
#
# WHAT CANNOT BE CONTROLLED (reported every run; these FLATTER SALT -- do not quietly drop them)
#   1. SALT commits (nonce, balance) only; an MPT account leaf is RLP(nonce, balance, storageRoot,
#      codeHash) -- two extra 32-byte hashes per leaf.
#   2. SALT persists NO trie nodes (overlay returns TrieUpdates::default()); the MPT writes its
#      trie to MDBX every persistence batch.
#   3. SALT's commitment is RAM-resident; the MPT's is disk-backed. That is SALT's design bet, but
#      it means this comparison holds only while SALT fits in RAM.
#
# The result is a TIME SERIES, not a single number: the whole question is where the curves cross as
# state grows. A single sample on a fresh chain says "MPT wins" and on a large one says "SALT wins";
# both are true and neither is the answer.
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"; cd "$REPO"
DUR=${1:-1800}; RATE=${2:-300}
RUN=/tmp/fair-compare; mkdir -p "$RUN"
CSV="$RUN/series.csv"
DASH=${DASH:-http://localhost:8080/state}
declare -A RTS=( [1]=127.0.0.1 [2]=100.85.150.119 [3]=100.70.62.92 [4]=100.86.97.40 )
EVM_WS="ws://127.0.0.1:8546,ws://${RTS[2]}:8646,ws://${RTS[3]}:8746,ws://${RTS[4]}:8846"
PAY_WS="ws://127.0.0.1:19546,ws://${RTS[2]}:19646,ws://${RTS[3]}:19746,ws://${RTS[4]}:19846"

rpc(){ curl -s -m "${3:-60}" -X POST "http://$1" -H 'content-type: application/json' --data "$2" 2>/dev/null; }

preflight(){
  local ok=1
  echo "== preflight =="
  # 1. no foreign load generators (the failure that invalidated three earlier readings)
  local n; n=$(ps -eo args | grep -c '[s]pammer ws' || true)
  if [ "$n" != "0" ]; then
    echo "  [FAIL] $n spammer(s) already running -- a foreign load generator makes this comparison"
    echo "         meaningless. Kill them AT THE PARENT (they respawn):"
    ps -eo pid,ppid,args | grep "[s]pammer ws" | sed 's/^/           /' | cut -c1-150
    echo "         e.g.  ps -eo pid,ppid,args | grep '[s]pammer ws'   then kill -9 the PPIDs"
    ok=0
  else echo "  [ok] no foreign spammers"; fi
  # 2. both lanes live on all machines
  for n in 1 2 3 4; do
    local e p
    e=$(rpc "${RTS[$n]}:$((8545+(n-1)*100))" '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' 20)
    p=$(rpc "${RTS[$n]}:$((19545+(n-1)*100))" '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' 20)
    if [ -n "$e" ] && [ -n "$p" ]; then echo "  [ok] val$n both lanes reachable"
    else echo "  [FAIL] val$n unreachable (evm='${e:0:20}' pay='${p:0:20}')"; ok=0; fi
  done
  # 3. identical gas limit -> identical block capacity -> comparable amortization
  local ge gp
  ge=$(rpc "127.0.0.1:8545" '{"jsonrpc":"2.0","id":1,"method":"eth_getBlockByNumber","params":["latest",false]}' 40 \
       | python3 -c "import sys,json;print(int(json.load(sys.stdin)['result']['gasLimit'],16))" 2>/dev/null)
  gp=$(rpc "127.0.0.1:19545" '{"jsonrpc":"2.0","id":1,"method":"eth_getBlockByNumber","params":["latest",false]}' 40 \
       | python3 -c "import sys,json;print(int(json.load(sys.stdin)['result']['gasLimit'],16))" 2>/dev/null)
  if [ -n "$ge" ] && [ "$ge" = "$gp" ]; then echo "  [ok] gas limits equal ($((ge/1000000))M)"
  else echo "  [WARN] gas limits differ (evm=$ge pay=$gp) -- block fullness will differ, which"
       echo "         changes per-block amortization independently of the commitment"; fi
  echo
  echo "  UNCONTROLLED (flatter SALT, reported not hidden):"
  echo "    - SALT leaf=(nonce,balance) vs MPT leaf=RLP(nonce,balance,storageRoot,codeHash)"
  echo "    - SALT persists no trie nodes; MPT writes its trie to MDBX"
  echo "    - SALT's commitment is RAM-resident (valid only while it fits)"
  [ "$ok" = 1 ] && echo "== preflight PASSED ==" || { echo "== preflight FAILED =="; return 1; }
}

sample(){ # one row: both lanes, plus a symmetry verdict
  python3 - "$DASH" "$CSV" <<'PY'
import json,subprocess,sys,time
dash,csv_path=sys.argv[1],sys.argv[2]
def j(url,tmo=90):
    try: return json.loads(subprocess.run(["curl","-s","-m",str(tmo),url],capture_output=True,text=True,timeout=tmo+10).stdout)
    except Exception: return None
d=j(dash)
if not d: print("  sample: dashboard unreachable"); sys.exit(0)
blk=d['exec'].get('blk_s') or 0
e,p=d['exec']['evm'],d['exec']['pay']
tx_e=(e['tps']/blk) if blk else 0
tx_p=(p['tps']/blk) if blk else 0
# symmetry: identical input rate should give comparable landed tps AND txs/block.
# >25% apart => the lanes are not doing the same amount of work, so root_ms is not comparable.
lo,hi=sorted([max(tx_e,1e-9),max(tx_p,1e-9)])
sym = (hi/lo) <= 1.25
agree = d['evm'].get('agree') and d['pay'].get('agree')
row=[f"{time.time():.0f}",f"{blk:.2f}",
     f"{e['tps']:.1f}",f"{tx_e:.1f}",f"{e['root_ms']:.3f}",f"{e['exec_ms']:.3f}",f"{e['persist_ms']:.2f}",
     f"{p['tps']:.1f}",f"{tx_p:.1f}",f"{p['root_ms']:.3f}",f"{p['exec_ms']:.3f}",f"{p['persist_ms']:.2f}",
     "1" if sym else "0","1" if agree else "0"]
open(csv_path,"a").write(",".join(row)+"\n")
flag="" if sym else "   <-- ASYMMETRIC, excluded"
print(f"  blk/s {blk:4.2f} | EVM tx/blk {tx_e:7.1f} root {e['root_ms']:6.2f} | PAY tx/blk {tx_p:7.1f} root {p['root_ms']:6.2f} | agree={'Y' if agree else 'N'}{flag}")
PY
}

report(){
  python3 - "$CSV" <<'PY'
import csv,sys,statistics as st
rows=[r for r in csv.reader(open(sys.argv[1])) if r and not r[0].startswith("epoch")]
if not rows: print("no samples"); sys.exit(0)
val=[r for r in rows if r[12]=="1"]
print(f"\n=== FAIR COMPARISON: {len(val)}/{len(rows)} samples symmetric (asymmetric ones EXCLUDED) ===")
if not val: print("NO symmetric samples -- the lanes were never under comparable load. Result void."); sys.exit(0)
def med(i): return st.median([float(r[i]) for r in val])
print(f"{'':22}{'EVM (MPT)':>14}{'PAY (SALT)':>14}   verdict")
print(f"{'landed tps':22}{med(2):>14.1f}{med(7):>14.1f}")
print(f"{'txs / block':22}{med(3):>14.1f}{med(8):>14.1f}   <- must match for validity")
print(f"{'exec ms/block':22}{med(5):>14.3f}{med(10):>14.3f}   <- CONTROL")
print(f"{'persist ms/block':22}{med(6):>14.2f}{med(11):>14.2f}   <- CONTROL")
re_,rp=med(4),med(9)
print(f"{'root ms/block':22}{re_:>14.3f}{rp:>14.3f}   <- THE COMMITMENT")
tx=med(3) or 1
print(f"{'root us / tx':22}{re_*1000/tx:>14.1f}{rp*1000/(med(8) or 1):>14.1f}")
ce,cp=med(5),med(10); pe,pp=med(6),med(11)
ctrl_ok = (max(ce,cp)/max(min(ce,cp),1e-9) <= 1.35) and (max(pe,pp)/max(min(pe,pp),1e-9) <= 1.35)
print()
print(f"controls {'AGREE' if ctrl_ok else 'DIVERGE'}: exec {ce:.2f}/{cp:.2f}  persist {pe:.2f}/{pp:.2f}")
if ctrl_ok:
    w = "MPT" if re_<rp else "SALT"
    print(f"=> exec and persist track each other, so the root delta IS the commitment: {w} faster by {max(re_,rp)/max(min(re_,rp),1e-9):.2f}x")
else:
    print("=> controls diverge; the root delta is NOT cleanly attributable to the commitment")
print(f"agreement held on all samples: {all(r[13]=='1' for r in val)}")
print("\nNOTE: a single run is one point on the curve. MPT cost grows with state; SALT's barely does")
print("(2h run: MPT +3.37ms vs SALT +0.61ms). Re-run as state grows to find the crossover.")
PY
}

case "${1:-}" in
  preflight) preflight; exit $? ;;
  report) report; exit 0 ;;
esac

preflight || { echo "!! fix preflight failures first (a foreign spammer silently voids the result)"; exit 1; }
echo "epoch,blk_s,evm_tps,evm_tx_blk,evm_root,evm_exec,evm_persist,pay_tps,pay_tx_blk,pay_root,pay_exec,pay_persist,symmetric,agree" > "$CSV"

echo "== identical load: $RATE tx/s per lane, native transfers, ${DUR}s =="
target/release/spammer ws --targets "$EVM_WS" -r "$RATE" -g 4 -a 1000 -t "$DUR" -l --fresh-recipients --mix transfer=100 >"$RUN/spam-evm.log" 2>&1 &
E=$!
target/release/spammer ws --targets "$PAY_WS" -r "$RATE" -g 4 -a 1000 -t "$DUR" -l --fresh-recipients --mix transfer=100 >"$RUN/spam-pay.log" 2>&1 &
P=$!
trap 'kill -9 $E $P 2>/dev/null' EXIT INT TERM

echo "== warmup 120s (pools fill, rates settle) =="; sleep 120
END=$(( $(date +%s) + DUR - 150 ))
while [ "$(date +%s)" -lt "$END" ]; do sample; sleep 30; done
kill -9 $E $P 2>/dev/null
report
echo "series: $CSV"
