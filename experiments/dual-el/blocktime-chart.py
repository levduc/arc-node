#!/usr/bin/env python3
"""Payment lane: the throughput/latency frontier, and why block time is not a throughput knob.

Renders the 2-D sweep (experiments/dual-el/blocktime-sweep.sh: target block time x gas limit) as a
self-contained SVG. No dependencies.

    python3 experiments/dual-el/blocktime-chart.py [out.svg]

2026-08-09, single-machine 4-validator chain, DISTRIBUTED load (2 local + 8 ginnythui + 5
papaduck-alien2 spammers over tailscale) so every point is 100% full -- local-only load saturates
around 6,000 tx/s and could never fill 100M+ blocks.

THE POINT OF THIS CHART: the target block time does NOT buy throughput. Under saturation the chain
runs at its natural cadence, which is set by the GAS LIMIT. A longer target only throttles (see the
1000 ms / 50M point, paced down to exactly 1.00 blk/s from a natural ~1.5); a shorter target does
nothing at all. So the real design choice is the gas limit, and it trades latency for throughput
with steep diminishing returns.
"""
import sys

# target_ms, gas(M), txs/blk, blk/s, ms/blk, tps, held-its-target?
ROWS = [
    (250,   50, 2380, 1.61,  620, 3840, False),
    (250,  100, 4761, 1.10,  910, 5234, False),
    (250,  200, 9523, 0.71, 1404, 6785, False),
    (500,   50, 2380, 1.47,  680, 3500, False),
    (500,  100, 4761, 1.06,  947, 5025, False),
    (500,  200, 9523, 0.68, 1460, 6522, False),
    (1000,  50, 2380, 1.00, 1001, 2378, True),
    (1000, 100, 4761, 1.00, 1003, 4748, True),
    (1000, 200, 9523, 0.67, 1492, 6382, False),
]

# 4-MACHINE FLEET, distributed spam (one spammer set per machine against its LOCAL payment EL).
# gas(M), txs/blk, blk/s, ms/blk, tps, %full, saturated?
FLEET = [
    (200,  6175, 1.16,  864, 7150,  65, False),   # 16 spammers
    (200,  9523, 0.80, 1250, 7616, 100, True),    # 32 spammers -- filled, but SLOWER
    (500, 15430, 0.53, 1876, 8226,  65, False),
    (1000,39832, 0.18, 5478, 7272,  84, False),
]

W, H = 980, 580
L, R, T, B = 84, 40, 82, 96
PW, PH = W - L - R, H - T - B
INK, MUTED, GRID = "#1a1f2e", "#7b8394", "#e6e9ef"
OK, WARN = "#15803d", "#b45309"
GAS_COLOR = {50: "#2563eb", 100: "#7c3aed", 200: "#db2777"}

LAT_MAX, TPS_MAX = 5800, 9000
def px(ms):  return L + ms / LAT_MAX * PW
def py(tps): return T + PH - tps / TPS_MAX * PH

o = []; a = o.append
a(f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="{W}" height="{H}" '
  f'font-family="Inter,Helvetica,Arial,sans-serif">')
a(f'<rect width="{W}" height="{H}" fill="#ffffff"/>')
a(f'<text x="{L}" y="34" font-size="19" font-weight="700" fill="{INK}">'
  f'Payment lane: throughput saturates near 7-8k tx/s; only latency changes</text>')
a(f'<text x="{L}" y="56" font-size="12.5" fill="{MUTED}">'
  f'One box (blue/purple/pink) vs 4 machines (teal) · on the fleet a 5x bigger block buys NO tps, just 6x the latency</text>')

for i in range(6):
    v = TPS_MAX * i / 5; y = py(v)
    a(f'<line x1="{L}" y1="{y:.1f}" x2="{L+PW}" y2="{y:.1f}" stroke="{GRID}"/>')
    a(f'<text x="{L-10}" y="{y+4:.1f}" font-size="11" fill="{MUTED}" text-anchor="end">{int(v):,}</text>')
for ms in (500, 1000, 2000, 3000, 4000, 5000):
    x = px(ms)
    a(f'<line x1="{x:.1f}" y1="{T}" x2="{x:.1f}" y2="{T+PH}" stroke="{GRID}"/>')
    a(f'<text x="{x:.1f}" y="{T+PH+18}" font-size="11" fill="{MUTED}" text-anchor="middle">{ms}</text>')
a(f'<text x="{L+PW/2}" y="{T+PH+40}" font-size="12.5" font-weight="600" fill="{INK}" text-anchor="middle">'
  f'achieved block latency (ms)</text>')
a(f'<text x="{L-58}" y="{T+PH/2}" font-size="12.5" font-weight="600" fill="{INK}" '
  f'transform="rotate(-90 {L-58} {T+PH/2})" text-anchor="middle">throughput (tx/s)</text>')

# 2 blk/s reference
a(f'<line x1="{px(500):.1f}" y1="{T}" x2="{px(500):.1f}" y2="{T+PH}" stroke="{OK}" stroke-width="1.6" stroke-dasharray="7 4"/>')
a(f'<text x="{px(500)+7:.1f}" y="{T+14}" font-size="11.5" font-weight="600" fill="{OK}">'
  f'2 blk/s target — unreachable at any size under this load</text>')

# one line per gas limit: the frontier the gas limit puts you on
for gas in (50, 100, 200):
    pts = [(r[4], r[5]) for r in ROWS if r[1] == gas]
    pts.sort()
    a(f'<polyline points="{" ".join(f"{px(m):.1f},{py(t):.1f}" for m, t in pts)}" fill="none" '
      f'stroke="{GAS_COLOR[gas]}" stroke-width="2.2" opacity="0.55"/>')

for target, gas, txs, bps, ms, tps, held in ROWS:
    x, y = px(ms), py(tps)
    c = GAS_COLOR[gas]
    a(f'<circle cx="{x:.1f}" cy="{y:.1f}" r="7" fill="{c if held else "#ffffff"}" stroke="{c}" stroke-width="2.4"/>')
    a(f'<text x="{x:.1f}" y="{y-13:.1f}" font-size="9.5" fill="{MUTED}" text-anchor="middle">{target}ms</text>')

# fleet frontier
fpts = sorted((r[3], r[4]) for r in FLEET)
a(f'<polyline points="{" ".join(f"{px(m):.1f},{py(t):.1f}" for m, t in fpts)}" fill="none" '
  f'stroke="#0f766e" stroke-width="2.8"/>')
for gas, txs, bps, ms, tps, full, sat in FLEET:
    x, y = px(ms), py(tps)
    a(f'<rect x="{x-6:.1f}" y="{y-6:.1f}" width="12" height="12" '
      f'fill="{"#0f766e" if sat else "#ffffff"}" stroke="#0f766e" stroke-width="2.4"/>')
    a(f'<text x="{x:.1f}" y="{y-13:.1f}" font-size="9.5" fill="#0f766e" text-anchor="middle">{gas}M</text>')
a(f'<text x="{px(864)+14:.1f}" y="{py(7150)-2:.1f}" font-size="12" font-weight="700" fill="#0f766e">'
  f'FLEET 200M: 7,150 tx/s @ 864 ms</text>')
a(f'<text x="{px(1876):.1f}" y="{py(8226)-16:.1f}" font-size="11" fill="#0f766e" text-anchor="middle">'
  f'fleet peak 8,226</text>')

# callouts
r = [x for x in ROWS if x[0] == 1000 and x[1] == 100][0]
a(f'<text x="{px(r[4])+14:.1f}" y="{py(r[5])+4:.1f}" font-size="12" font-weight="700" fill="{OK}">'
  f'sweet spot: 100M @ 1.00 blk/s — 4,748 tps, target HELD</text>')
r = [x for x in ROWS if x[0] == 250 and x[1] == 200][0]
a(f'<text x="{px(r[4]):.1f}" y="{py(r[5])-20:.1f}" font-size="11.5" font-weight="600" fill="{WARN}" text-anchor="middle">'
  f'best tps 6,785 — but 1.4 s blocks</text>')
r = [x for x in ROWS if x[0] == 1000 and x[1] == 50][0]
a(f'<text x="{px(r[4])+12:.1f}" y="{py(r[5])+4:.1f}" font-size="11" fill="{MUTED}">'
  f'throttled: could do ~1.5 blk/s, paced to 1.0</text>')

ly = H - 34
for i, gas in enumerate((50, 100, 200)):
    x = L + i * 118
    a(f'<circle cx="{x}" cy="{ly-4}" r="6" fill="{GAS_COLOR[gas]}"/>')
    a(f'<text x="{x+12}" y="{ly}" font-size="11.5" fill="{INK}">{gas}M gas</text>')
a(f'<rect x="{L+366}" y="{ly-10}" width="12" height="12" fill="#0f766e"/>')
a(f'<text x="{L+384}" y="{ly}" font-size="11.5" font-weight="600" fill="#0f766e">'
  f'4-machine FLEET</text>')
a(f'<text x="{L+500}" y="{ly}" font-size="11" fill="{MUTED}">'
  f'hollow = target missed / not saturated</text>')
a('</svg>')

out = sys.argv[1] if len(sys.argv) > 1 else "/tmp/blocktime-chart.svg"
open(out, "w").write("\n".join(o))
print(f"wrote {out}")
print("FLEET (4 machines):")
for gas, txs, bps, ms, tps, full, sat in FLEET:
    print(f"  {gas:>5}M {txs:>7,} {bps:>6.2f} blk/s {ms:>5}ms {tps:>7,} tps {full:>4}% full")
print("\nSINGLE BOX:")
print(f"{'target':>7} {'gas':>6} {'txs/blk':>9} {'blk/s':>7} {'ms':>6} {'tps':>7}  verdict")
for target, gas, txs, bps, ms, tps, held in ROWS:
    print(f"{target:>6}ms {gas:>5}M {txs:>9,} {bps:>7.2f} {ms:>6} {tps:>7,}  {'HELD' if held else 'missed'}")
