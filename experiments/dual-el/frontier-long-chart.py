#!/usr/bin/env python3
"""Payment lane: the sustained throughput/latency frontier (15-minute windows, 4 machines).

    python3 experiments/dual-el/frontier-long-chart.py [out.svg]

2026-08-09. Four physical machines, one gas limit at a time, each held under continuous load for a
full 15 minutes with the chain sampled every 60 s. Demand was tuned per size to ~1.15x that size's
expected capacity: enough to fill the block, not enough to build a runaway backlog.

Why long windows: every short measurement in this project carried +-12-15% run-to-run variance, and
twice produced a headline that failed to reproduce. Over 15 minutes the spread collapses to
1.6-3.6%, and drift becomes visible -- which a single average cannot show.

THE SHAPE: the gas limit is a dial that only acts WHILE IT BINDS. At 25/50/100M it binds, the block
fills, and it sets both latency and throughput. At 200M the same demand no longer fills the block,
so 200M behaves almost exactly like 100M (4,892 vs 4,761 tx/block, 690 vs 697 ms). Raising the limit
past the point where demand can fill it changes nothing at all.
"""
import sys

# gas(M), txs/blk, %full, latency_ms, lat_lo, lat_hi, tps, sd%, binds?
ROWS = [
    (25,  1190, 100, 520,  505,  532, 2287, 1.6, True),
    (50,  2380, 100, 537,  527,  566, 4430, 1.8, True),
    (100, 4761, 100, 697,  662,  779, 6827, 3.6, True),
    (200, 4892,  51, 690,  524, 1176, 7092, 3.2, False),
]
# Forced-saturation points from shorter runs, for context (limit made to bind by doubling demand).
FORCED = [
    (200,  9523, 1230, 7700),
    (1000, 39832, 5478, 7272),
]

W, H = 980, 580
L, R, T, B = 88, 44, 84, 104
PW, PH = W - L - R, H - T - B
INK, MUTED, GRID = "#1a1f2e", "#7b8394", "#e6e9ef"
TEAL, OK, WARN = "#0f766e", "#15803d", "#b45309"
LAT_MAX, TPS_MAX = 5800, 9000

def px(ms):  return L + ms / LAT_MAX * PW
def py(t):   return T + PH - t / TPS_MAX * PH

o = []; a = o.append
a(f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="{W}" height="{H}" '
  f'font-family="Inter,Helvetica,Arial,sans-serif">')
a(f'<rect width="{W}" height="{H}" fill="#ffffff"/>')
a(f'<text x="{L}" y="34" font-size="19" font-weight="700" fill="{INK}">'
  f'Payment lane, sustained: the gas limit is a dial that only acts while it binds</text>')
a(f'<text x="{L}" y="56" font-size="12.5" fill="{MUTED}">'
  f'4 machines · 15 minutes per point · sampled every 60 s · demand tuned to just fill each block</text>')

for i in range(6):
    v = TPS_MAX * i / 5; y = py(v)
    a(f'<line x1="{L}" y1="{y:.1f}" x2="{L+PW}" y2="{y:.1f}" stroke="{GRID}"/>')
    a(f'<text x="{L-10}" y="{y+4:.1f}" font-size="11" fill="{MUTED}" text-anchor="end">{int(v):,}</text>')
for ms in (500, 1000, 2000, 3000, 4000, 5000):
    x = px(ms)
    a(f'<line x1="{x:.1f}" y1="{T}" x2="{x:.1f}" y2="{T+PH}" stroke="{GRID}"/>')
    a(f'<text x="{x:.1f}" y="{T+PH+18}" font-size="11" fill="{MUTED}" text-anchor="middle">{ms}</text>')
a(f'<text x="{L+PW/2}" y="{T+PH+40}" font-size="12.5" font-weight="600" fill="{INK}" text-anchor="middle">'
  f'sustained block latency (ms)</text>')
a(f'<text x="{L-60}" y="{T+PH/2}" font-size="12.5" font-weight="600" fill="{INK}" '
  f'transform="rotate(-90 {L-60} {T+PH/2})" text-anchor="middle">sustained throughput (tx/s)</text>')

a(f'<line x1="{px(500):.1f}" y1="{T}" x2="{px(500):.1f}" y2="{T+PH}" stroke="{OK}" stroke-width="1.6" stroke-dasharray="7 4"/>')
a(f'<text x="{px(500)+7:.1f}" y="{T+14}" font-size="11" font-weight="600" fill="{OK}">2 blk/s</text>')

# forced-saturation context curve
fp = " ".join(f"{px(m):.1f},{py(t):.1f}" for _, _, m, t in FORCED)
a(f'<polyline points="{px(697):.1f},{py(6827):.1f} {fp}" fill="none" stroke="{WARN}" '
  f'stroke-width="2" stroke-dasharray="6 4" opacity="0.75"/>')
for gas, txs, ms, tps in FORCED:
    a(f'<circle cx="{px(ms):.1f}" cy="{py(tps):.1f}" r="6" fill="#ffffff" stroke="{WARN}" stroke-width="2.2"/>')
    a(f'<text x="{px(ms):.1f}" y="{py(tps)-13:.1f}" font-size="9.5" fill="{WARN}" text-anchor="middle">{gas}M</text>')
a(f'<text x="{px(3000):.1f}" y="{py(7272)+26:.1f}" font-size="11" fill="{WARN}" text-anchor="middle">'
  f'forcing the limit to bind (2x demand): latency grows, throughput does not</text>')

# main sustained curve, with latency spread bars
binds = [r for r in ROWS if r[8]]
a(f'<polyline points="{" ".join(f"{px(r[3]):.1f},{py(r[6]):.1f}" for r in binds)}" fill="none" '
  f'stroke="{TEAL}" stroke-width="3"/>')
for gas, txs, full, ms, lo, hi, tps, sd, b in ROWS:
    y = py(tps)
    a(f'<line x1="{px(lo):.1f}" y1="{y:.1f}" x2="{px(hi):.1f}" y2="{y:.1f}" stroke="{TEAL}" '
      f'stroke-width="1.4" opacity="0.55"/>')
    a(f'<rect x="{px(ms)-6:.1f}" y="{y-6:.1f}" width="12" height="12" '
      f'fill="{TEAL if b else "#ffffff"}" stroke="{TEAL}" stroke-width="2.4"/>')
    a(f'<text x="{px(ms):.1f}" y="{y-14:.1f}" font-size="10" font-weight="600" fill="{TEAL}" '
      f'text-anchor="middle">{gas}M</text>')

a(f'<text x="{px(520)+14:.1f}" y="{py(2287)+5:.1f}" font-size="11" fill="{INK}">'
  f'25M — 520 ms, 2,287 tx/s  (sd 1.6%)</text>')
a(f'<text x="{px(537)+14:.1f}" y="{py(4430)+5:.1f}" font-size="11" fill="{INK}">'
  f'50M — 537 ms, 4,430 tx/s  (sd 1.8%)</text>')
a(f'<text x="{px(697)+16:.1f}" y="{py(6827)+5:.1f}" font-size="12" font-weight="700" fill="{TEAL}">'
  f'100M — 697 ms, 6,827 tx/s  (sd 3.6%)  ← best sustained</text>')
a(f'<text x="{px(690)+16:.1f}" y="{py(7092)-12:.1f}" font-size="10.5" fill="{MUTED}">'
  f'200M sits on top of 100M: the limit no longer binds (51% full)</text>')

ly = H - 40
a(f'<rect x="{L}" y="{ly-10}" width="12" height="12" fill="{TEAL}"/>')
a(f'<text x="{L+18}" y="{ly}" font-size="11.5" fill="{INK}">gas limit binds (block 100% full)</text>')
a(f'<rect x="{L+250}" y="{ly-10}" width="12" height="12" fill="#ffffff" stroke="{TEAL}" stroke-width="2.2"/>')
a(f'<text x="{L+268}" y="{ly}" font-size="11.5" fill="{INK}">limit does not bind</text>')
a(f'<text x="{L}" y="{ly+18}" font-size="10.5" fill="{MUTED}">'
  f'horizontal bars = min–max block latency across the 15 min. State root stayed 1.3–3.4 ms at every size.</text>')
a('</svg>')

out = sys.argv[1] if len(sys.argv) > 1 else "/tmp/frontier-long-chart.svg"
open(out, "w").write("\n".join(o))
print(f"wrote {out}\n")
print(f"{'gas':>6} {'txs/blk':>9} {'full':>6} {'latency':>9} {'range':>13} {'tps':>8} {'sd':>6}  binds")
for gas, txs, full, ms, lo, hi, tps, sd, b in ROWS:
    print(f"{gas:>5}M {txs:>9,} {full:>5}% {ms:>7} ms {f'{lo}-{hi} ms':>13} {tps:>8,} {sd:>5.1f}%  {'yes' if b else 'no'}")
