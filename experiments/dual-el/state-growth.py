#!/usr/bin/env python3
"""State-growth demo: payment lane state stays ~flat while the EVM lane balloons.

The product point in one graph: a chain gets slow and pricey as its STATE grows. Payments between
existing users add ~zero state; general (contract) activity grows state fast. So an isolated payment
lane stays cheap forever.

Measures the REAL state size via reth's `reth_db_table_size{table=...}` prometheus gauge (live, no
DB lock) -- the sum of the account/storage/trie/bytecode tables, i.e. exactly what a pruned snapshot
ships. (`du` on the datadir is useless here: MDBX pre-allocates the file, so it reads a flat ~4 GB.)

Drives contrasting load, samples both lanes, writes a CSV and a self-contained SVG chart.

  ./state-growth.py --duration 900 \
      --evm-ws ws://127.0.0.1:8546  --evm-metrics http://127.0.0.1:9001 \
      --pay-ws ws://127.0.0.1:19546 --pay-metrics http://127.0.0.1:19001 \
      --out /tmp/state-growth

Run it on a FRESH dual-lane testnet (state starting near zero) for the cleanest before/after.
"""
import argparse, subprocess, time, urllib.request, sys, os

# Tables that make up on-disk STATE (what a pruned snapshot ships). History/tx tables excluded.
STATE_TABLES = {
    "HashedAccounts", "HashedStorages", "AccountsTrie", "StoragesTrie",
    "PlainAccountState", "PlainStorageState", "Bytecodes",
}

def scrape_state(metrics_url, tries=4):
    """Return (state_bytes, accounts_bytes, storage_bytes). Retries: reth's RPC/metrics stall under
    load, and a missed scrape must not read as 'state shrank'."""
    for _ in range(tries):
        try:
            raw = urllib.request.urlopen(metrics_url + "/metrics", timeout=8).read().decode()
        except Exception:
            time.sleep(1); continue
        tot = acct = stor = 0.0
        seen = False
        for ln in raw.splitlines():
            if ln.startswith('reth_db_table_size{table="'):
                seen = True
                t = ln.split('table="', 1)[1].split('"', 1)[0]
                try: v = float(ln.rsplit(" ", 1)[1])
                except ValueError: continue
                if t in STATE_TABLES: tot += v
                if t == "HashedAccounts": acct = v
                if t == "HashedStorages": stor = v
        if seen:
            return tot, acct, stor
        time.sleep(1)
    return None, None, None

def head(ws_or_http):
    url = ws_or_http.replace("ws://", "http://").rsplit(":", 1)
    url = url[0] + ":" + str(int(url[1]) - 1)  # ws port -> http port
    try:
        req = urllib.request.Request(url, data=b'{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}',
                                     headers={"content-type": "application/json"})
        import json
        return int(json.load(urllib.request.urlopen(req, timeout=6))["result"], 16)
    except Exception:
        return None

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--duration", type=int, default=900)
    ap.add_argument("--interval", type=int, default=10)
    ap.add_argument("--evm-ws", default="ws://127.0.0.1:8546")
    ap.add_argument("--pay-ws", default="ws://127.0.0.1:19546")
    ap.add_argument("--evm-metrics", default="http://127.0.0.1:9001")
    ap.add_argument("--pay-metrics", default="http://127.0.0.1:19001")
    ap.add_argument("--evm-rate", type=int, default=60)
    ap.add_argument("--pay-rate", type=int, default=400)
    ap.add_argument("--spammer", default="target/release/spammer")
    ap.add_argument("--out", default="/tmp/state-growth")
    ap.add_argument("--no-load", action="store_true", help="don't launch spammers (drive load yourself)")
    a = ap.parse_args()
    os.makedirs(a.out, exist_ok=True)

    procs = []
    if not a.no_load:
        # EVM lane: RANDOM, state-heavy load -- storage writes + new accounts + contract calls, the
        # activity that balloons a general chain's state.
        procs.append(subprocess.Popen(
            [a.spammer, "ws", "--targets", a.evm_ws, "-r", str(a.evm_rate), "-t", str(a.duration + 30),
             "-g", "4", "-a", "1000", "-l", "--fresh-recipients",
             "--mix", "guzzler=70,transfer=30", "--guzzler-fn-weights", "storage-write=100@600"],
            stdout=open(a.out + "/spam_evm.log", "w"), stderr=subprocess.STDOUT))
        # Payment lane: transfers among EXISTING (prefunded) accounts -> adds ~zero state.
        procs.append(subprocess.Popen(
            [a.spammer, "ws", "--targets", a.pay_ws, "-r", str(a.pay_rate), "-t", str(a.duration + 30),
             "-g", "8", "-a", "1000", "-l", "--mix", "transfer=100"],
            stdout=open(a.out + "/spam_pay.log", "w"), stderr=subprocess.STDOUT))
        print(f"load: EVM {a.evm_rate}/s (storage-write+new-accts, random) | PAY {a.pay_rate}/s (transfers, existing accts)")

    # baseline so the graph shows GROWTH from the moment load starts
    es0, _, _ = scrape_state(a.evm_metrics); ps0, _, _ = scrape_state(a.pay_metrics)
    es0 = es0 or 0; ps0 = ps0 or 0
    csv_path = a.out + "/state-growth.csv"
    csv = open(csv_path, "w")
    csv.write("t,evm_state_mb,pay_state_mb,evm_head,pay_head,evm_acct_mb,evm_stor_mb,pay_acct_mb,pay_stor_mb\n")
    rows = []
    t0 = time.time()
    print(f"{'t':>5} {'EVM state':>12} {'PAY state':>12}   (MB, growth from start)")
    while time.time() - t0 < a.duration:
        es, ea, est = scrape_state(a.evm_metrics)
        ps, pa, pst = scrape_state(a.pay_metrics)
        if es is None or ps is None:
            time.sleep(a.interval); continue
        t = round(time.time() - t0, 1)
        e_mb = (es - es0) / 1e6; p_mb = (ps - ps0) / 1e6
        eh, ph = head(a.evm_ws), head(a.pay_ws)
        csv.write(f"{t},{e_mb:.3f},{p_mb:.3f},{eh},{ph},{ea/1e6:.3f},{est/1e6:.3f},{pa/1e6:.3f},{pst/1e6:.3f}\n"); csv.flush()
        rows.append((t, e_mb, p_mb))
        print(f"{int(t):>5} {e_mb:>10.2f}MB {p_mb:>10.2f}MB")
        time.sleep(a.interval)

    for p in procs:
        p.terminate()
    try:
        subprocess.run(["pkill", "-x", "spammer"], timeout=5)
    except Exception:
        pass
    csv.close()
    svg = plot_svg(rows)
    open(a.out + "/state-growth.svg", "w").write(svg)
    print(f"\nCSV:  {csv_path}\nSVG:  {a.out}/state-growth.svg")
    if rows:
        e_final, p_final = rows[-1][1], rows[-1][2]
        ratio = (e_final / p_final) if p_final > 0.05 else float("inf")
        print(f"\nFinal: EVM state +{e_final:.1f} MB   PAY state +{p_final:.2f} MB"
              + (f"   ->  EVM grew {ratio:.0f}x more" if ratio != float("inf") else "   ->  payment state essentially flat"))

def plot_svg(rows):
    """Self-contained two-line SVG. EVM (steep) vs Payment (flat). Presentation-ready."""
    W, H = 860, 460
    ml, mr, mt, mb = 78, 190, 46, 60
    pw, ph = W - ml - mr, H - mt - mb
    if not rows:
        return f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}"><text x="20" y="40">no data</text></svg>'
    tmax = max(r[0] for r in rows) or 1
    ymax = max(max(r[1] for r in rows), max(r[2] for r in rows), 1) * 1.1
    def X(t): return ml + pw * (t / tmax)
    def Y(v): return mt + ph * (1 - v / ymax)
    def path(idx):
        return "M " + " L ".join(f"{X(r[0]):.1f} {Y(r[idx]):.1f}" for r in rows)
    # gridlines
    grid = ""
    for i in range(6):
        yv = ymax * i / 5; yy = Y(yv)
        grid += f'<line x1="{ml}" y1="{yy:.1f}" x2="{ml+pw}" y2="{yy:.1f}" stroke="#e6e9ee"/>'
        grid += f'<text x="{ml-10}" y="{yy+4:.1f}" text-anchor="end" font-size="12" fill="#7d8794">{yv:.0f}</text>'
    for i in range(5):
        tv = tmax * i / 4; xx = X(tv)
        grid += f'<text x="{xx:.1f}" y="{mt+ph+22}" text-anchor="middle" font-size="12" fill="#7d8794">{tv/60:.0f}m</text>'
    e_final, p_final = rows[-1][1], rows[-1][2]
    evm_c, pay_c = "#e5484d", "#2f9e5f"
    return f'''<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" font-family="ui-sans-serif,system-ui,-apple-system,Segoe UI,Roboto,sans-serif">
<rect width="{W}" height="{H}" fill="#ffffff"/>
<text x="{ml}" y="26" font-size="19" font-weight="700" fill="#1a2029">State grows with activity — not with payments</text>
<text x="{ml}" y="{mt+ph+46}" font-size="12" fill="#7d8794">time</text>
<text x="24" y="{mt+ph/2}" font-size="12" fill="#7d8794" transform="rotate(-90 24 {mt+ph/2})" text-anchor="middle">state added (MB)</text>
{grid}
<path d="{path(1)}" fill="none" stroke="{evm_c}" stroke-width="3"/>
<path d="{path(2)}" fill="none" stroke="{pay_c}" stroke-width="3"/>
<rect x="{ml+pw+18}" y="{mt+8}" width="14" height="14" fill="{evm_c}" rx="3"/>
<text x="{ml+pw+38}" y="{mt+20}" font-size="14" font-weight="600" fill="#1a2029">EVM lane</text>
<text x="{ml+pw+38}" y="{mt+38}" font-size="12" fill="#7d8794">contracts + storage +</text>
<text x="{ml+pw+38}" y="{mt+54}" font-size="12" fill="#7d8794">new accounts (random)</text>
<text x="{ml+pw+38}" y="{mt+74}" font-size="15" font-weight="700" fill="{evm_c}">+{e_final:.0f} MB</text>
<rect x="{ml+pw+18}" y="{mt+108}" width="14" height="14" fill="{pay_c}" rx="3"/>
<text x="{ml+pw+38}" y="{mt+120}" font-size="14" font-weight="600" fill="#1a2029">Payment lane</text>
<text x="{ml+pw+38}" y="{mt+138}" font-size="12" fill="#7d8794">transfers between</text>
<text x="{ml+pw+38}" y="{mt+154}" font-size="12" fill="#7d8794">existing users</text>
<text x="{ml+pw+38}" y="{mt+174}" font-size="15" font-weight="700" fill="{pay_c}">+{p_final:.1f} MB</text>
</svg>'''

if __name__ == "__main__":
    main()
