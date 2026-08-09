#!/usr/bin/env python3
# ============================================================================
#  SUPERSEDED — DO NOT CITE THESE NUMBERS.
#
#  Single-machine, SHORT windows (75 s). Both properties make it unreliable:
#  run-to-run variance here is +-12-15%, and one box runs all four validators
#  on shared cores. Its "50M HOLDS 2 blk/s" row was RETRACTED — a reverse-order
#  repeat gave 1.72 blk/s, and the 15-minute sustained run gives 537 ms / 1.86
#  blk/s, which does not hold.
#
#  AUTHORITATIVE: experiments/dual-el/frontier-long-chart.py
#      4 machines, 15 min per size, per-minute sampling, spread 1.6-3.6%.
#
#  Kept only as the historical record of how the measurement evolved.
# ============================================================================
"""Payment lane: block size vs throughput vs latency — presentation chart.

Renders the measured block-size sweep (experiments/dual-el/blocksize-sweep.sh) as a
self-contained SVG for the slide deck. No dependencies.

    python3 experiments/dual-el/blocksize-chart.py [out.svg]

2026-08-09 fine-grained sweep: one chain, gas limit flipped at runtime, all four payment ELs on
the same config (--engine.state-root-fallback), 75 s windows, SIX local spammers so every point is
load-saturated.

HONESTY NOTES baked into the chart, because they change how it should be read:
  * Hollow points MISS the 2 blk/s target. Every point here is 100% full, so unlike the earlier
    coarse sweep none of them are delivery-bound.
  * 50M is MARGINAL, not a holding point: 1.96 blk/s in a forward sweep, 1.72 in a reverse-order
    repeat on a fresh chain. Run-to-run variance on this box is ~12-15%. 40M reproduced at 1.98.
    A reverse-order sweep (60 -> 55 -> 50) ruled out chain age as the explanation.
  * 25M is carried over from the earlier 4-spammer sweep (it was 100% full, so comparable).
"""
import sys

# gas(M), txs/blk, blk/s, ms/blk, tps, %full, holds-target?
#
# 2026-08-09 FINE-GRAINED sweep, 6 spammers so every point is load-SATURATED (100% full) --
# unlike the first coarse sweep, whose >=200M rows were delivery-bound and measured the spammer.
# 25M is carried over from that first sweep (it was 100% full, so saturated and comparable).
#
# CONFOUND, stated because it matters: sizes are swept sequentially on a GROWING chain, so later
# points carry more state. 55M ran last (~1,700 blocks in) and came out worse than 50M in BOTH
# tps and latency, which is partly chain age rather than block size.
ROWS = [
    (25,   1190, 1.99,  504, 2363, 100, True),
    (30,   1428, 1.99,  504, 2835, 100, True),
    (40,   1904, 1.98,  504, 3777, 100, True),
    (50,   2380, 1.96,  511, 4660, 100, False),   # MARGINAL: 1.96 in one run, 1.72 in a repeat
    (55,   2618, 1.69,  591, 4427, 100, False),
    (60,   2856, 1.75,  573, 4986, 100, False),
    (75,   3571, 1.45,  689, 5186, 100, False),
]
TARGET_MS = 500

W, H = 960, 560
L, R, T, B = 78, 78, 76, 92
PW, PH = W - L - R, H - T - B

INK = "#1a1f2e"
MUTED = "#7b8394"
TPS = "#2563eb"
LAT = "#e0562d"
GRID = "#e6e9ef"
OK = "#15803d"

import math
xs = [math.log10(r[0]) for r in ROWS]
x0, x1 = min(xs), max(xs)
TPS_MAX = 6000
LAT_MAX = 800

def px(g):   return L + (math.log10(g) - x0) / (x1 - x0) * PW
def py_t(v): return T + PH - v / TPS_MAX * PH
def py_l(v): return T + PH - v / LAT_MAX * PH

o = []
a = o.append
a(f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="{W}" height="{H}" '
  f'font-family="Inter,Helvetica,Arial,sans-serif">')
a(f'<rect width="{W}" height="{H}" fill="#ffffff"/>')
a(f'<text x="{L}" y="34" font-size="19" font-weight="700" fill="{INK}">'
  f'Payment lane: 40M gas reliably holds 2 blocks/s; 50M is marginal</text>')
a(f'<text x="{L}" y="56" font-size="12.5" fill="{MUTED}">'
  f'One chain, gas limit flipped at runtime · 4 validators · every point load-saturated (100% full)</text>')

# grid + left axis (tps)
for i in range(0, 6):
    v = TPS_MAX * i / 5
    y = py_t(v)
    a(f'<line x1="{L}" y1="{y:.1f}" x2="{L+PW}" y2="{y:.1f}" stroke="{GRID}" stroke-width="1"/>')
    a(f'<text x="{L-10}" y="{y+4:.1f}" font-size="11" fill="{TPS}" text-anchor="end">{int(v):,}</text>')
# right axis (latency)
for i in range(0, 5):
    v = LAT_MAX * i / 4
    a(f'<text x="{L+PW+10}" y="{py_l(v)+4:.1f}" font-size="11" fill="{LAT}">{int(v)}</text>')

a(f'<text x="{L-52}" y="{T+PH/2}" font-size="12" font-weight="600" fill="{TPS}" '
  f'transform="rotate(-90 {L-52} {T+PH/2})" text-anchor="middle">throughput (tx/s)</text>')
a(f'<text x="{L+PW+56}" y="{T+PH/2}" font-size="12" font-weight="600" fill="{LAT}" '
  f'transform="rotate(90 {L+PW+56} {T+PH/2})" text-anchor="middle">block latency (ms)</text>')

# 500 ms target
yt = py_l(TARGET_MS)
a(f'<line x1="{L}" y1="{yt:.1f}" x2="{L+PW}" y2="{yt:.1f}" stroke="{OK}" stroke-width="1.6" stroke-dasharray="7 4"/>')
a(f'<text x="{L+PW-6}" y="{yt-8:.1f}" font-size="11.5" font-weight="600" fill="{OK}" text-anchor="end">'
  f'2 blocks/s target — 500 ms</text>')

def poly(fy, idx, color, dash=""):
    pts = " ".join(f"{px(r[0]):.1f},{fy(r[idx]):.1f}" for r in ROWS)
    d = f' stroke-dasharray="{dash}"' if dash else ""
    a(f'<polyline points="{pts}" fill="none" stroke="{color}" stroke-width="2.6"{d}/>')

poly(py_t, 4, TPS)
poly(py_l, 3, LAT, "5 3")

for g, txs, bps, ms, tps, full, solid in ROWS:
    x = px(g)
    for fy, val, color in ((py_t, tps, TPS), (py_l, ms, LAT)):
        y = fy(val)
        fill = color if solid else "#ffffff"
        a(f'<circle cx="{x:.1f}" cy="{y:.1f}" r="5.2" fill="{fill}" stroke="{color}" stroke-width="2.2"/>')
    a(f'<text x="{x:.1f}" y="{T+PH+20}" font-size="11.5" font-weight="600" fill="{INK}" text-anchor="middle">{g}M</text>')
    a(f'<text x="{x:.1f}" y="{T+PH+35}" font-size="10" fill="{MUTED}" text-anchor="middle">{txs:,} tx</text>')
    a(f'<text x="{x:.1f}" y="{T+PH+48}" font-size="10" fill="{MUTED if full<100 else INK}" text-anchor="middle">{full}% full</text>')

# callouts
x25 = px(25)
a(f'<text x="{x25-4}" y="{py_t(2363)-14:.1f}" font-size="11" fill="{MUTED}">2,363 tps</text>')
x50 = px(50)
a(f'<text x="{x50}" y="{py_t(4660)-16:.1f}" font-size="11.5" font-weight="700" fill="{MUTED}" text-anchor="middle">'
  f'50M · 4,660 tps — MARGINAL (1.96 blk/s once, 1.72 on repeat)</text>')
x40 = px(40)
a(f'<text x="{x40}" y="{py_t(3777)+26:.1f}" font-size="12" font-weight="700" fill="{OK}" text-anchor="middle">'
  f'40M · 3,777 tps @ 504 ms — reproducible</text>')

# legend
ly = H - 30
a(f'<line x1="{L}" y1="{ly-4}" x2="{L+26}" y2="{ly-4}" stroke="{TPS}" stroke-width="2.6"/>')
a(f'<text x="{L+32}" y="{ly}" font-size="11.5" fill="{INK}">throughput</text>')
a(f'<line x1="{L+120}" y1="{ly-4}" x2="{L+146}" y2="{ly-4}" stroke="{LAT}" stroke-width="2.6" stroke-dasharray="5 3"/>')
a(f'<text x="{L+152}" y="{ly}" font-size="11.5" fill="{INK}">latency</text>')
a(f'<circle cx="{L+232}" cy="{ly-4}" r="5.2" fill="#ffffff" stroke="{MUTED}" stroke-width="2.2"/>')
a(f'<text x="{L+244}" y="{ly}" font-size="11.5" fill="{MUTED}">'
  f'hollow = misses the 2 blk/s target (55M also carries extra chain age — swept last)</text>')
a('</svg>')

out = sys.argv[1] if len(sys.argv) > 1 else "/tmp/blocksize-chart.svg"
open(out, "w").write("\n".join(o))
print(f"wrote {out}")
print(f"{'gas':>7} {'txs/blk':>9} {'blk/s':>7} {'ms':>6} {'tps':>7} {'full':>6}  note")
for g, txs, bps, ms, tps, full, solid in ROWS:
    note = "HOLDS 2 blk/s" if bps >= 1.9 else "degraded"
    print(f"{g:>6}M {txs:>9,} {bps:>7.2f} {ms:>6} {tps:>7,} {full:>5}%  {note}")
