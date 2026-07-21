#!/usr/bin/env python3
"""Congestion demo: under load the EVM lane fills up, its fee ratchets up, and txs back up in the
mempool -- while the payment lane stays uncongested, so its fee stays pinned at the floor and every
tx clears next block.

The product point in one graph: shared general-purpose blockspace gets scarce (fees spike, txs
queue) exactly when you least want it; an isolated payment lane sized for payments has spare capacity,
so payments keep landing at a flat, cheap fee.

This is REAL EIP-1559 behaviour, not a mock: baseFee rises 12.5%/block while a block is >50% full
and decays while it's under -- so a saturated EVM lane compounds upward and an idle payment lane sits
at the floor. We read baseFee from the block header and the backlog from `txpool_status`.

Drives contrasting load (unless --no-load), samples both lanes, writes a CSV and two self-contained
SVG charts (fee-over-time and backlog-over-time).

  ./congestion.py --duration 300 \
      --evm-ws ws://127.0.0.1:8546  --pay-ws ws://127.0.0.1:19546 \
      --out /tmp/congestion

Run it on a dual-lane testnet where the EVM block gas limit (30M) is much smaller than the payment
lane's (200M): the whole point is that the same offered load saturates one and not the other.
"""
import argparse, subprocess, time, urllib.request, json, sys, os

def rpc(http_url, method, params=None):
    req = urllib.request.Request(
        http_url,
        data=json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params or []}).encode(),
        headers={"content-type": "application/json"})
    for _ in range(3):
        try:
            return json.load(urllib.request.urlopen(req, timeout=8))["result"]
        except Exception:
            time.sleep(0.5)
    return None

def http_of(ws_url):
    u = ws_url.replace("ws://", "http://").rsplit(":", 1)
    return u[0] + ":" + str(int(u[1]) - 1)  # ws port -> http port

def sample(http_url):
    """Return (fullness_pct, basefee_wei, pending, queued) for the latest block, or None."""
    blk = rpc(http_url, "eth_getBlockByNumber", ["latest", False])
    if not blk:
        return None
    gu = int(blk["gasUsed"], 16); gl = int(blk["gasLimit"], 16)
    bf = int(blk.get("baseFeePerGas", "0x0"), 16)
    pool = rpc(http_url, "txpool_status") or {}
    try:
        pend = int(pool["pending"], 16); q = int(pool["queued"], 16)
    except Exception:
        pend = q = 0
    return (100.0 * gu / gl if gl else 0.0, bf, pend, q)

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--duration", type=int, default=300)
    ap.add_argument("--interval", type=int, default=5)
    ap.add_argument("--evm-ws", default="ws://127.0.0.1:8546")
    ap.add_argument("--pay-ws", default="ws://127.0.0.1:19546")
    ap.add_argument("--evm-chainid", default="1337")
    ap.add_argument("--pay-chainid", default="1338")
    ap.add_argument("--evm-rate", type=int, default=200, help="offered EVM load (tx/s) -- oversubscribe the 30M lane")
    ap.add_argument("--pay-rate", type=int, default=300)
    ap.add_argument("--spammer", default="target/release/spammer")
    ap.add_argument("--out", default="/tmp/congestion")
    ap.add_argument("--no-load", action="store_true", help="don't launch spammers (drive load yourself)")
    a = ap.parse_args()
    os.makedirs(a.out, exist_ok=True)
    evm_http, pay_http = http_of(a.evm_ws), http_of(a.pay_ws)

    procs = []
    if not a.no_load:
        # EVM lane: heavy guzzlers (~1.6M gas each) at a rate that oversubscribes the 30M block ->
        # blocks pin ~full, baseFee ratchets up, mempool backs up.
        procs.append(subprocess.Popen(
            [a.spammer, "ws", "--targets", a.evm_ws, "--chain-id", a.evm_chainid,
             "-r", str(a.evm_rate), "-t", str(a.duration + 30), "-g", "4", "-a", "48", "-l",
             "--fresh-recipients", "--mix", "guzzler=90,transfer=10",
             "--guzzler-fn-weights", "storage-write=100@80"],
            stdout=open(a.out + "/spam_evm.log", "w"), stderr=subprocess.STDOUT))
        # Payment lane: cheap transfers among existing accounts -> lane stays near-empty, fee at floor.
        procs.append(subprocess.Popen(
            [a.spammer, "ws", "--targets", a.pay_ws, "--chain-id", a.pay_chainid,
             "-r", str(a.pay_rate), "-t", str(a.duration + 30), "-g", "8", "-a", "1000", "-l",
             "--mix", "transfer=100"],
            stdout=open(a.out + "/spam_pay.log", "w"), stderr=subprocess.STDOUT))
        print(f"load: EVM {a.evm_rate}/s heavy guzzlers (oversubscribe 30M) | PAY {a.pay_rate}/s transfers")

    csv_path = a.out + "/congestion.csv"
    csv = open(csv_path, "w")
    csv.write("t,evm_full,evm_basefee,evm_pending,pay_full,pay_basefee,pay_pending\n")
    rows = []
    t0 = time.time()
    print(f"{'t':>5} | {'EVM full':>8} {'EVM fee':>10} {'EVM queue':>9} | {'PAY full':>8} {'PAY fee':>10} {'PAY queue':>9}")
    while time.time() - t0 < a.duration:
        e = sample(evm_http); p = sample(pay_http)
        if e is None or p is None:
            time.sleep(a.interval); continue
        t = round(time.time() - t0, 1)
        csv.write(f"{t},{e[0]:.1f},{e[1]},{e[2]},{p[0]:.1f},{p[1]},{p[2]}\n"); csv.flush()
        rows.append((t, e[0], e[1], e[2], p[0], p[1], p[2]))
        print(f"{int(t):>5} | {e[0]:>7.1f}% {e[1]:>10} {e[2]:>9} | {p[0]:>7.1f}% {p[1]:>10} {p[2]:>9}")
        time.sleep(a.interval)

    for pr in procs:
        pr.terminate()
    try:
        subprocess.run(["pkill", "-x", "spammer"], timeout=5)
    except Exception:
        pass
    csv.close()
    open(a.out + "/congestion-fee.svg", "w").write(plot(rows, 2, 5, "Fee ratchets up on a congested lane — flat on the payment lane",
                                                        "base fee (wei)", "EVM lane\ncongested", "Payment lane\nspare capacity"))
    open(a.out + "/congestion-backlog.svg", "w").write(plot(rows, 3, 6, "Transactions back up on a congested lane — clear on the payment lane",
                                                            "mempool backlog (txs)", "EVM lane\ncongested", "Payment lane\nspare capacity"))
    print(f"\nCSV:  {csv_path}\nSVG:  {a.out}/congestion-fee.svg , {a.out}/congestion-backlog.svg")
    if rows:
        print(f"\nEVM peak: {max(r[1] for r in rows):.0f}% full, fee {max(r[2] for r in rows)} wei, backlog {max(r[3] for r in rows)} txs")
        print(f"PAY:      {max(r[4] for r in rows):.1f}% full, fee {max(r[5] for r in rows)} wei, backlog {max(r[6] for r in rows)} txs")

def plot(rows, evm_idx, pay_idx, title, ylabel, evm_lbl, pay_lbl):
    """Self-contained two-line SVG: EVM (rising) vs payment (flat)."""
    W, H = 860, 460
    ml, mr, mt, mb = 84, 190, 46, 60
    pw, ph = W - ml - mr, H - mt - mb
    if not rows:
        return f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}"><text x="20" y="40">no data</text></svg>'
    tmax = max(r[0] for r in rows) or 1
    ymax = max(max(r[evm_idx] for r in rows), max(r[pay_idx] for r in rows), 1) * 1.1
    def X(t): return ml + pw * (t / tmax)
    def Y(v): return mt + ph * (1 - v / ymax)
    def path(idx): return "M " + " L ".join(f"{X(r[0]):.1f} {Y(r[idx]):.1f}" for r in rows)
    grid = ""
    for i in range(6):
        yv = ymax * i / 5; yy = Y(yv)
        grid += f'<line x1="{ml}" y1="{yy:.1f}" x2="{ml+pw}" y2="{yy:.1f}" stroke="#e6e9ee"/>'
        grid += f'<text x="{ml-10}" y="{yy+4:.1f}" text-anchor="end" font-size="12" fill="#7d8794">{yv:,.0f}</text>'
    for i in range(5):
        tv = tmax * i / 4; xx = X(tv)
        grid += f'<text x="{xx:.1f}" y="{mt+ph+22}" text-anchor="middle" font-size="12" fill="#7d8794">{tv/60:.0f}m</text>'
    evm_c, pay_c = "#e5484d", "#2f9e5f"
    e_final, p_final = rows[-1][evm_idx], rows[-1][pay_idx]
    def lines(lbl, x, y):
        return "".join(f'<text x="{x}" y="{y+i*16}" font-size="12" fill="#7d8794">{ln}</text>' for i, ln in enumerate(lbl.split("\n")))
    return f'''<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" font-family="ui-sans-serif,system-ui,-apple-system,Segoe UI,Roboto,sans-serif">
<rect width="{W}" height="{H}" fill="#ffffff"/>
<text x="{ml}" y="26" font-size="18" font-weight="700" fill="#1a2029">{title}</text>
<text x="{ml}" y="{mt+ph+46}" font-size="12" fill="#7d8794">time</text>
<text x="24" y="{mt+ph/2}" font-size="12" fill="#7d8794" transform="rotate(-90 24 {mt+ph/2})" text-anchor="middle">{ylabel}</text>
{grid}
<path d="{path(evm_idx)}" fill="none" stroke="{evm_c}" stroke-width="3"/>
<path d="{path(pay_idx)}" fill="none" stroke="{pay_c}" stroke-width="3"/>
<rect x="{ml+pw+18}" y="{mt+8}" width="14" height="14" fill="{evm_c}" rx="3"/>
<text x="{ml+pw+38}" y="{mt+20}" font-size="14" font-weight="600" fill="#1a2029">{evm_lbl.split(chr(10))[0]}</text>
{lines(chr(10).join(evm_lbl.split(chr(10))[1:]), ml+pw+38, mt+38)}
<text x="{ml+pw+38}" y="{mt+72}" font-size="15" font-weight="700" fill="{evm_c}">{e_final:,.0f}</text>
<rect x="{ml+pw+18}" y="{mt+108}" width="14" height="14" fill="{pay_c}" rx="3"/>
<text x="{ml+pw+38}" y="{mt+120}" font-size="14" font-weight="600" fill="#1a2029">{pay_lbl.split(chr(10))[0]}</text>
{lines(chr(10).join(pay_lbl.split(chr(10))[1:]), ml+pw+38, mt+138)}
<text x="{ml+pw+38}" y="{mt+172}" font-size="15" font-weight="700" fill="{pay_c}">{p_final:,.0f}</text>
</svg>'''

if __name__ == "__main__":
    main()
