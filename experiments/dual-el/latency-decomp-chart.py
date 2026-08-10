#!/usr/bin/env python3
"""Block latency = EL(n) + consensus(n) — both LINEAR in transactions per block.

    python3 experiments/dual-el/latency-decomp-chart.py [out.svg]

AUTHORITATIVE for the latency-vs-block-size story (2026-08-09). Data = the clean drain
experiment: fresh 4-machine fleet chain per point, mempool pre-filled to 57-73k, ALL spam
stopped, height measured only over intervals where every block is >=95% full (v2 harness,
fleet/drain-test.py). Three drains per point, chain age 2.7-3.6k blocks at every drain —
this removes BOTH confounds that biased earlier numbers (delivery pacing made 100M read
829 ms sustained; chain-age churn made a 1.5h-old chain read 513 ms at 60M).

MEASURED (stock):   60M = 2,856 tx -> 295 ms (286-305, n=3)   9.7k tps
                   100M = 4,761 tx -> 407 ms (401-418, n=3)  11.7k tps
EL(n) from reth histograms at 100% full (consensus-split.sh, per-size exec+root+npOther):
                   1,190tx=42.1  2,371tx=71.3  4,761tx=127.2  9,517tx=210.1 ms
CONSENSUS(n) = drain height - EL(n).

THE MODEL (consensus fit as the residual against the EL line; passes through both drain points):
    EL(n)        ~= 18 ms  + 20.2 us/tx      (execute + state root + newPayload other)
    consensus(n) ~= 109 ms + 38.6 us/tx      (build+SSZ+stream+decode+2 vote rounds; every
                                              term touches all n txs -> linear, as expected)
    latency(n)   ~= 127 ms + 58.8 us/tx      => tps(n) = n / latency(n) -> ASYMPTOTE ~17k tps

CONSEQUENCES:
  * Bigger blocks DO buy tps at capacity (60M->100M: +21% tps for +37% latency) but with
    diminishing returns: the 128 ms fixed floor amortizes away while the 59 us/tx marginal
    cost never does. 200M predicts ~690 ms / ~13.8k tps; infinity = 17k tps.
  * At capacity the consensus marginal cost is ~36 us/tx — LOWER than the ~100 us/tx from the
    old sustained (delivery-confounded) runs; that number is SUPERSEDED. Agreeing on a tx
    still costs ~1.8x executing it.
  * Prebuild (ARC_SPECULATIVE_BUILD=1) at capacity: -8% latency at 60M (272 vs 295 ms),
    ~nil at 100M (410 vs 407) — the hidden build is part of the fixed+linear consensus term
    and other machines' work overlaps it as blocks grow.
  * Sustained fleet runs sit BELOW this frontier (60M: 534 ms sustained vs 295 capacity =
    1.8x) because spammer delivery (~5.3-5.8k tx/s) fill-paces blocks. Fixing delivery, not
    the chain, closes that gap.
"""
import sys

# n (txs/blk), measured drain height ms (stock), label
# 2026-08-10 LAW SWEEP: +3 fresh-chain sizes (3 drains each) — refit 110ms + 58.7us/tx,
# all residuals within 7% across 10x. 300M is the pooled estimate (few blocks/drain at that size).
DRAIN = [(1428, 181, "30M"), (2856, 295, "60M"), (4761, 407, "100M"),
         (7142, 501, "150M"), (14285, 955, "300M")]
SPEC  = [(2856, 272), (4761, 410)]          # prebuild arm
EL_A, EL_B = 18.1, 0.0202  # EL fit unchanged                    # ms, ms/tx
CN_A, CN_B = 91.9, 0.0385   # refit residual vs EL line (total: 110ms + 58.7us/tx, 5 sizes)
NMAX, YMAX = 15000, 1100
PRED = [(9517, "200M")]                      # model extrapolation

W, H = 1000, 620
L, R, T, B = 88, 300, 92, 96
PW, PH = W - L - R, H - T - B
INK, MUTED, GRID = "#1a1f2e", "#7b8394", "#e6e9ef"
C_EL, C_CN, C_TOT, C_SPEC = "#2563eb", "#dc2626", "#1a1f2e", "#15803d"

def X(n):  return L + n / NMAX * PW
def Y(ms): return T + PH - ms / YMAX * PH
def el(n): return EL_A + EL_B * n
def cn(n): return CN_A + CN_B * n
def tot(n): return el(n) + cn(n)

o = []; a = o.append
a(f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="{W}" height="{H}" '
  f'font-family="Inter,Helvetica,Arial,sans-serif">')
a(f'<rect width="{W}" height="{H}" fill="#ffffff"/>')
a(f'<text x="{L}" y="34" font-size="19" font-weight="700" fill="{INK}">'
  f'latency(n) = EL(n) + consensus(n) — both linear in transactions</text>')
a(f'<text x="{L}" y="56" font-size="12.5" fill="{MUTED}">'
  f'capacity (drain) measurements, fresh 4-machine fleet chains, 3 drains/point · 2026-08-09</text>')

for i in range(0, YMAX + 1, 100):
    y = Y(i)
    a(f'<line x1="{L}" y1="{y:.1f}" x2="{L+PW}" y2="{y:.1f}" stroke="{GRID}"/>')
    a(f'<text x="{L-10}" y="{y+4:.1f}" font-size="11" fill="{MUTED}" text-anchor="end">{i}</text>')
for n in range(0, NMAX + 1, 2000):
    a(f'<text x="{X(n):.1f}" y="{T+PH+20}" font-size="11" fill="{MUTED}" text-anchor="middle">{n:,}</text>')
a(f'<text x="{L+PW/2}" y="{T+PH+44}" font-size="12.5" font-weight="600" fill="{INK}" '
  f'text-anchor="middle">transactions per block (100% full)</text>')
a(f'<text x="{L-56}" y="{T+PH/2}" font-size="12.5" font-weight="600" fill="{INK}" '
  f'transform="rotate(-90 {L-56} {T+PH/2})" text-anchor="middle">block latency (ms)</text>')

# stacked areas: EL on the bottom, consensus above it
pts_n = list(range(0, NMAX + 1, 250))
area_el = " ".join(f"{X(n):.1f},{Y(el(n)):.1f}" for n in pts_n)
a(f'<polygon points="{X(0):.1f},{Y(0):.1f} {area_el} {X(NMAX):.1f},{Y(0):.1f}" '
  f'fill="{C_EL}" opacity="0.28"/>')
top = " ".join(f"{X(n):.1f},{Y(tot(n)):.1f}" for n in pts_n)
bot = " ".join(f"{X(n):.1f},{Y(el(n)):.1f}" for n in reversed(pts_n))
a(f'<polygon points="{top} {bot}" fill="{C_CN}" opacity="0.20"/>')
a(f'<polyline points="{area_el}" fill="none" stroke="{C_EL}" stroke-width="2"/>')
a(f'<polyline points="{top}" fill="none" stroke="{C_TOT}" stroke-width="2.5"/>')

# in-area labels
a(f'<text x="{X(6200):.1f}" y="{Y(el(6200)/2)+4:.1f}" font-size="12" font-weight="700" '
  f'fill="{C_EL}">EL: 18 ms + 20 µs/tx</text>')
a(f'<text x="{X(5600):.1f}" y="{Y(el(5600)+cn(5600)/2)+4:.1f}" font-size="12" font-weight="700" '
  f'fill="{C_CN}">consensus: 92 ms + 38.5 µs/tx</text>')

# measured drain points
for (n, ms, lab) in DRAIN:
    a(f'<circle cx="{X(n):.1f}" cy="{Y(ms):.1f}" r="6" fill="{C_TOT}"/>')
    a(f'<text x="{X(n)+10:.1f}" y="{Y(ms)-10:.1f}" font-size="11.5" font-weight="700" fill="{INK}">'
      f'{lab}: {ms} ms · {n:,} tx · {n/ms*1000/1000:.1f}k tps</text>')
for (n, ms) in SPEC:
    a(f'<circle cx="{X(n):.1f}" cy="{Y(ms):.1f}" r="4.5" fill="none" stroke="{C_SPEC}" stroke-width="2.2"/>')
a(f'<text x="{X(2856)+10:.1f}" y="{Y(272)+16:.1f}" font-size="10.5" fill="{C_SPEC}">'
  f'prebuild 272 ms (−8%)</text>')

# model extrapolation marker
for (n, lab) in PRED:
    if n <= NMAX:
        ms = tot(n)
        a(f'<circle cx="{X(n):.1f}" cy="{Y(ms):.1f}" r="5" fill="none" stroke="{C_TOT}" '
          f'stroke-width="1.8" stroke-dasharray="3 2"/>')
        a(f'<text x="{X(n)-10:.1f}" y="{Y(ms)-12:.1f}" font-size="10.5" fill="{MUTED}" '
          f'text-anchor="end">{lab} predicted: {ms:.0f} ms · {n/ms*1000/1000:.1f}k tps</text>')

# 500 ms line
a(f'<line x1="{L}" y1="{Y(500):.1f}" x2="{L+PW}" y2="{Y(500):.1f}" stroke="#15803d" '
  f'stroke-width="1.4" stroke-dasharray="7 4" opacity="0.8"/>')
a(f'<text x="{L+6}" y="{Y(500)-7:.1f}" font-size="10.5" font-weight="600" fill="#15803d">'
  f'2 blk/s — 500 ms (crosses at ~6,650 tx ≈ ~139M gas)</text>')

# right panel: consequences
lx = L + PW + 18
a(f'<text x="{lx}" y="{T+6}" font-size="12.5" font-weight="700" fill="{INK}">what the two slopes mean</text>')
lines = [
    ("latency(n) ≈ 110 ms + 58.7 µs/tx", INK, 700),
    ("(5 sizes, 10×, residuals ≤7%)", MUTED, 400),
    ("tps(n) = n / latency(n)", INK, 700),
    ("→ asymptote ≈ 17k tps", C_CN, 700),
    ("", INK, 400),
    ("bigger blocks amortize the", MUTED, 400),
    ("110 ms floor — never the", MUTED, 400),
    ("59 µs/tx marginal cost:", MUTED, 400),
    ("  30M   7.9k tps @ 181 ms", INK, 400),
    ("  60M   9.7k tps @ 295 ms", INK, 400),
    ("100M  11.7k tps @ 407 ms", INK, 400),
    ("150M 14.3k tps @ 501 ms", INK, 400),
    ("300M ~14.2-15.7k @ 0.9-1.0 s", INK, 400),
    ("   ∞     17k tps (asymptote)", MUTED, 400),
    ("", INK, 400),
    ("agreeing on a tx (38.5 µs)", MUTED, 400),
    ("costs ~1.9× running it (20 µs)", MUTED, 400),
    ("", INK, 400),
    ("sustained runs sit 1.6–1.8×", MUTED, 400),
    ("below this frontier: spammer", MUTED, 400),
    ("delivery paces the fill, not", MUTED, 400),
    ("the chain — fix delivery to", MUTED, 400),
    ("collect these numbers live", MUTED, 400),
]
for j, (t, c, w) in enumerate(lines):
    a(f'<text x="{lx}" y="{T+28+j*19}" font-size="11" font-weight="{w}" fill="{c}">{t}</text>')

a(f'<text x="{L}" y="{H-26}" font-size="10.5" fill="{MUTED}">'
  f'EL(n) from reth exec/root/newPayload histograms at 100% full; consensus(n) = measured drain '
  f'height − EL(n). Supersedes the 100 µs/tx consensus marginal from sustained (delivery-paced) runs.</text>')
a('</svg>')

out = sys.argv[1] if len(sys.argv) > 1 else "/tmp/latency-decomp-chart.svg"
open(out, "w").write("\n".join(o))
print(f"wrote {out}\n")
print(f"{'n':>7} {'EL':>7} {'consensus':>10} {'latency':>8} {'tps':>7}")
for n in (1190, 2371, 2856, 4761, 6300, 9517):
    print(f"{n:>7,} {el(n):>6.0f} {cn(n):>9.0f} {tot(n):>7.0f} {n/tot(n)*1000:>7,.0f}")
