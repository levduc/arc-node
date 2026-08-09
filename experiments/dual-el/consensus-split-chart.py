#!/usr/bin/env python3
"""Where a payment-lane block height goes, as the gas limit grows: execution vs consensus.

    python3 experiments/dual-el/consensus-split-chart.py [out.svg]

2026-08-09, 4-machine fleet, every point 100% full, demand tuned per size, ~6 min windows.
Figures averaged across the four validators (`fleet/consensus-split.sh`).

The height decomposes as:
    exec + root + np-other   -- inside newPayload; this is the EL
    remainder                -- next proposer builds the payload, SSZ encode, stream, decode
    vote gap                 -- newPayload done -> next forkchoiceUpdated. NOT idle: the other
                                validators are receiving/decoding/EXECUTING the same block here,
                                then two vote rounds. Quorum waits for the slowest.

TWO THINGS THIS SHOWS THAT SINGLE-SIZE MEASUREMENTS COULD NOT:

1. A FIXED CONSENSUS FLOOR of roughly 450 ms, independent of block size. Between 25M and 50M the
   transaction count doubles and the consensus time barely moves (464 -> 479 ms, 12.9 us per extra
   tx). That floor is why 2 blk/s is only reachable with small blocks: at a 500 ms target you are
   spending ~90% of the budget before the first transaction is executed.

2. ABOVE ~2,400 TRANSACTIONS the consensus cost starts scaling at ~100 us per transaction --
   4-6x the marginal cost of EXECUTING that same transaction (17-22 us). So each additional
   transaction costs several times more to agree on than to run.

Meanwhile the commitment gets CHEAPER per transaction as blocks grow: state root falls from
3.19 to 0.82 us/tx across an 8x range, and in absolute terms only moves 3.8 -> 7.8 ms.
"""
import sys

# gas(M), txs/blk, height_ms, exec, root, np_other, remainder, vote_gap
ROWS = [
    (25,  1190,  506,  32.2, 3.8,  6.1, 110.7, 353.4),
    (50,  2371,  551,  58.3, 5.8,  7.2, 127.8, 351.5),
    (100, 4761,  829, 109.7, 7.5, 10.0, 164.5, 537.5),
    (200, 9517, 1398, 191.1, 7.8, 11.2, 286.5, 901.3),
]
SEGS = [("execution", 3, "#2563eb"), ("state root", 4, "#0ea5e9"),
        ("newPayload other", 5, "#7dd3fc"), ("build + stream + decode", 6, "#f59e0b"),
        ("vote gap (peers execute + 2 rounds)", 7, "#dc2626")]

W, H = 1000, 600
L, R, T, B = 92, 210, 92, 108
PW, PH = W - L - R, H - T - B
INK, MUTED, GRID = "#1a1f2e", "#7b8394", "#e6e9ef"
YMAX = 1500
bw = PW / len(ROWS) * 0.52

def py(ms): return T + PH - ms / YMAX * PH
def bx(i):  return L + PW * (i + 0.5) / len(ROWS)

o = []; a = o.append
a(f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="{W}" height="{H}" '
  f'font-family="Inter,Helvetica,Arial,sans-serif">')
a(f'<rect width="{W}" height="{H}" fill="#ffffff"/>')
a(f'<text x="{L}" y="34" font-size="19" font-weight="700" fill="{INK}">'
  f'Where a block height goes: execution is 7–14%, consensus is the rest</text>')
a(f'<text x="{L}" y="56" font-size="12.5" fill="{MUTED}">'
  f'4-machine fleet · every point 100% full · averaged across all four validators</text>')

for i in range(6):
    v = YMAX * i / 5; y = py(v)
    a(f'<line x1="{L}" y1="{y:.1f}" x2="{L+PW}" y2="{y:.1f}" stroke="{GRID}"/>')
    a(f'<text x="{L-10}" y="{y+4:.1f}" font-size="11" fill="{MUTED}" text-anchor="end">{int(v)}</text>')
a(f'<text x="{L-62}" y="{T+PH/2}" font-size="12.5" font-weight="600" fill="{INK}" '
  f'transform="rotate(-90 {L-62} {T+PH/2})" text-anchor="middle">block height (ms)</text>')

# 500 ms target
a(f'<line x1="{L}" y1="{py(500):.1f}" x2="{L+PW}" y2="{py(500):.1f}" stroke="#15803d" '
  f'stroke-width="1.6" stroke-dasharray="7 4"/>')
a(f'<text x="{L+6}" y="{py(500)-7:.1f}" font-size="11" font-weight="600" fill="#15803d">'
  f'2 blk/s target — 500 ms</text>')

# the fixed consensus floor
a(f'<line x1="{L}" y1="{py(450):.1f}" x2="{L+PW}" y2="{py(450):.1f}" stroke="#dc2626" '
  f'stroke-width="1.3" stroke-dasharray="3 3" opacity="0.65"/>')
a(f'<text x="{L+PW-4}" y="{py(450)+14:.1f}" font-size="10.5" fill="#dc2626" text-anchor="end">'
  f'~450 ms fixed consensus floor (independent of block size)</text>')

for i, r in enumerate(ROWS):
    x = bx(i); base = 0.0
    for label, idx, col in SEGS:
        v = r[idx]
        a(f'<rect x="{x-bw/2:.1f}" y="{py(base+v):.1f}" width="{bw:.1f}" '
          f'height="{(py(base)-py(base+v)):.1f}" fill="{col}"/>')
        if v > 55:
            a(f'<text x="{x:.1f}" y="{(py(base)+py(base+v))/2+4:.1f}" font-size="10.5" '
              f'font-weight="600" fill="#ffffff" text-anchor="middle">{v:.0f}</text>')
        base += v
    cons = 100*(r[6]+r[7])/r[2]
    a(f'<text x="{x:.1f}" y="{py(base)-10:.1f}" font-size="11.5" font-weight="700" fill="{INK}" '
      f'text-anchor="middle">{cons:.0f}% consensus</text>')
    a(f'<text x="{x:.1f}" y="{T+PH+20}" font-size="12" font-weight="700" fill="{INK}" '
      f'text-anchor="middle">{r[0]}M</text>')
    a(f'<text x="{x:.1f}" y="{T+PH+36}" font-size="10.5" fill="{MUTED}" text-anchor="middle">'
      f'{r[1]:,} tx · {r[2]} ms</text>')

lx, ly0 = L + PW + 16, T + 10
a(f'<text x="{lx}" y="{ly0-8}" font-size="11.5" font-weight="700" fill="{INK}">height is made of</text>')
for j, (label, idx, col) in enumerate(reversed(SEGS)):
    y = ly0 + 12 + j*20
    a(f'<rect x="{lx}" y="{y-9}" width="12" height="12" fill="{col}"/>')
    a(f'<text x="{lx+17}" y="{y+1}" font-size="10.5" fill="{INK}">{label}</text>')

a(f'<text x="{lx}" y="{ly0+130}" font-size="11.5" font-weight="700" fill="{INK}">marginal cost</text>')
for j, (t, s) in enumerate([("per extra tx:", ""), ("50M→100M", "exec 21.5 vs cons 93 µs"),
                            ("100M→200M", "exec 17.1 vs cons 102 µs"),
                            ("", "agreeing costs 4–6× running")]):
    y = ly0 + 148 + j*17
    a(f'<text x="{lx}" y="{y}" font-size="10.5" fill="{INK if t else MUTED}">'
      f'{"<tspan font-weight=\"600\">"+t+"</tspan> " if t else ""}{s}</text>')

a(f'<text x="{L}" y="{H-30}" font-size="11" fill="{MUTED}">'
  f'State root shrinks per transaction as blocks grow: 3.19 → 0.82 µs/tx across the 8× range '
  f'(3.8 → 7.8 ms absolute). The commitment is never the constraint.</text>')
a('</svg>')

out = sys.argv[1] if len(sys.argv) > 1 else "/tmp/consensus-split-chart.svg"
open(out, "w").write("\n".join(o))
print(f"wrote {out}\n")
print(f"{'gas':>5} {'txs':>7} {'height':>8} {'EL':>7} {'consensus':>10} {'cons%':>6} "
      f"{'exec µs/tx':>11} {'root µs/tx':>11} {'cons µs/tx':>11}")
for g, tx, h, ex, rt, npo, rem, gap in ROWS:
    el = ex+rt+npo; cons = rem+gap
    print(f"{g:>4}M {tx:>7,} {h:>7}ms {el:>6.1f} {cons:>9.1f} {100*cons/h:>5.0f}% "
          f"{ex*1000/tx:>10.1f} {rt*1000/tx:>10.2f} {cons*1000/tx:>10.1f}")
