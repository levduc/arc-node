#!/usr/bin/env python3
"""Pick a heartbeat: latency-for-throughput with CLOSED-LOOP (pool-governed) load.

    python3 experiments/dual-el/paced-tradeoff-chart.py [out.svg]

2026-08-10, 4-machine fleet, one chain (runtime gas + block-time flips), 240s windows,
chain age 3.2-5.4k. Load = spam-fleet-distributed with --pool-target = 3 blocks' worth
(RATE=3500/spammer, safe to over-provision — the governor equilibrates). Pool stayed
healthy at EVERY point: queued = 0 throughout, pending pinned at target. See
fleet/LOADING.md for the technique; MISSION-EL.md iter 17-19 for its derivation.

POINTS (sustained, all blocks 100% full):
  target 500ms:  50M 518ms/4,591tps HELD · 100M 580ms/8,213 miss · 130M 659ms/9,388 miss
  target 1000ms: 100M 1,000ms/4,761 HELD · 200M 1,034ms/9,206 HELD · 300M miss (RANGE, see below)

REPRODUCED n=2 (iter 20, remote-spam-verified): every operating point within 1% across two fresh
chains (50M@500: 517-518ms/4,591-4,601 · 200M@1s: 1,034-1,039ms/9,166-9,206 · 100M unpaced:
537-550ms/8,656-8,864). 300M did NOT reproduce as a point: 1,348/10,595 vs 1,739/8,214 —
last-in-sequence both days (oldest chain, most state churn) and the second run's validator health
was degrading (alien2 OOM window). Quote 300M as 8.2-10.6k @ 1.35-1.74s, never one number.
  unpaced:       100M 550ms/8,656 (iter-18 reference)

READINGS:
1. The paced points trace the SAME diminishing-returns frontier as the drain model,
   displaced by the cost of concurrent ingress (~1.3-1.6x in height at 100M+).
2. At 2 blk/s the biggest sustainable block is 50M (4.6k tps) — the zero-ingress model
   said 132M, but sustained ingress moves the crossover down.
3. At 1 blk/s, 200M holds with margin: 9.2k tps at a clean 1.03s heartbeat — the
   latency trade buys ~2x the throughput of the 500ms promise.
4. The governor makes every point boring: no collapse, no fill-pacing ambiguity —
   fullness + healthy pool at every size for the first time.
"""
import sys

# (latency_ms, tps, label, held: True/False/None[unpaced], anchor)
PTS = [
    (518, 4591, "50M @ 500ms", True,  "e"),
    (580, 8213, "100M (misses 500ms)", False, "e"),
    (659, 9388, "130M (misses 500ms)", False, "e"),
    (1000, 4761, "100M @ 1s", True, "e"),
    (1034, 9206, "200M @ 1s", True, "e"),
    (550, 8656, "100M unpaced", None, "w"),
]
# 300M reproduced at 1,739ms/8,214 on an older chain with a degrading validator (iter 20):
# quote as a RANGE, not a point — plotted at both measurements, joined.
PTS.append((1348, 10595, "300M: 8.2-10.6k @ 1.35-1.74s (age-sensitive)", False, "e"))
PTS.append((1739, 8214, "", False, "e"))
RANGE_300M = ((1348, 10595), (1739, 8214))

EL_A, EL_B = 127.0, 0.0588   # capacity model latency(n) = 127ms + 58.8us/tx (drain-fit)

W, H = 1000, 620
L, R, T, B = 88, 60, 92, 96
PW, PH = W - L - R, H - T - B
XMAX, YMAX = 1400, 14000
INK, MUTED, GRID = "#1a1f2e", "#7b8394", "#e6e9ef"

def X(ms): return L + ms / XMAX * PW
def Y(t):  return T + PH - t / YMAX * PH

o = []; a = o.append
a(f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="{W}" height="{H}" '
  f'font-family="Inter,Helvetica,Arial,sans-serif">')
a(f'<rect width="{W}" height="{H}" fill="#ffffff"/>')
a(f'<text x="{L}" y="34" font-size="19" font-weight="700" fill="{INK}">'
  f'Pick a heartbeat: block latency buys throughput — governed load, chain untouched</text>')
a(f'<text x="{L}" y="56" font-size="12.5" fill="{MUTED}">'
  f'4-machine fleet · every point 100% full with a healthy pool (queued = 0) · pool-governed spam · 2026-08-10</text>')

for t in range(0, YMAX + 1, 2000):
    y = Y(t)
    a(f'<line x1="{L}" y1="{y:.1f}" x2="{L+PW}" y2="{y:.1f}" stroke="{GRID}"/>')
    a(f'<text x="{L-10}" y="{y+4:.1f}" font-size="11" fill="{MUTED}" text-anchor="end">{t:,}</text>')
for ms in range(0, XMAX + 1, 200):
    a(f'<text x="{X(ms):.1f}" y="{T+PH+20}" font-size="11" fill="{MUTED}" text-anchor="middle">{ms}</text>')
a(f'<text x="{L+PW/2}" y="{T+PH+44}" font-size="12.5" font-weight="600" fill="{INK}" '
  f'text-anchor="middle">block latency (ms)</text>')
a(f'<text x="{L-56}" y="{T+PH/2}" font-size="12.5" font-weight="600" fill="{INK}" '
  f'transform="rotate(-90 {L-56} {T+PH/2})" text-anchor="middle">sustained throughput (tx/s)</text>')

# capacity frontier (drain model, zero ingress): parametric in n
pts = []
for n in range(500, 20001, 250):
    lat = EL_A + EL_B * n
    if lat > XMAX: break
    pts.append(f"{X(lat):.1f},{Y(n / lat * 1000):.1f}")
a(f'<polyline points="{" ".join(pts)}" fill="none" stroke="{MUTED}" stroke-width="1.6" '
  f'stroke-dasharray="6 4"/>')
a(f'<text x="{X(1160):.1f}" y="{Y(11600)-8:.1f}" font-size="10.5" fill="{MUTED}">'
  f'zero-ingress capacity (drain model, asymptote 17k)</text>')

# 300M range connector
(x1, y1), (x2, y2) = RANGE_300M
a(f'<line x1="{X(x1):.1f}" y1="{Y(y1):.1f}" x2="{X(x2):.1f}" y2="{Y(y2):.1f}" '
  f'stroke="#dc2626" stroke-width="1.4" stroke-dasharray="3 3" opacity="0.7"/>')

# heartbeat verticals
for ms, lab in [(500, "2 blk/s"), (1000, "1 blk/s")]:
    a(f'<line x1="{X(ms):.1f}" y1="{T}" x2="{X(ms):.1f}" y2="{T+PH}" stroke="#15803d" '
      f'stroke-width="1.3" stroke-dasharray="7 4" opacity="0.7"/>')
    a(f'<text x="{X(ms)+5:.1f}" y="{T+14}" font-size="10.5" font-weight="600" fill="#15803d">{lab}</text>')

for (ms, tps, lab, held, anch) in PTS:
    if held is True:
        a(f'<circle cx="{X(ms):.1f}" cy="{Y(tps):.1f}" r="6" fill="#15803d"/>')
    elif held is False:
        a(f'<circle cx="{X(ms):.1f}" cy="{Y(tps):.1f}" r="5.5" fill="none" stroke="#dc2626" stroke-width="2.2"/>')
    else:
        a(f'<rect x="{X(ms)-5:.1f}" y="{Y(tps)-5:.1f}" width="10" height="10" fill="#2563eb"/>')
    dx = 10 if anch == "e" else -10
    ta = "start" if anch == "e" else "end"
    a(f'<text x="{X(ms)+dx:.1f}" y="{Y(tps)+4:.1f}" font-size="11" font-weight="600" fill="{INK}" '
      f'text-anchor="{ta}">{lab} — {tps:,}</text>')

lx, ly = L + 14, T + PH - 78
a(f'<rect x="{lx-8}" y="{ly-18}" width="270" height="86" fill="#ffffff" stroke="{GRID}"/>')
a(f'<circle cx="{lx+6}" cy="{ly}" r="6" fill="#15803d"/>')
a(f'<text x="{lx+18}" y="{ly+4}" font-size="10.5" fill="{INK}">holds its block-time target</text>')
a(f'<circle cx="{lx+6}" cy="{ly+22}" r="5.5" fill="none" stroke="#dc2626" stroke-width="2.2"/>')
a(f'<text x="{lx+18}" y="{ly+26}" font-size="10.5" fill="{INK}">misses target (natural cadence shown)</text>')
a(f'<rect x="{lx+1}" y="{ly+39}" width="10" height="10" fill="#2563eb"/>')
a(f'<text x="{lx+18}" y="{ly+48}" font-size="10.5" fill="{INK}">unpaced reference (iter 18)</text>')

a(f'<text x="{L}" y="{H-26}" font-size="10.5" fill="{MUTED}">'
  f'Operating points: 2 blk/s promise → 50M / 4.6k tps. 1 s heartbeat → 200M / 9.2k tps (2× the '
  f'throughput for 2× the latency). Gap to the dashed curve = cost of concurrent ingress.</text>')
a('</svg>')

out = sys.argv[1] if len(sys.argv) > 1 else "/tmp/paced-tradeoff-chart.svg"
open(out, "w").write("\n".join(o))
print(f"wrote {out}")
