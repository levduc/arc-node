#!/usr/bin/env bash
# Clean A/B of the native-transfer fast path: TWO SEPARATE full runs on identical config+load.
#
# Why two runs and not one mixed chain: recreating a payment EL mid-run triggers finding #7 (the
# fresh EL loses unpersisted blocks, the CL tip is ahead, value-sync can't backfill) and the
# validator stalls. The fast path is state-identical so a mixed config is SAFE for consensus — it
# is purely an ops/resync problem. Two clean runs sidestep it entirely.
#
#   ./ab-fastpath.sh            # ~35 min, writes /tmp/ab-fastpath-results.txt
#   WINDOW=120 LOAD=8 ./ab-fastpath.sh
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"; cd "$REPO"
export PATH="$HOME/.foundry/bin:$HOME/.cargo/bin:$PATH"
OUT=/tmp/ab-fastpath-results.txt
WINDOW=${WINDOW:-120}
LOAD=${LOAD:-8}
: > "$OUT"

run_one(){ # run_one <label> <env-args>
  local label="$1" envargs="$2"
  echo "########## $label ##########" | tee -a "$OUT"

  pkill -x spammer 2>/dev/null; sleep 2
  bash experiments/dual-el/demo-metamask.sh stop >/dev/null 2>&1 || true
  sleep 5

  PAY_GAS=1000000000 EXTRA_ACCOUNTS=8000 PAY_EL_ENV="$envargs" \
    bash experiments/dual-el/demo-metamask.sh start >/tmp/ab_start_$label.log 2>&1
  if ! grep -q "two-lane demo up" /tmp/ab_start_$label.log; then
    echo "  !! start failed (see /tmp/ab_start_$label.log)" | tee -a "$OUT"; return 1
  fi

  # confirm the env actually reached every payment EL
  local n_env=0
  for i in 1 2 3 4; do
    docker inspect validator${i}_el_pay --format '{{json .Config.Env}}' 2>/dev/null \
      | grep -q ARC_PARALLEL_TRANSFERS=1 && n_env=$((n_env+1))
  done
  echo "  payment ELs with ARC_PARALLEL_TRANSFERS=1: $n_env/4" | tee -a "$OUT"

  # identical load for both runs
  for k in $(seq 0 $((LOAD-1))); do
    nohup target/release/spammer ws --targets ws://127.0.0.1:19546 --chain-id 1338 \
      -r 12000 -t $((WINDOW+180)) -g 20 -a 1000 --account-offset $((k*1000)) --mix transfer=100 \
      >/tmp/ab_spam_${label}_$k.log 2>&1 &
  done
  sleep 45   # let the mempool fill and cadence settle

  WINDOW=$WINDOW bash experiments/dual-el/pay-throughput-bench.sh measure 2>/dev/null \
    | sed -n '/PAYMENT LANE @/,/====/p' | tee -a "$OUT"

  # consensus health: every validator must agree on the payment-lane root
  python3 - <<'PY' | tee -a "$OUT"
import json,urllib.request
def rpc(p,m,q):
    r=urllib.request.Request(f"http://127.0.0.1:{p}",data=json.dumps({"jsonrpc":"2.0","id":1,"method":m,"params":q}).encode(),headers={"content-type":"application/json"})
    return json.load(urllib.request.urlopen(r,timeout=8))["result"]
try:
    heads={v:int(rpc(19545+(v-1)*100,"eth_blockNumber",[]),16) for v in (1,2,3,4)}
    h=min(heads.values())-3
    roots={v:rpc(19545+(v-1)*100,"eth_getBlockByNumber",[hex(h),False])["stateRoot"] for v in (1,2,3,4)}
    ok=len(set(roots.values()))==1
    print(f"  heads {heads}")
    print(f"  root agreement @{h}: {'ALL 4 AGREE OK' if ok else 'DIVERGED'}  {list(roots.values())[0][:20]}")
except Exception as e:
    print(f"  agreement check failed: {e}")
PY
  pkill -x spammer 2>/dev/null
  echo "" | tee -a "$OUT"
}

run_one stock ""
run_one fastpath "-e ARC_PARALLEL_TRANSFERS=1"

echo "########## COMPARISON ##########" | tee -a "$OUT"
python3 - "$OUT" <<'PY' | tee -a "$OUT"
import re,sys
t=open(sys.argv[1]).read()
def grab(sec):
    m=re.search(rf"#+ {sec} #+(.*?)(?:#####|\Z)", t, re.S)
    if not m: return None
    b=m.group(1)
    g=lambda p: (re.search(p,b).group(1) if re.search(p,b) else "?")
    return dict(exec_=g(r"exec / block\s+([\d.]+)"), root=g(r"state-root / block\s+([\d.]+)"),
                tpb=g(r"txs / block\s+([\d,]+)"), tps=g(r"throughput \(avg\)\s+([\d,]+)"),
                agree="ALL 4 AGREE OK" in b)
a,b=grab("stock"),grab("fastpath")
if a and b:
    ea,eb=float(a["exec_"]),float(b["exec_"])
    na,nb=float(a["tpb"].replace(",","")),float(b["tpb"].replace(",",""))
    print(f"  {'':10} {'exec/blk':>10} {'txs/blk':>9} {'us/tx':>8} {'tps':>8}  agree")
    for n,d in (("stock",a),("fastpath",b)):
        e=float(d['exec_']); nn=float(d['tpb'].replace(',',''))
        print(f"  {n:10} {e:>9.1f}ms {nn:>9.0f} {e*1000/max(nn,1):>8.2f} {d['tps']:>8}  {'yes' if d['agree'] else 'NO'}")
    if na>0 and nb>0:
        print(f"\n  per-tx exec: {ea*1000/na:.2f} us -> {eb*1000/nb:.2f} us  = {(ea/na)/(eb/nb):.2f}x faster")
PY
echo "results: $OUT"
