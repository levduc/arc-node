#!/usr/bin/env bash
# Does the payment EL's memory grow without bound under sustained load?
#
# WHY: during a 15-min-per-size frontier run, val4's payment EL (papaduck-alien2, 15 GB RAM, an
# 11 GB container cap -- the only capped node) was OOM-killed after ~1.5 h of continuous load. It
# had survived 15-minute windows at 25M, 50M and 100M first, and a FRESH process at the same size
# ran clean for 13 min, so the trigger was cumulative, not the block size. That leaves an open
# question worth more than a few hundred tps: does reth's memory plateau, or climb until something
# dies?
#
# This samples every payment EL's container memory once a minute under continuous load at the
# recommended 100M operating point, alongside block height, so growth can be read per 1000 blocks
# as well as per minute.
#
#   ./mem-soak.sh                 # 45 min
#   MINUTES=90 ./mem-soak.sh
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"; cd "$REPO"
export PATH="$HOME/.foundry/bin:$HOME/.cargo/bin:$PATH"
MINUTES=${MINUTES:-45}
OUT=${OUT:-/tmp/mem-soak.txt}
: > "$OUT"

MINUTES=$MINUTES OUT=$OUT python3 - <<'PY' | tee -a "$OUT"
import json,os,subprocess,statistics,time,urllib.request
MIN=int(os.environ["MINUTES"])
HOSTS={1:("local","127.0.0.1"),2:("ginnythui","100.85.150.119"),
       3:("papaduck","100.70.62.92"),4:("papaduck-alien2","100.86.97.40")}
def mem(v):
    host,_=HOSTS[v]; name=f"validator{v}_el_pay"
    cmd=["docker","stats","--no-stream","--format","{{.MemUsage}}",name]
    if host!="local":
        cmd=["timeout","-k","5","45","tailscale","ssh",host,
             f"docker stats --no-stream --format '{{{{.MemUsage}}}}' {name}"]
    try:
        out=subprocess.run(cmd,capture_output=True,text=True,timeout=60).stdout.strip()
        raw=out.split("/")[0].strip()          # e.g. "3.412GiB"
        num=float("".join(c for c in raw if c.isdigit() or c=="."))
        u=raw.lstrip("0123456789.").strip().lower()   # the unit is a SUFFIX -- lstrip the number
        return num*{"gib":1024,"mib":1,"kib":1/1024,"b":1/1048576}.get(u,1)   # -> MiB
    except Exception:
        return float("nan")
def head(v):
    _,ip=HOSTS[v]
    try:
        r=urllib.request.Request(f"http://{ip}:{19545+(v-1)*100}",
            data=json.dumps({"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}).encode(),
            headers={"content-type":"application/json"})
        return int(json.load(urllib.request.urlopen(r,timeout=12))["result"],16)
    except Exception: return -1

print(f"payment-EL memory under sustained load — {MIN} min, sampled every 60 s")
print(f"{'min':>4} {'height':>8} " + " ".join(f"{HOSTS[v][0][:9]:>10}" for v in HOSTS))
series={v:[] for v in HOSTS}; heights=[]
t0=time.time()
while (time.time()-t0)/60 < MIN:
    h=head(1); ms={v:mem(v) for v in HOSTS}
    for v in HOSTS: series[v].append(ms[v])
    heights.append(h)
    print(f"{(time.time()-t0)/60:>4.0f} {h:>8} " + " ".join(f"{ms[v]:>9.0f}M" for v in HOSTS), flush=True)
    time.sleep(60)

print("\n=== growth ===")
dblk=(heights[-1]-heights[0]) if heights[-1]>0 and heights[0]>0 else 0
for v in HOSTS:
    s=[x for x in series[v] if x==x]
    if len(s)<3: print(f"  {HOSTS[v][0]:<16} no data"); continue
    first,last=s[0],s[-1]; mins=len(s)-1
    rate=(last-first)/max(mins,1)
    # is it still climbing at the end, or flat? compare last third to middle third
    n=len(s)//3
    tail=statistics.mean(s[-n:]) - statistics.mean(s[n:2*n]) if n>=2 else float('nan')
    per1k=(last-first)/dblk*1000 if dblk>0 else float('nan')
    verdict = "FLAT" if abs(tail) < 50 else ("STILL CLIMBING" if tail>0 else "receding")
    print(f"  {HOSTS[v][0]:<16} {first:>7.0f}M -> {last:>7.0f}M   "
          f"{rate:+6.1f} MiB/min   {per1k:+7.1f} MiB/1000 blk   last-third {tail:+6.0f}M  {verdict}")
print(f"\n  {dblk} blocks over {MIN} min")
print("  val4 (papaduck-alien2) is the only capped node: 11 GiB = 11264 MiB")
PY
echo "results: $OUT"
