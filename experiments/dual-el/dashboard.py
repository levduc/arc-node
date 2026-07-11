#!/usr/bin/env python3
"""Live dual-EL payment-lane dashboard. Polls the running soak4 testnet server-side (no CORS/CSP),
serves a browser view of: both lanes advancing, the two roots per block, per-lane agreement across
all 4 validators. Open http://localhost:8080 ."""
import json, os, subprocess, threading, time, collections, urllib.request, socketserver, http.server
from concurrent.futures import ThreadPoolExecutor

PORT = 8080
CAST = os.path.expanduser("~/.foundry/bin/cast")
# representative validator datadirs (all validators hold ~identical state)
DDIR = "/home/papaduck/arc-node-paymentlane/.quake/soak4/validator2"
_series = collections.deque(maxlen=240)  # (elapsed_s, evm_mb, pay_mb)
_t0 = time.time()

def _du_kb(path):
    try:
        out = subprocess.run(["du", "-sk", path], capture_output=True, text=True, timeout=8)
        return int(out.stdout.split()[0])
    except Exception:
        return 0

def _wal_kb(rocksdb):
    # RocksDB write-ahead logs (*.log): a transient write buffer that accumulates then flushes into
    # SSTs and is deleted on compaction. Counting it makes the history number jump up/down, so we
    # exclude it and report the PERSISTENT (compacted) history, which grows monotonically.
    import glob
    total = 0
    for f in glob.glob(rocksdb + "/*.log"):
        try:
            total += os.path.getsize(f) // 1024
        except Exception:
            pass
    return total

def _lane_sizes(base):
    # state = db/ (MDBX: accounts/storage/trie — what a pruned snapshot ships)
    # history = static_files/ (headers, receipts, changesets) + rocksdb/ (tx-hash + history index),
    #           minus RocksDB WAL churn. Snapshot EXCLUDES all of this.
    state = _du_kb(base + "/db")
    rdb = _du_kb(base + "/rocksdb")
    hist = _du_kb(base + "/static_files") + rdb - _wal_kb(base + "/rocksdb")
    return state, hist

def _growth_loop():
    while True:
        es, eh = _lane_sizes(DDIR + "/reth")
        ps, ph = _lane_sizes(DDIR + "/reth-pay")
        _series.append((round(time.time() - _t0, 1), es, eh, ps, ph))
        time.sleep(5)

# ---- execution-cost metrics: state-root latency (reth prometheus) + disk I/O (cgroup) per lane ----
_exec = {"evm": {}, "pay": {}}
_exec_series = collections.deque(maxlen=240)  # (t, evm_root_ms, pay_root_ms, evm_rd_mbs, pay_rd_mbs)
_mprev = {"evm": {}, "pay": {}}

def _docker_out(args):
    try:
        return subprocess.run(["docker"] + args, capture_output=True, text=True, timeout=6).stdout.strip()
    except Exception:
        return ""

def _discover_metrics():
    """Host port of each lane's reth metrics endpoint + cgroup path, for validator2."""
    cfg = {}
    for lane, cname in (("evm", "validator2_el"), ("pay", "validator2_el_pay")):
        port = None
        out = _docker_out(["port", cname, "9001/tcp"])  # e.g. 0.0.0.0:9101
        for tok in out.split():
            if ":" in tok:
                port = tok.rsplit(":", 1)[-1]
        cid = _docker_out(["inspect", "-f", "{{.Id}}", cname])
        cfg[lane] = {"port": port, "cid": cid}
    return cfg

def _prom_state_root_ms(text, prev):
    """Rolling state-root latency from a histogram pair <name>_sum/<name>_count whose name
    mentions state_root+duration (fallback: root+duration). Returns (ms, new_prev)."""
    sums, counts = {}, {}
    for ln in text.splitlines():
        if ln.startswith("#") or " " not in ln:
            continue
        name, val = ln.split(" ", 1)
        base = name.split("{")[0]
        low = base.lower()
        if ("root" not in low) or ("duration" not in low and "seconds" not in low and "histogram" not in low):
            continue
        try:
            v = float(val.strip())
        except ValueError:
            continue
        if base.endswith("_sum"):
            sums[base[:-4]] = sums.get(base[:-4], 0.0) + v
        elif base.endswith("_count"):
            counts[base[:-6]] = counts.get(base[:-6], 0.0) + v
    cands = [k for k in sums if k in counts]
    if not cands:
        return None, prev
    pick = sorted(cands, key=lambda k: (("state_root" not in k.lower()), len(k)))[0]
    s, c = sums[pick], counts[pick]
    ps, pc = prev.get("sum", 0.0), prev.get("count", 0.0)
    new_prev = {"sum": s, "count": c, "pick": pick}
    if c > pc:
        return round((s - ps) / (c - pc) * 1000.0, 2), new_prev
    return prev.get("last_ms"), new_prev

def _cgroup_io(cid):
    """(read_bytes, write_bytes) totals from cgroup v2 io.stat for a container id."""
    for p in (f"/sys/fs/cgroup/system.slice/docker-{cid}.scope/io.stat",
              f"/sys/fs/cgroup/docker/{cid}/io.stat"):
        try:
            rb = wb = 0
            with open(p) as f:
                for ln in f:
                    for tok in ln.split():
                        if tok.startswith("rbytes="):
                            rb += int(tok[7:])
                        elif tok.startswith("wbytes="):
                            wb += int(tok[7:])
            return rb, wb
        except Exception:
            continue
    return None, None

def _exec_loop():
    cfg = {}
    while True:
        try:
            if not cfg or any(not cfg[l]["port"] for l in cfg):
                cfg = _discover_metrics()
            now = round(time.time() - _t0, 1)
            for lane in ("evm", "pay"):
                port, cid = cfg.get(lane, {}).get("port"), cfg.get(lane, {}).get("cid")
                st = _exec[lane]
                if port:
                    try:
                        with urllib.request.urlopen(f"http://127.0.0.1:{port}/metrics", timeout=3) as r:
                            text = r.read().decode()
                    except Exception:
                        text = ""
                    if text:
                        ms, _mprev[lane] = _prom_state_root_ms(text, _mprev[lane])
                        if ms is not None:
                            st["root_ms"] = ms
                            _mprev[lane]["last_ms"] = ms
                if cid:
                    rb, wb = _cgroup_io(cid)
                    if rb is not None:
                        pt, prb, pwb = st.get("_io", (None, 0, 0))
                        if pt is not None and now > pt:
                            dt = now - pt
                            st["rd_mb_s"] = round((rb - prb) / dt / 1048576, 2)
                            st["wr_mb_s"] = round((wb - pwb) / dt / 1048576, 2)
                        st["_io"] = (now, rb, wb)
            _exec_series.append((now,
                                 _exec["evm"].get("root_ms"), _exec["pay"].get("root_ms"),
                                 _exec["evm"].get("rd_mb_s"), _exec["pay"].get("rd_mb_s")))
        except Exception:
            pass
        time.sleep(3)

def exec_stats():
    def pub(d):
        return {k: v for k, v in d.items() if not k.startswith("_")}
    return {"evm": pub(_exec["evm"]), "pay": pub(_exec["pay"]),
            "series": [x for x in list(_exec_series)[-100:]]}

# unique addresses seen in blocks (from + to), per lane — climbs as new accounts are created
_addr = {"evm": set(), "pay": set()}
_alast = {"evm": 0, "pay": 0}

def _addr_loop():
    ports = {"evm": EVM["val2"], "pay": PAY["val2"]}
    while True:
        for lane, port in ports.items():
            try:
                h = head(port)
                if not h:
                    continue
                start = _alast[lane] + 1 if _alast[lane] else max(1, h - 20)
                for n in range(start, min(h, start + 25) + 1):
                    b = rpc(port, "eth_getBlockByNumber", [hex(n), True])
                    if not b:
                        continue
                    for t in b["transactions"]:
                        _addr[lane].add(t["from"])
                        if t.get("to"):
                            _addr[lane].add(t["to"])
                    _alast[lane] = n
            except Exception:
                pass
        time.sleep(2)

def growth():
    # series tuples: (t, evm_state_kb, evm_hist_kb, pay_state_kb, pay_hist_kb)
    s = list(_series)
    ua = {"evm_addr": len(_addr["evm"]), "pay_addr": len(_addr["pay"])}
    cur = s[-1] if s else (0, 0, 0, 0, 0)

    def rate_kb_per_min(idx):
        if len(s) < 2:
            return 0
        tnow = s[-1][0]
        win = [x for x in s if x[0] >= tnow - 60] or s
        dt = ((s[-1][0] - win[0][0]) / 60.0) or 1e-9
        return round((s[-1][idx] - win[0][idx]) / dt)

    return {"series": s[-100:],
            "evm_state": cur[1], "evm_hist": cur[2], "pay_state": cur[3], "pay_hist": cur[4],
            "evm_state_rate": rate_kb_per_min(1), "evm_hist_rate": rate_kb_per_min(2),
            "pay_state_rate": rate_kb_per_min(3), "pay_hist_rate": rate_kb_per_min(4), **ua}
EVM = {"val1": 8545, "val2": 8645, "val3": 8745, "val4": 8845}
PAY = {"val1": 19545, "val2": 19645, "val3": 19745, "val4": 19845}
POOL = ThreadPoolExecutor(max_workers=16)

def rpc(port, method, params):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    req = urllib.request.Request(f"http://127.0.0.1:{port}", data=body,
                                 headers={"content-type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=1.5) as r:
            return json.load(r).get("result")
    except Exception:
        return None

def head(port):
    h = rpc(port, "eth_blockNumber", [])
    return int(h, 16) if h else None

HEADER_FIELDS = ["number", "hash", "parentHash", "stateRoot", "transactionsRoot", "receiptsRoot",
                 "withdrawalsRoot", "miner", "gasUsed", "gasLimit", "timestamp", "baseFeePerGas",
                 "extraData", "blobGasUsed", "excessBlobGas"]

def block_at(port, n):
    b = rpc(port, "eth_getBlockByNumber", [hex(n), False])
    if not b:
        return None
    return {"hash": b["hash"], "root": b["stateRoot"],
            "txs": len(b.get("transactions", [])), "ts": int(b["timestamp"], 16),
            "num": int(b["number"], 16),
            "header": {k: b.get(k) for k in HEADER_FIELDS}}

def lane_state(ports):
    heads = dict(zip(ports, POOL.map(lambda p: head(p), ports.values())))
    live = [h for h in heads.values() if h is not None]
    if not live:
        return {"up": False, "heads": heads}
    settled = max(1, min(live) - 3)
    blocks = dict(zip(ports, POOL.map(lambda p: block_at(p, settled), ports.values())))
    hashes = [b["hash"] for b in blocks.values() if b]
    agree = len(hashes) >= 2 and len(set(hashes)) == 1
    # representative block for the headline (first validator that answered)
    rep = next((b for b in blocks.values() if b), None)
    return {"up": True, "settled": settled, "agree": agree,
            "heads": heads,
            "val_hashes": {v: (blocks[v]["hash"][:10] if blocks[v] else None) for v in ports},
            "block": rep}

def value_id(evm_hash, pay_hash):
    """Combined consensus value the BFT certificate binds: keccak256(evmBlockHash ‖ paymentBlockHash),
    exactly as arc_consensus_types::block::commit_lanes computes it (cast keccak == the node's keccak256)."""
    concat = "0x" + evm_hash[2:] + pay_hash[2:]  # 64 bytes: two 32-byte hashes concatenated
    try:
        out = subprocess.run([CAST, "keccak", concat], capture_output=True, text=True, timeout=2)
        return (out.stdout.strip() or None)
    except Exception:
        return None

def collect():
    evm, pay = POOL.submit(lane_state, EVM), POOL.submit(lane_state, PAY)
    evm, pay = evm.result(), pay.result()
    both = None
    if evm.get("block") and pay.get("block") and evm["settled"] == pay["settled"]:
        eh, ph = evm["block"]["hash"], pay["block"]["hash"]
        both = {"height": evm["settled"], "evm_hash": eh, "pay_hash": ph,
                "value_id": value_id(eh, ph)}
    return {"evm": evm, "pay": pay, "both": both, "growth": growth(), "exec": exec_stats()}

HTML = r"""<!doctype html><html><head><meta charset=utf-8>
<meta name=viewport content="width=device-width,initial-scale=1">
<title>Arc dual-EL payment lane</title><style>
:root{--bg:#0d1117;--panel:#151b23;--line:#232c38;--ink:#e6edf3;--dim:#7d8794;
 --evm:#3aa8c1;--pay:#42c98a;--warn:#e0a44b;--bad:#e5484d;
 --mono:ui-monospace,"SF Mono","JetBrains Mono",Menlo,Consolas,monospace}
*{box-sizing:border-box}
body{margin:0;background:radial-gradient(1200px 600px at 50% -10%,#131c26,#0d1117);
 color:var(--ink);font-family:var(--mono);-webkit-font-smoothing:antialiased}
.wrap{max-width:1080px;margin:0 auto;padding:28px 22px 60px}
header{display:flex;align-items:baseline;justify-content:space-between;gap:16px;flex-wrap:wrap;margin-bottom:6px}
h1{font-size:19px;font-weight:600;letter-spacing:.02em;margin:0}
h1 b{color:var(--pay)}
.sub{color:var(--dim);font-size:12.5px;letter-spacing:.03em}
.pill{font-size:11px;padding:3px 10px;border-radius:20px;border:1px solid var(--line);color:var(--dim)}
.pill.ok{color:#0d1117;background:var(--pay);border-color:var(--pay);font-weight:600}
.pill.bad{color:#fff;background:var(--bad);border-color:var(--bad)}
.lanes{display:grid;grid-template-columns:1fr 1fr;gap:16px;margin:18px 0}
.lane{background:var(--panel);border:1px solid var(--line);border-radius:10px;padding:20px 22px;position:relative;overflow:hidden}
.lane.flash::after{content:"";position:absolute;inset:0;background:currentColor;opacity:.12;animation:fl .6s ease-out}
@keyframes fl{from{opacity:.22}to{opacity:0}}
@media(prefers-reduced-motion:reduce){.lane.flash::after{animation:none;opacity:0}}
.lane.evm{color:var(--evm)} .lane.pay{color:var(--pay)}
.lane .name{font-size:12px;letter-spacing:.16em;text-transform:uppercase;font-weight:600}
.lane .name .dotc{width:8px;height:8px;border-radius:50%;background:currentColor;display:inline-block;margin-right:8px;vertical-align:middle}
.blk{font-size:52px;font-weight:600;letter-spacing:-.02em;color:var(--ink);line-height:1.05;margin:10px 0 2px;font-variant-numeric:tabular-nums}
.blk .h{color:var(--dim);font-size:22px;font-weight:400}
.row{display:flex;justify-content:space-between;border-top:1px solid var(--line);padding:9px 0;font-size:13px}
.row .k{color:var(--dim)} .row .v{color:var(--ink);font-variant-numeric:tabular-nums}
.root{color:currentColor;font-weight:600}
.grow{background:var(--panel);border:1px solid var(--line);border-radius:10px;padding:18px 22px;margin:0 0 16px}
.grow .hd{display:flex;justify-content:space-between;align-items:baseline;font-size:11px;letter-spacing:.14em;text-transform:uppercase;color:var(--dim);font-weight:600;margin-bottom:14px}
.grow .hd .rx{color:var(--pay)}
.gtop{display:grid;grid-template-columns:1fr 1fr;gap:14px 30px;margin-bottom:16px}
.gstat .gl{font-size:11px;letter-spacing:.1em;text-transform:uppercase;font-weight:600;margin-bottom:6px}
.gstat.evm .gl{color:var(--evm)} .gstat.pay .gl{color:var(--pay)}
.growrow{display:flex;align-items:baseline;gap:10px;padding:6px 0;border-top:1px solid var(--line)}
.growrow .gk{font-size:10.5px;letter-spacing:.08em;text-transform:uppercase;color:var(--dim);width:56px;flex:none}
.growrow .gv{font-size:21px;font-weight:600;color:var(--ink);font-variant-numeric:tabular-nums}
.growrow .gsub{font-size:11px;color:var(--dim);font-weight:400}
.growrow .grr{font-size:11.5px;color:var(--dim);font-variant-numeric:tabular-nums;margin-left:auto}
.gcharts{display:grid;grid-template-columns:1fr 1fr;gap:18px}
.gc .gct{font-size:10.5px;letter-spacing:.06em;text-transform:uppercase;color:var(--dim);margin-bottom:5px}
.gc svg{width:100%;height:96px;display:block}
.gnote{font-size:12px;color:#8895a2;margin-top:14px;line-height:1.6;border-top:1px solid var(--line);padding-top:11px}
.gnote b{color:var(--warn)}
@media(max-width:640px){.gtop,.gcharts{grid-template-columns:1fr}}
.vid{background:linear-gradient(180deg,#141d27,#101720);border:1px solid #26333f;border-radius:10px;padding:18px 22px;margin:0 0 16px;box-shadow:0 0 0 1px rgba(224,164,75,.06)}
.vid .hd{display:flex;justify-content:space-between;align-items:baseline;font-size:11px;letter-spacing:.14em;text-transform:uppercase;color:var(--warn);font-weight:600}
.vid .hd .hg{color:var(--dim)}
.vid .formula{font-size:13px;color:var(--dim);margin:12px 0 4px;line-height:1.85}
.vid .formula .fn{color:#cdd9e5}
.vid .inh{color:var(--evm)} .vid .inp{color:var(--pay)}
.vid .arrow{color:#586675}
.vid .out{display:flex;align-items:center;gap:10px;font-size:clamp(13px,2.4vw,19px);font-weight:600;
 color:#eafff5;word-break:break-all;margin-top:10px;padding:11px 13px;background:#0b141d;
 border:1px solid #23323e;border-radius:6px}
.vid .out .lab{color:var(--warn);flex:none}
.vid .note{color:#8895a2;font-size:12px;margin-top:11px;line-height:1.55}
.vid .note b{color:#cdd9e5;font-weight:600}
.hdr{background:var(--panel);border:1px solid var(--line);border-radius:10px;padding:16px 20px;margin:16px 0 0}
.hdr .hd{display:flex;justify-content:space-between;align-items:baseline;font-size:11px;letter-spacing:.14em;text-transform:uppercase;color:var(--dim);font-weight:600;margin-bottom:12px}
.hdr .hd .rx{color:var(--dim);letter-spacing:.04em;text-transform:none}
.hdrtbl{width:100%;border-collapse:collapse;font-size:12px}
.hdrtbl th{text-align:left;color:var(--dim);font-weight:600;font-size:10.5px;letter-spacing:.06em;text-transform:uppercase;padding:6px 12px;border-bottom:1px solid var(--line)}
.hdrtbl td{padding:5px 12px;border-bottom:1px solid var(--line);font-variant-numeric:tabular-nums;white-space:nowrap}
.hdrtbl td.k{color:var(--dim)}
.hdrtbl td.val{color:#7d8794}
.hdrtbl tr.diff td.evm{color:var(--evm)} .hdrtbl tr.diff td.pay{color:var(--pay)}
.hdrtbl tr.diff td.k{color:var(--ink)}
.xc{background:var(--panel);border:1px solid var(--line);border-radius:10px;padding:16px 20px;margin:16px 0 0}
.xc .hd{display:flex;justify-content:space-between;align-items:baseline;font-size:11px;letter-spacing:.14em;text-transform:uppercase;color:var(--dim);font-weight:600;margin-bottom:12px}
.xc .hd .rx{color:var(--warn);letter-spacing:.04em;text-transform:none}
.xgrid{display:grid;grid-template-columns:1fr 1fr;gap:14px 30px;margin-bottom:14px}
.xstat .gl{font-size:11px;letter-spacing:.1em;text-transform:uppercase;font-weight:600;margin-bottom:6px}
.xstat.evm .gl{color:var(--evm)} .xstat.pay .gl{color:var(--pay)}
.xrow{display:flex;align-items:baseline;gap:10px;padding:6px 0;border-top:1px solid var(--line)}
.xrow .gk{font-size:10.5px;letter-spacing:.08em;text-transform:uppercase;color:var(--dim);width:92px;flex:none}
.xrow .gv{font-size:21px;font-weight:600;color:var(--ink);font-variant-numeric:tabular-nums}
.xrow .gu{font-size:11px;color:var(--dim)}
.xc .gct{font-size:10.5px;letter-spacing:.06em;text-transform:uppercase;color:var(--dim);margin:4px 0 5px}
.xc svg{width:100%;height:110px;display:block}
@media(max-width:640px){.xgrid{grid-template-columns:1fr}}
.agree{background:var(--panel);border:1px solid var(--line);border-radius:10px;padding:16px 22px;
 display:flex;align-items:center;justify-content:space-between;gap:14px;flex-wrap:wrap}
.vgrp{display:flex;gap:22px;flex-wrap:wrap}
.vg{font-size:12px;color:var(--dim)}
.vg .lbl{letter-spacing:.1em;text-transform:uppercase;margin-right:10px}
.dot{width:11px;height:11px;border-radius:50%;display:inline-block;margin:0 3px;vertical-align:middle;
 background:var(--line);box-shadow:0 0 0 1px var(--line)}
.dot.on{background:var(--pay);box-shadow:0 0 8px var(--pay)}
.dot.off{background:var(--bad)}
.verdict{font-size:14px;font-weight:600}
.verdict.ok{color:var(--pay)} .verdict.bad{color:var(--bad)}
.foot{color:var(--dim);font-size:11.5px;margin-top:16px;line-height:1.7}
.mono2{color:#9fb0c0}
.tick{position:absolute;top:16px;right:20px;font-size:11px;color:var(--dim)}
</style></head><body><div class=wrap>
<header>
 <div><h1>Arc &mdash; dual&#8209;EL <b>payment lane</b></h1>
  <div class=sub>one consensus, two execution layers, two state roots per block &middot; 4 validators &middot; Malachite BFT</div></div>
 <div id=chainpill class=pill>connecting&hellip;</div>
</header>

<div class=lanes>
 <div class="lane evm" id=laneEvm>
  <div class=tick id=tickEvm></div>
  <div class=name><span class=dotc></span>EVM lane</div>
  <div class=blk>#<span id=evmBlk>&mdash;</span></div>
  <div class=row><span class=k>state root</span><span class="v root" id=evmRoot>&mdash;</span></div>
  <div class=row><span class=k>transactions</span><span class=v id=evmTx>&mdash;</span></div>
  <div class=row><span class=k>head / 4 validators</span><span class=v id=evmHead>&mdash;</span></div>
 </div>
 <div class="lane pay" id=lanePay>
  <div class=tick id=tickPay></div>
  <div class=name><span class=dotc></span>Payment lane</div>
  <div class=blk>#<span id=payBlk>&mdash;</span></div>
  <div class=row><span class=k>state root</span><span class="v root" id=payRoot>&mdash;</span></div>
  <div class=row><span class=k>transactions</span><span class=v id=payTx>&mdash;</span></div>
  <div class=row><span class=k>head / 4 validators</span><span class=v id=payHead>&mdash;</span></div>
 </div>
</div>

<div class=grow>
 <div class=hd><span>state &nbsp;vs&nbsp; history &mdash; on disk</span><span class=rx id=grwNote></span></div>
 <div class=gtop>
  <div class="gstat evm"><div class=gl>EVM lane</div>
   <div class=growrow><span class=gk>state</span><span class=gv id=evmState>&mdash;</span> <span class=gsub id=evmAcct></span><span class=grr id=evmStateR></span></div>
   <div class=growrow><span class=gk>history</span><span class=gv id=evmHist>&mdash;</span><span class=grr id=evmHistR></span></div></div>
  <div class="gstat pay"><div class=gl>Payment lane</div>
   <div class=growrow><span class=gk>state</span><span class=gv id=payState>&mdash;</span> <span class=gsub id=payAcct></span><span class=grr id=payStateR></span></div>
   <div class=growrow><span class=gk>history</span><span class=gv id=payHist>&mdash;</span><span class=grr id=payHistR></span></div></div>
 </div>
 <div class=gcharts>
  <div class=gc><div class=gct>payment state &mdash; db/ (accounts + trie)</div><svg id=svgState viewBox="0 0 320 96" preserveAspectRatio=none></svg></div>
  <div class=gc><div class=gct>payment history &mdash; static_files + rocksdb</div><svg id=svgHist viewBox="0 0 320 96" preserveAspectRatio=none></svg></div>
 </div>
 <div class=gnote><b>State</b> (db/) is the accounts + storage trie &mdash; exactly what a <b>pruned Arc
  snapshot ships</b>. <b>History</b> (static_files + rocksdb) is block bodies, receipts and indices &mdash;
  <b>excluded from the snapshot</b>. Note the two charts have independent axes: state is tiny and grows
  slowly (new accounts); history balloons with tx volume.</div>
</div>

<div class=vid>
 <div class=hd><span>equivocation&#8209;safe consensus value</span><span class=hg id=vidHeight>height &mdash;</span></div>
 <div class=formula><span class=fn>value_id</span> = keccak256(<br>
  &nbsp;&nbsp;<span class=inh id=vidEvm>&mdash;</span> <span class=arrow>&larr; EVM block hash</span><br>
  &nbsp;&nbsp;<span class=inp id=vidPay>&mdash;</span> <span class=arrow>&larr; payment block hash</span><br>
 )</div>
 <div class=out><span class=lab>=</span><span id=vidOut>&mdash;</span></div>
 <div class=note>One BFT certificate binds <b>both</b> lanes. A proposer cannot stream a second payment payload under a valid EVM commit &mdash; every validator certifies <b>this exact value</b> at this height.</div>
</div>

<div class=agree>
 <div class=vgrp>
  <div class=vg><span class=lbl>EVM</span><span id=evmDots></span></div>
  <div class=vg><span class=lbl>PAY</span><span id=payDots></span></div>
 </div>
 <div class=verdict id=verdict>&mdash;</div>
</div>

<div class=hdr>
 <div class=hd><span>block headers &mdash; both lanes @ <span id=hdrHeight>&mdash;</span></span><span class=rx>two independent chains, one consensus height</span></div>
 <div style="overflow-x:auto">
  <table class=hdrtbl>
   <thead><tr><th>field</th><th style="color:var(--evm)">EVM lane</th><th style="color:var(--pay)">payment lane</th></tr></thead>
   <tbody id=hdrBody></tbody>
  </table>
 </div>
</div>

<div class=xc>
 <div class=hd><span>execution cost &mdash; state root &amp; disk I/O (validator2, live)</span><span class=rx>the divergence the payment lane exists to avoid</span></div>
 <div class=xgrid>
  <div class="xstat evm"><div class=gl>EVM lane</div>
   <div class=xrow><span class=gk>state root</span><span class=gv id=xEvmRoot>&mdash;</span><span class=gu>ms avg</span></div>
   <div class=xrow><span class=gk>disk read</span><span class=gv id=xEvmRd>&mdash;</span><span class=gu>MB/s</span></div>
   <div class=xrow><span class=gk>disk write</span><span class=gv id=xEvmWr>&mdash;</span><span class=gu>MB/s</span></div></div>
  <div class="xstat pay"><div class=gl>Payment lane</div>
   <div class=xrow><span class=gk>state root</span><span class=gv id=xPayRoot>&mdash;</span><span class=gu>ms avg</span></div>
   <div class=xrow><span class=gk>disk read</span><span class=gv id=xPayRd>&mdash;</span><span class=gu>MB/s</span></div>
   <div class=xrow><span class=gk>disk write</span><span class=gv id=xPayWr>&mdash;</span><span class=gu>MB/s</span></div></div>
 </div>
 <div class=gct>state-root latency over time &mdash; <span style="color:var(--evm)">EVM</span> vs <span style="color:var(--pay)">payment</span></div>
 <svg id=svgExec viewBox="0 0 640 110" preserveAspectRatio=none></svg>
</div>

<div class=foot>
 The certified consensus value binds <span class=mono2>keccak(evmBlockHash &#8214; paymentBlockHash)</span> &mdash;
 both lanes are committed by one BFT certificate, so every validator must agree on <em>both</em> roots at every height.
 <br>Settled&#8209;height agreement shown above (head minus 3). Poll 1s.
</div>
</div><script>
let lastEvm=null,lastPay=null;
function short(h){return h?h.slice(0,10)+'…':'—';}
function fmtKB(kb){kb=kb||0;if(kb>=1048576)return (kb/1048576).toFixed(2)+' GB';if(kb>=1024)return (kb/1024).toFixed(1)+' MB';return kb+' KB';}
function fmtRate(kpm){kpm=kpm||0;return kpm>=1024?('+'+(kpm/1024).toFixed(1)+' MB/min'):('+'+kpm+' KB/min');}
function spark(id,s,idx,color){
 const svg=document.getElementById(id);if(!svg||!s||s.length<2){return;}
 const W=320,H=96,pad=6;
 const ts=s.map(x=>x[0]),v=s.map(x=>x[idx]);
 const t0=ts[0],t1=ts[ts.length-1],span=(t1-t0)||1;
 const vmax=Math.max(Math.max(...v),1)*1.12;
 const X=t=>pad+((t-t0)/span)*(W-2*pad);
 const Y=val=>H-pad-(val/vmax)*(H-2*pad);
 const line=v.map((val,i)=>(i?'L':'M')+X(ts[i]).toFixed(1)+','+Y(val).toFixed(1)).join('');
 const area='M'+X(ts[0]).toFixed(1)+','+Y(0)+line.slice(1)+' L'+X(ts[ts.length-1]).toFixed(1)+','+Y(0)+' Z';
 svg.innerHTML='<path d="'+area+'" fill="'+color+'22"/><path d="'+line+'" fill=none stroke="'+color+'" stroke-width=2.2/>';
}
function drawGrowth(g){
 if(!g)return;
 const nf=n=>(n||0).toLocaleString();
 document.getElementById('evmState').textContent=fmtKB(g.evm_state);
 document.getElementById('evmHist').textContent=fmtKB(g.evm_hist);
 document.getElementById('payState').textContent=fmtKB(g.pay_state);
 document.getElementById('payHist').textContent=fmtKB(g.pay_hist);
 document.getElementById('evmStateR').textContent=fmtRate(g.evm_state_rate);
 document.getElementById('evmHistR').textContent=fmtRate(g.evm_hist_rate);
 document.getElementById('payStateR').textContent=fmtRate(g.pay_state_rate);
 document.getElementById('payHistR').textContent=fmtRate(g.pay_hist_rate);
 document.getElementById('evmAcct').textContent='· '+nf(g.evm_addr)+' accts';
 document.getElementById('payAcct').textContent='· '+nf(g.pay_addr)+' accts';
 const rh=g.pay_state_rate>0?Math.round(g.pay_hist_rate/g.pay_state_rate):0;
 document.getElementById('grwNote').textContent=rh?('history grows ~'+rh+'× faster than state'):'';
 spark('svgState',g.series,3,'#42c98a');
 spark('svgHist',g.series,4,'#e0a44b');
}
function drawExec(x){
 if(!x)return;
 const set=(id,v)=>{document.getElementById(id).textContent=(v==null?'—':v);};
 set('xEvmRoot',x.evm.root_ms);set('xPayRoot',x.pay.root_ms);
 set('xEvmRd',x.evm.rd_mb_s);set('xPayRd',x.pay.rd_mb_s);
 set('xEvmWr',x.evm.wr_mb_s);set('xPayWr',x.pay.wr_mb_s);
 const s=(x.series||[]).filter(r=>r[1]!=null||r[2]!=null);
 if(s.length<2)return;
 const W=640,H=110,pad=8;
 const ts=s.map(r=>r[0]);
 const t0=ts[0],span=(ts[ts.length-1]-t0)||1;
 const vmax=Math.max(...s.map(r=>Math.max(r[1]||0,r[2]||0)),0.1)*1.15;
 const X=t=>pad+((t-t0)/span)*(W-2*pad);
 const Y=v=>H-pad-((v||0)/vmax)*(H-2*pad);
 const line=idx=>{let d='',pen=false;
  for(const r of s){ if(r[idx]==null){pen=false;continue;}
   d+=(pen?'L':'M')+X(r[0]).toFixed(1)+','+Y(r[idx]).toFixed(1);pen=true;}
  return d;};
 document.getElementById('svgExec').innerHTML=
  '<text x="'+(W-10)+'" y="14" text-anchor="end" font-family="ui-monospace,monospace" font-size="10" fill="#7d8794">max '+vmax.toFixed(1)+' ms</text>'+
  '<path d="'+line(1)+'" fill=none stroke="#3aa8c1" stroke-width=2/>'+
  '<path d="'+line(2)+'" fill=none stroke="#42c98a" stroke-width=2/>';
}
const HDR_INTS=['gasUsed','gasLimit','timestamp','baseFeePerGas','blobGasUsed','excessBlobGas'];
function fmtHdr(f,v){
 if(v==null)return '—';
 if(HDR_INTS.includes(f))return parseInt(v,16).toLocaleString();
 if(f==='extraData')return v.length>22?v.slice(0,22)+'…':(v==='0x'?'0x (empty)':v);
 if(typeof v==='string'&&v.length>26)return v.slice(0,12)+'…'+v.slice(-8);
 return v;
}
const HDR_ROWS=['parentHash','hash','stateRoot','transactionsRoot','receiptsRoot','withdrawalsRoot','miner','gasUsed','gasLimit','timestamp','baseFeePerGas','extraData','blobGasUsed','excessBlobGas'];
function renderHeaders(s){
 const e=s.evm&&s.evm.block&&s.evm.block.header, p=s.pay&&s.pay.block&&s.pay.block.header;
 if(!e||!p)return;
 document.getElementById('hdrHeight').textContent='#'+parseInt(e.number,16).toLocaleString();
 let html='';
 for(const f of HDR_ROWS){
  const ev=e[f], pv=p[f]; const diff=(ev!==pv);
  html+='<tr class="'+(diff?'diff':'')+'"><td class=k>'+f+'</td>'
   +'<td class="val evm" title="'+(ev==null?'':ev)+'">'+fmtHdr(f,ev)+'</td>'
   +'<td class="val pay" title="'+(pv==null?'':pv)+'">'+fmtHdr(f,pv)+'</td></tr>';
 }
 document.getElementById('hdrBody').innerHTML=html;
}
function dots(el,vhashes,agree){el.innerHTML='';
 for(const v of ['val1','val2','val3','val4']){const d=document.createElement('span');
  d.className='dot '+(vhashes[v]?(agree?'on':'off'):'');d.title=v+': '+(vhashes[v]||'no data');el.appendChild(d);}}
async function tick(){
 let s;try{s=await(await fetch('/state')).json();}catch(e){return;}
 const pill=document.getElementById('chainpill');
 const evm=s.evm,pay=s.pay;
 if(evm.up&&evm.block){
  document.getElementById('evmBlk').textContent=evm.block.num;
  document.getElementById('evmRoot').textContent=short(evm.block.root);
  document.getElementById('evmTx').textContent=evm.block.txs;
  document.getElementById('evmHead').textContent=Object.values(evm.heads).map(x=>x??'·').join('  ');
  dots(document.getElementById('evmDots'),evm.val_hashes,evm.agree);
  if(lastEvm!==null&&evm.block.num>lastEvm){const l=document.getElementById('laneEvm');l.classList.remove('flash');void l.offsetWidth;l.classList.add('flash');}
  lastEvm=evm.block.num;
 }
 if(pay.up&&pay.block){
  document.getElementById('payBlk').textContent=pay.block.num;
  document.getElementById('payRoot').textContent=short(pay.block.root);
  document.getElementById('payTx').textContent=pay.block.txs;
  document.getElementById('payHead').textContent=Object.values(pay.heads).map(x=>x??'·').join('  ');
  dots(document.getElementById('payDots'),pay.val_hashes,pay.agree);
  if(lastPay!==null&&pay.block.num>lastPay){const l=document.getElementById('lanePay');l.classList.remove('flash');void l.offsetWidth;l.classList.add('flash');}
  lastPay=pay.block.num;
 }
 drawGrowth(s.growth);
 renderHeaders(s);
 drawExec(s.exec);
 if(s.both){
  document.getElementById('vidHeight').textContent='height '+s.both.height;
  document.getElementById('vidEvm').textContent=s.both.evm_hash;
  document.getElementById('vidPay').textContent=s.both.pay_hash;
  document.getElementById('vidOut').textContent=s.both.value_id||'computing…';
 }
 const bothAgree=evm.agree&&pay.agree;
 const roots2=evm.block&&pay.block&&evm.block.root!==pay.block.root;
 const vd=document.getElementById('verdict');
 if(evm.up&&pay.up){vd.textContent=bothAgree?'✓ all 4 validators agree — both lanes':'✗ divergence';
  vd.className='verdict '+(bothAgree?'ok':'bad');
  pill.textContent=roots2?'two distinct roots ✓':'both lanes live';pill.className='pill ok';}
 else{pill.textContent='waiting for lanes…';pill.className='pill';}
}
tick();setInterval(tick,1000);
</script></body></html>"""

class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_GET(self):
        if self.path.startswith("/state"):
            data = json.dumps(collect()).encode()
            self.send_response(200); self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(data))); self.end_headers()
            self.wfile.write(data)
        else:
            b = HTML.encode()
            self.send_response(200); self.send_header("content-type", "text/html; charset=utf-8")
            self.send_header("content-length", str(len(b))); self.end_headers()
            self.wfile.write(b)

class Srv(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True

if __name__ == "__main__":
    threading.Thread(target=_growth_loop, daemon=True).start()
    threading.Thread(target=_addr_loop, daemon=True).start()
    threading.Thread(target=_exec_loop, daemon=True).start()
    print(f"dual-EL dashboard on http://localhost:{PORT}")
    Srv(("0.0.0.0", PORT), H).serve_forever()
