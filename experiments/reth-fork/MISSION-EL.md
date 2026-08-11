# MISSION 2: optimise the payment-lane EL (CL/consensus path is OFF LIMITS)

**Hard constraint from the user: do NOT touch the CL / consensus path.** No changes to proposal
streaming, SSZ framing, voting, or `crates/malachite-app`. Everything must land in the EL —
`crates/evm`, `crates/execution-*`, `crates/evm-node`, or EL launch flags.

## 2026-08-11 — CLOSING VERDICT (user Q&A synthesis): no single culprit — a serial pipeline
plus a contention multiplier

Receipts commitment: dismissed by measurement (~0.3us/tx build + ordered in-memory root; both
commitments together <1us/tx). Final ranked ledger at 150M sustained (~103us/tx):
1. proposer build 14-20us/tx (6-10x contention-inflated; ~2us quiet)
2. slowest-peer validation 9-15us/tx (recovery 4-7 + exec 3.5-6 + receipts)
3. wire fan-out ~7us/tx (x3 unicast through ~470Mbps effective: Wi-Fi ceiling ~600 - overlay 15-25%)
4. decode/pool-maintenance/gossip residual ~5-10us/tx
5. state root + receipts root <1us/tx; fixed floor ~110ms (votes+scheduling).
ONE-SENTENCE VERDICT: execution and commitments are solved; the lane's remaining cost is a
serial pipeline of build, ship, and re-verify — every stage paying a contention tax for
sharing its machine with the ingestion firehose. NEXT DECOMPOSITION (this iteration): cpuset
experiment — is the build inflation core-scheduling or in-process pool-lock contention?

## 2026-08-11 — DISSEMINATION BANDWIDTH SPLIT (user Q&A follow-up, measured):
val1 -> papaduck, same host, two paths, 40MB x2 each:
- direct LAN IP: 576-627 Mbps (val1's WI-FI RADIO is the ceiling, not the gigabit wire)
- via tailscale: 423-524 Mbps (**userspace WireGuard tax ~15-25%** on top of the radio)
Ranking of fan-out costs: Wi-Fi radio first (~600 ceiling), overlay second (-20%), naive x3
unicast multiplication third. Earlier 209-434 Mbps figures included an extra SSH layer.
Practical order: wire the proposer-heavy machines (free 2-3x) > tailscale kernel/direct path >
protocol fixes (compact blocks / erasure coding) at this fleet size.

## 2026-08-11 — SUSTAINED PREBUILD A/B AT 150M: +2.8% ONLY — hit-starved by miss_parent;
the ~13% prize is real but gated on deeper pending-visibility work (honest negative)

Two fresh chains, same day/load (governed, 150M unpaced), all-4 arms, fork binary 3485bd62:
- STOCK: 729ms / 9,802 tps (builds 81.9-141.1ms — reconfirms the live-ingress build cost)
- SPEC:  709ms / 10,074 tps = **-2.7% height / +2.8% tps**
- WHY SO SMALL: **hits 6/35/3/7 vs miss_parent 100/69/101/99 per validator** (~5-35% hit rate;
  miss_timestamp ~0 — dual-timestamp still works). The chain-anchored pending-publish fix that
  gave 91-100% hits at 60M FAILS at 150M sustained: heavier newPayload(N) still races FCU(N-1)
  and the pending never surfaces to the watcher in time. ~840 speculative builds/validator/
  window (≈2.8/s) = harmless idle-CPU waste per design.
- VERDICT: prebuild's sustained value at the knee = +2.8% as-is. The ledger-implied ~13% is
  REAL (builds are 80-140ms live) but requires the next fork iteration on pending visibility
  (publish-on-any-valid-insert or equivalent) — recorded OPEN, not pursued tonight.
Both arms agreement OK; teardowns verified. Prebuild deck/docs status unchanged (flag stays
default-OFF; -8% @60M capacity remains the quotable win).

## 2026-08-11 — PER-STAGE LEDGER RUN (150M, both regimes, all 4 validators' histograms):
three results, one slide correction, one re-opened question

SUSTAINED (governed, 399 blocks @ 756ms, 100% full): build 96.6-143.2ms/blk (13.5-20us/tx) ·
newPayload 64.3-108.1ms (exec 60-95.5 + root 0.4-4.9) · FCU 1-2ms · txWait 3.8-7.0us/tx ·
txExec 3.6-5.7us/tx. DRAIN (same chain, quiet pool): build 9.9-18.0ms/blk (1.4-2.5us/tx!) ·
per-tx txWait/txExec UNCHANGED (3.5-7.0/3.5-4.7us).
1. **BUILD IS WHERE INGRESS LANDS: 6-10x more expensive under live ingress (96-143ms) than
   quiet-pool (10-18ms) for IDENTICAL 7,142-tx blocks** — pool lock/iterator contention inside
   the builder, not RPC/gossip CPU. RE-OPENS: the prebuild drain A/B (-8% @60M) measured
   against ~10ms quiet builds; sustained builds are ~100+ms → hiding them could be worth ~13%
   of a sustained height. TODO (justified next experiment): sustained-regime prebuild A/B.
2. **The 3-vs-20us/tx exec discrepancy resolved: histogram scope.** Per-tx executor+recovery
   cost is regime-INDEPENDENT (wait 3.7-7.0 + exec 3.5-5.7 ≈ 7-13us/tx); the old 20.2us/tx
   "EL slope" fit absorbed sustained-mode contention.
3. **SLIDE CORRECTION: the finalize-bars EL segments were empty-block-diluted at big sizes**
   (drain-window prom captures included post-drain racing empties). Honest EL = per-tx x n:
   ~13/26/43/64/129ms at 30/60/100/150/300M → consensus share 85-93%, not 93-96%. Slide fixed
   with method note; totals unchanged.
Agreement OK; teardown verified. Raw snaps /tmp/led_*.json.

## 2026-08-11 — LANE-COMPARE RERUN (user-directed, replaces the July 2h-window table):
both lanes loaded SIMULTANEOUSLY, 5-min window, 524 blocks each, fleet

| per-tx | EVM (1.6M-gas storage writes, 17.4 tx/blk) | PAY (transfers, 4,761/blk) | gap |
|--------|--------------------------------------------|----------------------------|-----|
| execution | 306.6us | 8.8us | 35x |
| state root | 391.1us | 0.2us | ~2,000x |
| persistence | 3,204us | 13.5us | 237x |

Same chain, same certificate, same window — only the workload differs. NOTES vs the old
10M-account 2h table (root 2.7ms/exec 3.2ms/persist 24ms per EVM tx): today's EVM per-tx costs
are lower because the guzzler mix (1.6M gas/tx, 17/blk vs ~5/blk) and the small fresh state
differ — provenance matters, both tables are real. Slide 16 is now a log-scale grouped bar
graph with today's numbers + config stated on-slide. Teardown verified.

## 2026-08-11 — LIVE-SUSTAINED GRID (one session, runtime gas flips, governed, unpaced):
the regular grid slide-15 needed — and a NEW SUSTAINED RECORD

| size | LIVE sustained | capacity (drain median) | live/capacity |
|------|----------------|--------------------------|---------------|
| 30M  | 217ms / 6,593  | 181 / 7,894  | 84% |
| 60M  | 352 / 8,104    | 293 / ~9,970 | 81% |
| 100M | 530 / 8,986    | 401 / 11,876 | 76% |
| 150M | 725 / 9,850    | 501 / 14,255 | 69% |
| 300M | 1,304 / **10,952** | 1,003 / 14,244 | 77% |
All windows 100% full, agreement OK, one chain (ages 4.4-9.1k across windows). FINDINGS:
1. **Live sustained keeps CLIMBING through 300M (10,952 = new sustained record)** while
   capacity plateaus at 150M — the ingress cost is partly fixed-ish per height, so bigger
   blocks amortize it too.
2. The live/capacity ratio dips at 150M (69%) — ingress hurts most right at the capacity knee.
3. Slide 15 rebuilt as grouped live-vs-capacity bars (5 sizes, latencies in tick labels) —
   same visual language as the finalize-time bars; three-tank methodology slide precedes both.

## 2026-08-11 — CAMPAIGN #3 (user-directed third confirmation): every size holds

| size | campaign-3 drains | cross-campaign median (n clean drains) |
|------|-------------------|----------------------------------------|
| 30M  | 181 / 216* / 174  | **181ms** (n=8 clean of 9) |
| 60M  | 286 / 281 / 286   | **286-295ms** (n=9) |
| 100M | 501* / 385 / 401  | **401ms** (n=8 of 9) |
| 150M | fail† / thin† / **501** | **501ms / 14.24-14.26k tps** (n=6, identical to the ms) |
| 300M | 1,337* / thin† / fail† | **1,003ms** (n=4 clean, campaigns 1-2) |
(* straggler episodes; † blast/backlog structural limits at big sizes: 50k backlog ≈ 7 blocks
@150M, 3.5 @300M, plus first-blast nonce-resync warmup and end-of-point dirty-nonce races.
HARNESS TODO recorded: scale blast target with block size.)
THE CEILING IS TRIPLE-CONFIRMED: 150M = 501ms at 14.24-14.26k tps in every campaign. Agreement
OK at every size in every campaign. Slide 14 (three-tank methodology) moved BEFORE the results
per user; live-sustained grid (slide-15 bars) auto-started next; lane-compare (slide-16 rerun)
queued behind it.

## 2026-08-10/11 — REPEAT CAMPAIGN (user-directed): every size re-measured with EL captured
on the SAME blocks; law + ceiling reproduce; slides rebuilt as EL-vs-consensus bars

Second full campaign (fresh chain per size, 3 drains each, prom exec+root per drain), joined
with campaign 1 => n=5-6 drains/size:
| size | txs | EL med | consensus | total med (all drains) | tps |
|------|-----|--------|-----------|-------------------------|-----|
| 30M  | 1,428 | ~13ms | ~168 | 181 (182/189/173 + 265*/181/181) | 7.9k |
| 60M  | 2,856 | ~15ms | ~278 | 293 (286/295/305 + 292/278/295) | 9.7k |
| 100M | 4,761 | ~33ms | ~368 | 401 (401/418/401 + 364/401/638*) | 11.9k |
| 150M | 7,142 | ~29ms | ~472 | 501 (501/601*/501 + 601*/501) | **14.3k** |
| 300M | 14,285| ~41ms | ~962 | 1,003 (1003/751†/1001 + 1004; 3 under-filled drains) | 14.2k |
(* straggler episodes — 1s stall blocks visible in spreads, excluded from medians, kept in the
record; † coarse mixed sample. 300M drains are structurally thin: 50k backlog ≈ 3.5 blocks.)
- **EVERY value reproduced across two campaigns days apart** (30M/60M/150M to the ms; 100M
  median identical; 300M identical). The 14.3k ceiling @150M is now n=4 clean drains.
- **CONSENSUS = 93-96% OF FINALIZE TIME AT EVERY SIZE**, EL 13-41ms flat-ish — measured on the
  same blocks, one campaign, answering "cost of consensus on bigger blocks" directly.
- Two more straggler episodes on record (R30d1 265ms, R100d3 638ms): occasional 1s round
  timeouts, plausibly wifi-alien2; median-of-drains is the robust estimator, episodes shown.
- Slides rebuilt per user: 13 = stacked EL/consensus bars (categorical sizes, no 10^4 axis);
  15 = minimal-text 3-regime plot (held menu / sustained unpaced / capacity stars 11.9k @100M,
  14.3k @150M), legend below the plot.

## 2026-08-10 — LAW SWEEP (user-directed): the law is now MEASURED at 5 sizes — refit
110ms + 58.7us/tx, every residual within 7% over a 10x range; ceiling measured 14.3k tps

Three NEW fresh-chain capacity points (3 drains each, drains FIRST, age 2.1-3.9k):
| gas | txs | height | tps | old 2-pt-line pred |
|-----|-----|--------|-----|--------------------|
| 30M | 1,428 | 181ms (173/182/189) | 7.9k | 211 (-14%) |
| 150M | 7,142 | 501ms (501/601*/501) | **14.3k** | 547 (-8%) |
| 300M | 14,285 | ~0.91-1.0s pooled (1003/751*/1001) | 14.2-15.7k | 967 (+~0%) |
(*one stall block in a tiny sample; >=150M drains hold only 4-8 blocks vs 44-53 at 30M — use
pooled/median, and note the 1s-sampling resolution limit at big sizes.)
REFIT on all five (30/60/100/150/300M): **latency(n) = 110ms + 58.7us/tx, residuals -7.0/+5.9/
+4.3/-5.7/+0.6%** — the law HOLDS across 10x; slope unchanged (58.7 vs 58.8), floor eases
127->110ms (the 2-pt fit leaned on the 60M point's high side). CONSEQUENCES:
- **Ceiling is now measured, not extrapolated: 14.3k tps @ 150M/501ms; ~14-16k @ 300M** =
  84-92% of the 17k slope-asymptote. 150->300M doubles latency for ~0-10% tps — the knee is
  ~150M at capacity.
- 2 blk/s crossing moves 132M -> ~139M gas (n≈6,650).
- The FRESH 300M point (~1.0s) replaces the age-smeared sustained range as the capacity number;
  the old 8.2-10.6k range stays valid for SUSTAINED/paced regime only.
- Consensus slope decomposition unchanged: ~38.5us/tx ≈ transport+decode (~18) + slowest-peer
  re-execution (~20); votes are hash-sized and contribute ~0 (user Q&A, slide 11 step-4 label).
Charts + slides 13/15 updated with measured points; agreement OK at every size; teardowns
verified. Chart: latency-decomp-chart.py (5-point banner).

## 2026-08-10 — PPROF FIXES PORTED + COMPILE-VERIFIED EVERYWHERE (user-directed)

The two heap-profiling fixes (b4df3e3 + c43527e) now live, cherry-picked cleanly and
`cargo check -p arc-node-execution --features pprof` PASSING, on:
- blockstm-native-transfers (origin of the fixes)
- fleet-multi-machine (tip c447cd9; check 5m10s clean)
- payment-lane-gas (tip 99db115; check 5m25s clean)
- pprof-heap-profiling-fixes (clean branch off origin/main for the upstream PR; patches +
  PR description also delivered as files in /tmp/pprof-fixes/)
MISSION 4 STATE: no open items. Prebuild frozen (-8%@60M capacity, nil@100M); EL levers all
closed; delivery closed-loop (governor); operating points n=3 <=1.3%; capacity model frozen
(127ms + 59us/tx, fresh-chain ceiling qualifier); endurance closed with fleet receipts;
pprof functional; bug ledger consolidated in CLAUDE.md.

## 2026-08-10 — FLEET ENDURANCE SOAK PASSES: the fix holds on real hardware — 15/15 slices,
20-block final agreement OK, all four validators alive the full 75 min

Fleet, 200M@1s, governed ~9.2k tps, rpc-cache caps 200/200 (the launcher default):
- **Chain: 1,008-1,057ms / 9,007-9,444 tps, 100% full EVERY slice, per-slice agreement OK,
  spread 0-1, final 20-block 4-way agreement OK.** 75 minutes at the recommended operating
  point with zero incidents — on the harness where pre-fix v4 died at slice 6.
- **Memory: converging sawtooth to a LATE plateau** — slices 13-15 near-flat on all four
  (final: v1 4.44 / v2 5.13 / v3 7.46 / v4 4.09 GiB). v4 (11 GiB cap, the pre-fix casualty)
  finished at 4.09 with three of its last five slices NEGATIVE. Fleet plateaus are higher and
  later than local (9.3k vs 7.8k tps, bigger pools, machine-dependent baselines — v3/papaduck
  always runs high); tail rate over the last 5 slices ~0-15 MiB/min/validator vs ~250 pre-fix.
- HONEST QUALIFIER: "plateau" on the fleet = converged-to-noise over the last 3 slices, not a
  75-min flat line; if a >6h burst ever matters, extend the soak then. For every realistic
  demo/benchmark window, memory is a solved problem.
ENDURANCE CHAPTER CLOSED end-to-end: symptom -> profiling fixed -> attribution -> stock-flag
cure -> local validation -> FLEET validation. Teardown verified 0 containers / 0 spammers on
all 4 machines (exact-name checks).

## 2026-08-10 — ITER 28-30: ENDURANCE FIX VALIDATED — 75-min soak at the 200M@1s operating
point, all four ELs PLATEAU at ~3.2 GiB, zero incidents

Single-machine (fleet blocked on tailscale-ssh re-auth — check expired overnight; the fleet
launcher ALSO got the caps: demo-fleet-metamask.sh has its OWN pay-EL docker run that
launch-payment-els.sh edits don't cover — patched on both branches BEFORE this run).
15 x 4-min slices, 4 governed spammers (~7.9k tps offered/landed), rpc-cache caps 200/200:
- **Chain: 1,176-1,270ms / 7.5-8.1k tps, 98-100% full, agreement OK on ALL 15 slices** —
  75 minutes at the operating point, zero stalls, zero divergence, no validator loss (pre-fix:
  val died in slice 6 on this exact harness at 6 GiB caps).
- **Memory: 2.2 -> 3.2 GiB decelerating sawtooth (250 -> 45 -> 22 -> ~15 MiB/min), then FLAT
  for the last 5 slices (3.16-3.29 GiB, all four ELs).** A genuine plateau; the level scales
  with block size (100M A/B plateaued 1.7 GiB — 200 cached blocks, 2x bigger at 200M).
- The "~30-40 MiB/min residual" from iter 27 was the cache still FILLING; converged residual
  ~0. Endurance at the operating point: bounded ~3.2 GiB vs 11 GiB fleet caps = indefinite.
REMAINING (one formality): re-run this soak on the FLEET after tailscale re-auth (one command;
launcher already carries the caps). Teardown verified 0/0.

## 2026-08-10 — ITER 27: ENDURANCE SOLVED — rpc-cache caps make payment-EL memory FLAT
(A/B: capped val1 plateaus at 1.7 GiB; stock peers pass 5.2 GiB still climbing)

18-min same-box A/B, ~5k tps governed load, val1 `--rpc-cache.max-blocks 200
--rpc-cache.max-receipts 200` vs val2-4 stock, per-minute memory:
- val1: 0.66 -> 1.72 GiB, **FLAT from minute 13** (last 5 samples 1.70-1.76 — a plateau, not a
  slower climb). The cache fills its 200 entries in ~2 min, then the process is bounded.
- val2-4: 0.65 -> 5.22-5.34 GiB at minute 18, still climbing ~230 MiB/min (headed for the cap).
- Agreement OK (the cache is read-path only; consensus untouched). Teardown verified.
CONSEQUENCES:
- **The 27-min OOM bound is GONE.** The 2026-08-09 memory model ("~350 MiB/min, not a leak, no
  config knob moves it") is SUPERSEDED: the knob existed, it just was never tested — the growth
  was reth's RPC eth cache LRU (entry-bounded) never filling at payment-lane block sizes.
- launch-payment-els.sh now defaults `--rpc-cache.max-blocks/max-receipts` to
  PAY_RPC_CACHE_BLOCKS/RECEIPTS (200/200). Long soaks/demos no longer need restart cadences on
  small-RAM validators (val4/alien2's 11 GiB cap fits with 9+ GiB headroom).
- Residual growth ~30-40 MiB/min at 5k tps remains (bodies in flight, pool, trie buffers) —
  plateau behavior suggests bounded; re-verify on the next fleet soak.
- The iter-21 soak numbers (healthy 1.03s/9.2k, dead-validator 1.85s/5.1k) stand; only the
  time-to-OOM bound is obsolete. Deck/docs updated.

## 2026-08-10 — ITER 26: MEMORY GROWTH ATTRIBUTED — 51% IS RETH'S RPC ETH CACHE (a stock flag
nobody ever tested)

Symbolized heap profile (profiling-profile binary, env-activated jemalloc prof, t17 snapshot at
5.07 GiB container RSS, ~7.0 GB sampled in-use):
- **51% (3.58 GB): `reth_rpc_eth_types::cache::cache_new_blocks_task` -> `ChainChange::new`** —
  the RPC eth-state cache CLONES every canonical block (RecoveredBlock, 2.30 GB) + its receipts
  (1.28 GB) into LRUs bounded by ENTRY COUNT (defaults ~5,000 blocks / 2,000 receipts), not bytes.
  With 4.7-9.5k-tx payment blocks, entry bounds = many GB by design. Node-local, zero consensus
  impact; explains "releases at ~40% when idle" (cache turnover) and why engine/persistence cache
  knobs did nothing — the RPC cache flags (`--rpc-cache.max-blocks/max-receipts/max-headers`)
  were never in those tests.
- Remainder: ~17% receipts to_vec (execute path), rest diffuse (bodies in flight, pool, trie).
LIVE A/B IN FLIGHT: val1 pay EL with `--rpc-cache.max-blocks 200 --rpc-cache.max-receipts 200`
vs 3 stock, per-minute memory trajectories. If val1's growth halves, the 27-min endurance bound
roughly doubles via a stock CLI flag.
Also fixed on the way (both committed): Arc pprof was dead-on-arrival from TWO bugs —
(1) malloc_conf exported unprefixed while tikv jemalloc reads _rjem_malloc_conf (b4df3e3);
(2) activation via raw mallctl bypassed jemalloc_pprof PROF_CTL's cached bookkeeping, so the
allocs handler always said "not activated" (c43527e). Workaround for old binaries:
_RJEM_MALLOC_CONF=prof:true,prof_active:true env.

## 2026-08-10 (night) — ITER 24/25 IN PROGRESS: jemalloc profiling was BROKEN IN ARC — found,
fixed, symbolized build running attribution

- Arc ships a full pprof stack (`--features pprof`: pprof_hyper_server on :6061,
  `/debug/pprof/allocs`, `--pprof.heap-prof`) — but it NEVER worked: the config static is
  exported as `malloc_conf` while tikv-jemalloc builds PREFIXED (`_rjem_malloc_conf`), so
  `opt.prof` stayed false and the allocs handler returned Err -> hyper closes the connection
  with an EMPTY REPLY (curl rc=52; the route 404s only when the feature is off — confusing).
  Runtime workaround: `_RJEM_MALLOC_CONF=prof:true,prof_active:true`. FIX committed b4df3e3:
  export the config under BOTH names. Verified end-to-end standalone: HTTP 200, valid pprof,
  go tool pprof parses.
- Release binaries are STRIPPED (workspace strip=true) -> profiles unsymbolized; the workspace
  already has `[profile.profiling]` (release+debug, strip=false) — use it for attribution
  builds (`cargo build --profile profiling -p arc-node-execution --features pprof`, in
  rust:1.93-bookworm for container GLIBC).
- LANDMINES (all cost a run each): (1) container-IP probing must skip the INTERNAL docker
  network (no host route; probe for the pprof 404 on '/', not TCP-connect); (2) `pkill -x
  arc-node-execution` matches NOTHING — kernel comm truncates names to 15 chars ("arc-node-
  execut"); kill by PID or ss -tlnp for port holders; an orphaned standalone node holding
  :6061 broke a demo boot (compose publishes 6061).
Attribution run (symbolized, t2/t17 snapshots under governed load) in flight.

## 2026-08-11 — NIGHT CONFIRMATION RUN (user-directed): every non-Arc deck number re-measured
on the fleet, one fresh chain, flips verified, remote spam verified, agreement OK

| point | frozen (n=2) | tonight (n=3) | verdict |
|-------|--------------|---------------|---------|
| unpaced 100M governed | 537-550ms / 8,656-8,864 | 531ms / 8,967 | CONFIRMED (range 531-550) |
| 50M @ 500ms | 517-518 / 4,591-4,601 HELD | 515 / 4,621 HELD | CONFIRMED (0.6% spread n=3) |
| 200M @ 1s | 1,034-1,039 / 9,166-9,206 HELD | 1,026 / 9,285 HELD | CONFIRMED (1.3% spread n=3) |
| 100M drain ceiling | 407ms / 11,699 (fresh, age ~2.9k) | 501ms / 9,505 (AGED ~5k, ran last, one 1s stall blk) | ceiling is FRESH-CHAIN; ages toward ~9.5k |

Per-tx (val1 prom, THIS run, 16k-account chain): 100M: exec 8.7us / root 0.17us / persist 13.3us;
200M: exec 8.5us / root 0.10us / persist 8.9us. Slide-15's table (root 2.5us, exec 13.5us) is from
the 10M-ACCOUNT chain (2h window) — state size explains the delta; keep 10M as the conservative
at-scale figure, cite tonight's as the small-state reconfirmation. Root per tx FALLS with block
size again (0.17 -> 0.10us).
RULE APPLIED: drain-last = aged measurement; the 11.7k ceiling now carries a "fresh-chain"
qualifier everywhere it appears. Teardown verified.

## 2026-08-10 — ITER 23: MEMORY GROWTH ATTRIBUTED — IT IS REAL ANONYMOUS HEAP, NOT PAGE CACHE

Single-machine attribution run (per-process phenomenon; fleet adds nothing): val1 payment EL under
governed load (~5-6k tps), 20 x 1-min samples of the container cgroup's memory.stat (anon vs file)
+ memory.current, alongside reth /metrics.
- **anon grows monotonically 0.46 -> 5.31 GiB (~0.24 GiB/min at this rate; scales with tps).**
- **file (MDBX page cache) is the RECLAIM buffer, not the growth:** once memory.current pinned at
  the 6 GiB cap, the kernel squeezed file 3.0 -> 0.4 GiB to make room for anon. OOM arrives when
  anon alone approaches the cap and there is no file left to give back.
- CONSEQUENCE: the cgroup-accounting hypothesis is DEAD — the 27-min OOM bound is real heap;
  docker memory tuning cannot fix it. Prior result stands: no reth config knob moves it either.
- reth_jemalloc_* gauges read absent in this build (0.0) — component attribution needs a
  jemalloc-profiling build (MALLOC_CONF prof:true + jeprof, or reth --debug jemalloc feature).
  That is the recorded next step for this thread if endurance ever becomes the binding product
  constraint; NOT pursued now (bounded iteration).
Teardown verified (0 containers).

## 2026-08-10 — ITER 21 COMPLETE: 75-MIN SOAK OF THE OPERATING POINT (200M @ 1s, governed) —
three numbers for the runbook (all 15 slices in; tail slices 10-15 flat at 1,832-1,890ms /
5,039-5,198 tps, degraded state fully stationary; teardown verified 0 containers / 0 spammers
exact-name-checked on all 4 machines)

Fresh fleet chain, BLOCK_TIME_MS=1000, PAY_GAS=200M, governed spam (POOL_TARGET=28569, RATE=3500),
5-min slices with per-slice 10-block agreement + per-EL memory:
1. **HEALTHY STEADY STATE: 1,026-1,053ms / 9,047-9,285 tps, 100% full, agree OK, spread 0-2**
   for slices 1-5 (~27 min) — the recommended point is stable while all validators live.
2. **TIME-TO-OOM BOUND: ~27 min at 9.2k tps on an 11 GiB-capped payment EL.** All ELs grow
   ~0.4-0.6 GiB/min under this load (v1 3.3->10.6, v2 4.3->12.9, v3 6.6->16.7 GiB over 5 slices);
   v4/alien2 (11 GiB cap) died mid-slice-6 exactly on schedule. Predictable, linear, no plateau.
3. **DEGRADED STEADY STATE (3-of-4, v4 dead): 1,846-1,875ms / ~5,100 tps, flat across slices 7-9.**
   A dead validator costs ~45% of throughput at this point, NOT 25% — its round-robin proposer
   slots burn full round timeouts every 4th height (compounds, then stabilizes by slice 7).
OPS RULE: on an 11-16 GiB validator, either budget ~0.5 GiB/min of continuous 9k-tps burst
(~27 min on 11 GiB) and schedule EL restarts, or cap the sustained rate. Big-RAM validators
(62-78 GB) never approached limits (v3 at 21.5 GiB and climbing linearly, fine).
Agreement checks: OK every slice until v4's RPC died (then UNREACHABLE by construction — the
3-live lockstep continued; production never stalled => no fork).

## 2026-08-10 — ITER 20: PACED SWEEP REPRODUCED n=2 (user-directed, censused fleet load) —
operating points solid within 1%; 300M requalified as a RANGE

Re-ran all 7 points on a fresh fleet chain after tailscale re-auth, with per-point remote-spam
verification (forensics first: yesterday's sweep WAS fleet-fed — remote log mtimes/content match
every window; tailscale expired only overnight).

| point | yesterday | today | delta |
|-------|-----------|-------|-------|
| 100M unpaced | 550ms/8,656 | 537ms/8,864 | 2.4% |
| 50M @500ms | 518/4,591 HELD | 517/4,601 HELD | 0.2% |
| 100M @500ms | 580/8,213 miss | 574/8,292 miss | 1% |
| 130M @500ms | 659/9,388 miss | 686/9,027 miss | 4% |
| 100M @1s | 1,000/4,761 HELD | 1,008/4,721 HELD | 1% |
| 200M @1s | 1,034/9,206 HELD | 1,039/9,166 HELD | 0.5% |
| 300M @1s | 1,348/10,595 | 1,739/8,214 | **±25%** |

- **OPERATING POINTS ARE FROZEN, n=2 <=1%:** 2 blk/s -> 50M/4.6k · 1s -> 200M/9.2k HELD.
- **300M REQUALIFIED: 8.2-10.6k @ 1.35-1.74s, NOT a record point.** Last-in-sequence both days
  (oldest chain, most state churn per block) AND today's window is additionally suspect: the
  post-sweep agreement check hit Connection refused on a validator RPC (val4/alien2 OOM window —
  ~55min loaded run ending at 300M) — a dying validator drags heights (known confound). Yesterday's
  300M had verified agreement; today's formal check did not complete (production never stalled ->
  no fork, but the 20-block check is missing). Do not quote a single 300M number.
- GOTCHAS: remote spammer binary is /tmp/arc-spammer -> `pgrep -x spammer` counts 0 on remotes
  (census read DEGRADED spuriously all run; verify via spam-log mtime/content instead, or
  pgrep -f arc-spammer). Big-block beyond-frontier points should be measured FIRST in a sequence,
  not last, and with mem-soak alongside.
Teardown verified (0/0 all machines).

## 2026-08-10 — ITER 19: PACED SWEEP (latency-for-tps, governed load) — 1s heartbeat holds 9.2k tps

User-directed: hold a block-time target, sweep gas, governed spam (POOL_TARGET=3 blocks' worth,
RATE=3500/spammer). One fleet chain (1G genesis cap, runtime gas+pacer flips, quiesce->apply->verify
per point), 240s windows, chain age 3.2-5.4k, ALL points 100% full with queued=0:

| target | gas | height | tps | held? |
|--------|-----|--------|-----|-------|
| 500ms  | 50M | 518ms | 4,591 | HELD |
| 500ms  | 100M| 580ms | 8,213 | miss |
| 500ms  | 130M| 659ms | 9,388 | miss |
| 1000ms | 100M| 1,000ms | 4,761 | HELD (exact) |
| 1000ms | 200M| 1,034ms | 9,206 | HELD |
| 1000ms | 300M| 1,348ms | 10,595 | miss — SUSTAINED RECORD |

FINDINGS:
1. **Operating points:** 2 blk/s promise -> 50M/4.6k. 1s heartbeat -> **200M/9.2k HELD** (2x tps for
   2x latency). Record sustained 10.6k @ 1.35s (300M).
2. **Sustained crossover for 500ms is ~50-60M, NOT the zero-ingress model's 132M** — concurrent
   ingress displaces the frontier (100M runs 580ms sustained vs 407ms drain).
3. Paced points run PARALLEL to the drain frontier; horizontal gap = ingress cost, roughly constant
   ratio 1.3-1.6x at 100M+.
4. Governor held every point healthy (pool pinned at target, queued 0 even at 46.7k backlog for
   300M) — first sweep ever with neither fill-pacing nor collapse ambiguity anywhere.
Agreement OK; teardown verified. Chart: experiments/dual-el/paced-tradeoff-chart.py. Deck: two new
frames (load technique + pick-a-heartbeat). Governor + flags ported to fleet-multi-machine
(cf6c530); LOADING.md on both branches.

## 2026-08-10 — ITER 18 COMPLETE: POOL GOVERNOR WORKS — sustained record 8,656 tx/s; the residual
gap to capacity is CONCURRENT-INGRESS cost, not delivery

Fleet 100M unpaced, fresh chain, RATE=2500/spammer (the config that collapsed to 3.2k open-loop),
--pool-target 12000 on all 16 spammers, 300s window, chain age 3000 (comparable to the drain runs):
**546 blocks, height 550ms, landed 8,656 tx/s, 99% full, pool pending 8.9k/14.2k/21.3k
(min/med/max), queued 0/0/138.** Agreement OK. Teardown verified.

1. **The governor kills the cliff.** Same RATE: open-loop 3,240 -> governed 8,656 tx/s. queued~=0
   the whole window = the eviction/nonce-gap poisoning never formed -> iter-17b's mechanism is
   confirmed by its controlled absence. Overshoot above target (max 21k) is the 250ms poll lag;
   harmless.
2. **New sustained record: 8,656 tx/s @ 550ms** (prior best 8,094 @ 588; pre-iter-17 baseline
   6,827 @ 697). RECOMMENDED LOAD CONFIG: POOL_TARGET=12000, RATE high (2500), S=4/machine —
   RATE is now safe to over-provision; the governor finds the equilibrium.
3. **Delivery is EXHAUSTED as a lever.** Pending sat ABOVE target most of the window (senders
   paused) — the pool was never starved and never poisoned, yet the chain ran 550ms, not the
   407ms drain height at the same age. The residual sustained-vs-capacity gap (8.7k vs 11.7k,
   550 vs 407ms) is the cost of CONCURRENT INGRESS (RPC admission + gossip + pool maintenance
   while consenting). That work rides on the EL/CL boxes during every height; it is not spammer
   tooling and not EL execution. Closing it would need ingress isolation (e.g. dedicated RPC/
   admission cores or nodes) — out of scope for this mission's constraint.
FINAL SUSTAINED LADDER (fleet, 100M, honest): 6.8k (pre-17 config) -> 8.1k (RATE=1000 open loop)
-> 8.65k (governed; ceiling with concurrent ingress) -> 11.7k (drain = zero-ingress capacity).

### Landmine log (from the in-progress entry)

Built the closed-loop spammer governor (--pool-target, commit d67f106): background task polls
txpool_status (250ms) into an AtomicU64; RateLimiter::wait() pauses while pending+queued > target.
Covers both send modes; 74 tests pass (also fixed pre-existing spammer test-build breakage).

LANDMINE (cost one void fleet window): `target/release/spammer` on this box was built from a
DIFFERENT branch state — commit 420eb22 (--chain-id flag) was never on blockstm-native-transfers.
Rebuilding the spammer from THIS branch silently produced a binary without --chain-id; every
spammer then died at arg parse ("unexpected argument '--chain-id'") and the governed window
measured an empty chain (0 tx — voided, obvious). All EARLIER measurements are unaffected (they
used the old binary, which had the flag). FIX: cherry-picked 420eb22 (c9ad442), merged with the
governor changes, 74 tests pass, full fleet arg shape smoke-tested.
RULE: after rebuilding any harness binary, smoke the EXACT arg string the scripts use before a
fleet run; a branch rebuild can silently drop flags the harness depends on.

## 2026-08-09 — ITER 17b: OVER-OFFERING HAS A CLIFF, NOT A PLATEAU — RATE sweep 1500/2500 COLLAPSED

Follow-up sweep (fresh fleet chain, 100M unpaced, backpressure mode, S=4/machine):
| RATE/spammer | offered | landed | txs/blk | %full |
|--------------|---------|--------|---------|-------|
| 1000 (17a)   | 16k     | **8,094** | 4,761 | 100%  |
| 1500         | 24k     | 3,124  | 203     | 0%    |
| 2000         | 32k     | 3,832  | 514     | 3%    |
| 2500         | 40k     | 3,240  | 268     | 2%    |

ALL >=1500 windows collapsed the same way on ALL 4 machines: "txpool is full" rejecting ~50-70% of
sends while val1 pending sat at only ~214-8.7k and blocks starved. MECHANISM (hypothesis, strongly
consistent, not yet instrumented): sustained offer above consumption drives the pool to its cap;
eviction at cap breaks per-account nonce continuity; the pool fills with QUEUED (non-executable)
txs; pending starves; blocks go near-empty; consumption falls; spiral. Backpressure's transient
backoff (100ms per txpool-full) then throttles senders to ~300-400 tx/s each.

CONSEQUENCES:
- **Sustained optimum measured so far: RATE=1000/spammer x 16 = 8.1k landed, 100% full, 588ms**
  (= fill-paced: 4,761/8,094). The 11.7k capacity frontier is NOT reachable sustained with
  open-loop spammers: below the cliff you are fill-paced, above it the pool implodes.
- The right spammer design is a CLOSED LOOP: govern offered rate on pool depth (target pending
  ~= 2-3 blocks worth, ~10-15k), not a fixed RATE. Next iteration: pool-depth governor in
  sender.rs (poll txpool_status, pause above target) — tooling only, no chain change.
- RULE: between sequential load windows on one chain, verify pending AND QUEUED both drained —
  a poisoned queued subpool silently ruins the next window (sleep-90 settle was not enough;
  R2000/R2500 may partly inherit R1500's poisoning; R1500 itself collapsed fresh).
- NOT yet instrumented: pending+queued sampled during the ramp (would confirm the spiral
  directly). Do that before trusting any mechanism refinement.
Agreement OK all windows (consensus never at risk). Full teardown verified.

## 2026-08-09 — ITER 17: THE DELIVERY GAP IS THE SPAMMER'S RATE CONFIG; fire-and-forget is DEAD

Target: close the sustained-vs-capacity gap (1.6-1.8x) from the delivery side (tooling only, no
chain change). Fleet A/B at 100M unpaced, same chain, S=4 ACCTS=1000 per machine:

1. **BACKPRESSURE @ RATE=1000/spammer (16k offered): 8,094 tx/s landed, 100% full, 588ms.**
   The historical ~5.3-5.8k "delivery plateau" was substantially our own RATE settings (spam-fleet
   scripts defaulted RATE=400 => 6.4k offered ceiling). 588ms == 4,761/8,094 exactly = still
   FILL-PACED (pool balances at delivery; builder waits) — capacity is 407ms/11.7k, so RATE was
   raised further (sweep 1500/2000/2500 in flight when this entry was written).
2. **FIRE-AND-FORGET (--fire-and-forget, existed in the binary, never used by our scripts):
   MEASURED DEAD for sustained load — 2,787 tx/s, 193-tx blocks, 0% full.** Mechanism from the
   spam logs: FF's optimistic nonces have no repair path; ~16 early "nonce too low" rejections
   permanently nonce-gapped those accounts, everything behind them QUEUED (not pending), queue
   hit the 200k pool cap, then "txpool is full" rejected ~87% of sends INCLUDING gap-fillers.
   Steady state: blocks starve at ~193 txs while the spammer reports 1000 tx/s sent. This is
   precisely why backpressure mode exists. Do NOT revisit without rejection-aware nonce repair.
   (FF=1 env wired into spam-fleet-distributed.sh for reproduction; default stays backpressure.)
3. Agreement OK after both arms; full clean-fleet teardown verified.

RULE reinforced: "delivery plateau" numbers are properties of the LOAD CONFIG, not the chain —
always state offered rate + mode alongside landed rate.

## 2026-08-09 — THE CLEAN DRAIN EXPERIMENT (iteration 16): numbers FROZEN

Four fresh fleet chains (60M/100M x stock/prebuild), fork binary 3485bd6253b7ad67 everywhere,
v2 drain harness (content-verified), 3 drains per point, chain age 2.7-3.6k blocks at every
drain, early 20-block 4-way agreement OK on every chain. Harness: /tmp/one_point.sh pattern
(boot -> settle -> blast w/ pool-growth guard >10k/20s -> stop all spam -> drain-test.py).

| point       | height at capacity (ms) | tps at capacity |
|-------------|-------------------------|-----------------|
| 60M  stock  | 295 (286/295/305)       | 9,700           |
| 60M  spec   | 272 (264/273/278)       | 10,500          |
| 100M stock  | 407 (401/418/401)       | 11,700          |
| 100M spec   | ~410 (418/401 + one 501 stall outlier) | 11,600 |

FINDINGS (supersede all earlier capacity estimates):
1. Capacity headroom vs sustained CONFIRMED at 1.6-1.8x (60M: 534->295; 100M: ~650-700->407).
   The 1.05-2.3x uncertainty is closed. Sustained runs are delivery-paced (spammer ~5.3-5.8k/s).
2. PREBUILD AT CAPACITY: -8% height / +8% tps at 60M (real, 3v3 drains, non-overlapping ranges);
   NIL at 100M — the hidden build is overlapped by peers' growing execute+vote work.
3. LINEAR MODEL (fits both stock points exactly): EL(n) ~ 18ms + 20.2us/tx (from consensus-split
   histograms at 100% full); consensus(n) = drain - EL ~ 109ms + 38.6us/tx; total
   latency(n) ~ 127ms + 58.8us/tx  =>  tps asymptote ~17k. 200M predicted ~690ms / 13.9k tps.
   At capacity, agreeing on a tx (39us) costs ~1.9x executing it (20us) — NOT the 4-6x from
   sustained sweeps (those were delivery-confounded; superseded).
4. 2 blk/s budget crosses the model at n~6,300 tx ~ 132M gas.
Chart: experiments/dual-el/latency-decomp-chart.py (AUTHORITATIVE for latency-vs-size).
Deck updated (new "law of the lane" frame; anatomy + lever rows corrected), pushed to Overleaf.

## Why this mission exists (and the mistake that motivated it)

Mission 1 shipped a native-transfer fast path (exec 11.03 -> 7.00 us/tx, 1.58x) and then measured
"EL = 10% of block time, CL = 90%". **That 10/90 split is WRONG in the EL's favour**: it was computed
as `block_time - newPayload_elapsed`, but `newPayload_elapsed` is reth's INTERNAL timer, which starts
*after* the engine-API request is deserialised.

The payment EL is driven over **authrpc HTTP JSON-RPC** (`--authrpc.port=8551`, see
`launch-payment-els.sh`), while the EVM lane uses **IPC**. So every `engine_newPayloadV3` for the
payment lane arrives as JSON with all 47,618 transactions as hex strings — roughly **12 MB of JSON
per block** to parse, hex-decode and re-encode into reth types. That work is **inside the EL** but
**outside** the 357 ms I attributed to it. It was filed under "consensus overhead". It is not.

## Definition of done

1. The EL-side cost of ingesting a full 1-Ggas payload is **measured and attributed** (not inferred).
2. At least one EL-only change lands that measurably cuts payment-lane block cost, verified by the
   usual gates.
3. All 4 validators still agree on the payment-lane state root for 100+ consecutive blocks under load.
4. Nothing regressed: EVM lane stock, block production healthy.

## Ranked hypotheses (measure before optimising — this is the whole point)

1. **Engine-API ingestion (UNMEASURED, top suspect).** Gap between the CL issuing `newPayload` and
   reth's internal timer starting. Fixes are EL-side: switch the payment lane to **IPC** (the EVM
   lane already does this — `Engine::new_ipc` in `malachite-app/src/config.rs:54`, selected by
   endpoint config, NOT a CL code change), and/or a cheaper JSON path.
   NOTE: choosing IPC vs HTTP is a *configuration* choice, so it stays inside the constraint.
2. **Persistence** — 116-190 ms/block, the largest measured EL cost after execution. Async, but it
   caps sustainable cadence.
3. **State root** — 5-15 ms. The frozen-root idea lives here. Small.
4. **More execution parallelism** — the verified 4.25x scheme on top of the fast path. Least
   valuable: execution is already the smallest term.

## Verification protocol (unchanged, mandatory)

- Offline gates, BOTH must print IDENTICAL before any deploy:
  `cargo run --release -p arc-evm --example parallel_transfer_bench`
  `ARC_PARALLEL_TRANSFERS=1 cargo run --release -p arc-evm --example parallel_transfer_bench`
- Live: all validators must agree on the payment-lane state root at every height. Divergence halts
  consensus, so a chain that keeps advancing under load is the proof.
- Perf numbers are **state-dependent** (mission 1 saw 7.00 -> 9.08 us/tx purely from chain growth).
  Only compare runs at comparable chain age; `ab-fastpath.sh` starts both arms from a fresh chain.

## Status

- [x] **RECON DONE (iteration 1).** Established, from source:
      * reth's `new_payload_v3` latency metric starts INSIDE the handler
        (`rpc/rpc-engine-api/src/engine_api.rs:220`), i.e. AFTER jsonrpsee deserialises the params.
        The `Block added to canonical chain elapsed=` log is engine-tree time, also post-parse.
        **So neither existing metric sees the ~12 MB JSON parse — it is genuinely unmeasured.**
      * The CL has NO engine-call duration metric (`malachite-app/src/metrics/app.rs` has
        block_time / block_build_time / block_size_bytes but nothing per engine call), and adding
        one would touch the CL => OFF LIMITS.
      * **The payment lane ALREADY SUPPORTS IPC**: `config.rs` builds `EngineConfig::Ipc` when
        `payment_eth_socket` + `payment_execution_socket` are set, and that branch takes PRIORITY
        over the RPC branch. The EVM lane already runs IPC; the payment lane runs authrpc HTTP only
        because that is how `launch-payment-els.sh` / the fleet `payment_el_cmd` configure it.
        **=> switching the payment lane to IPC is CONFIG-ONLY and inside the constraint.**
      CONSEQUENCE: rather than instrument the parse (which needs CL changes), run a TRANSPORT A/B —
      HTTP vs IPC, same everything else — and read the difference in block time / cadence. That
      measures the ingestion cost end-to-end without touching consensus.
- [x] **PER-BLOCK READ CACHES LANDED + VERIFIED (iteration 2, commit below).** Took the state-read
      win WITHOUT batching — ~40 lines instead of ~250. Cached the 3 block-constant reads:
      blocklist status (memoised map; NATIVE_COIN_CONTROL is not written by transfers) and the fee
      beneficiary (full AccountInfo tracked in memory). Both INVALIDATED on any general-EVM tx.
      **5 state reads/transfer -> 2.**
      * Gates: BOTH IDENTICAL on both workloads.
      * Offline executor path: pool 107.7 -> 64.1 -> **54.1 ms**, closed 94.9 -> 58.4 -> **45.4 ms**
        (stock -> fast path -> +caches) = **~2.0x vs stock**, 1.18-1.29x from the caches alone.
      * Live 4-validator demo at 1 Ggas: **122 consecutive heights (154..275), ZERO divergence**;
        exec 89.2 ms @ 5,079 txs/blk = **17.6 us/tx**, vs a prior stock run at a comparable
        5,363 txs/blk = 22.6 us/tx (~1.29x). CAVEAT: those two live runs were not a controlled A/B
        (different chain instances); the controlled evidence is the offline number. Use
        `ab-fastpath.sh` for a rigorous live claim.
      * CORRECTNESS BUG CAUGHT PRE-TEST: the beneficiary cache must hold the FULL AccountInfo —
        rebuilding a default would reset its nonce/code_hash in the state diff and diverge the root.
- [x] **MEASURED (iteration 3): BATCH EXECUTION IS THE WRONG NEXT TARGET — deprioritised.**
      After the fast path + per-block caches, the ~54 ms executor path (47,618 transfers) splits as:
        * 2 state reads/tx ......... **7.2 ms (13%)**  <- all that batch-prefetching could attack
        * pure arithmetic .......... 4.6 ms (9%)
        * **receipts + State/bundle commit ... ~42 ms (78%)**  <- THE REMAINING COST
      So the ~250-line, consensus-critical batch-execution change would chase 13% (and save maybe
      6 ms of it). The caches already took the cheap read win; reads are no longer the problem.
      Probe lives in `parallel_transfer_bench.rs` (prints a "decomposition" block) so this is
      re-checkable after any change.
- [x] **COMMIT-ONCE-PER-BLOCK LANDED + VERIFIED (iteration 4).** Per-block write overlay replaces
      per-tx `db.commit`. Risk settled FIRST by a SAFETY PROBE (500 txs over 64 repeatedly-touched
      accounts, both ways): full BundleState compared — **plain state IDENTICAL, reverts IDENTICAL**.
      That was the check the normal gates could not do (a revert bug breaks reorg unwinding without
      moving the state root).
      * Both gates IDENTICAL on both workloads.
      * Offline executor path, cumulative (pool / closed):
        stock 107.7/94.9 -> fast path 64.1/58.4 -> +caches 54.1/45.4 -> **+overlay 45.3/35.3 ms**
        = **2.23x / 2.51x vs stock**; 1.19x / 1.29x from the overlay alone.
      * Live 4-validator demo at 1 Ggas: **123 consecutive heights (201..323), ZERO divergence**.
        exec 71.2 ms @ 4,254 txs/blk = **16.7 us/tx**; block rate **1.61 blk/s** (previous iteration:
        17.6 us/tx, 1.33 blk/s).
      * HONEST READ: the live exec/tx gain (17.6 -> 16.7, ~1.05x) is far smaller than the offline
        1.19-1.29x, because the executor is a MINORITY of live per-tx cost (state provider + engine
        loop dominate). Block rate did move toward the 2 blk/s target. Neither pair of runs was a
        controlled A/B (different block sizes/chain instances) — for a rigorous live claim use
        `ab-fastpath.sh`.
- [x] **GAP ATTRIBUTED (iteration 5) — SIGNATURE RECOVERY IS THE DOMINANT COST, NOT EXECUTION.**
      No code and no rebuild needed: reth already exposes the split via
      `reth_sync_execution_transaction_execution_histogram` (executor calls) and
      `reth_sync_execution_transaction_wait_histogram` (waiting on the tx iterator). Measured live,
      146 blocks / 870k txs at ~5,962 txs/blk:
        whole execute_transactions loop ... 92.3 ms/blk  (15.48 us/tx)
          waiting for next tx ............ 60.7 ms/blk  (**10.18 us/tx, 66%**) <- SIG RECOVERY
          executor (our code) ............ 21.5 ms/blk  (3.61 us/tx, 23%)
          loop overhead (receipt clone+send, metrics, atomics) 10.1 ms/blk (1.69 us/tx, 11%)
      **Our executor is only 23% of the execution phase.** Four iterations of executor work
      (fast path, read caches, commit-once) took it to 3.61 us/tx; there is little left there.
      ROOT CAUSE FOUND: `payload_validator.rs:323` does `let convert = |tx| tx.try_into_recovered();`
      — reth recovers the signer FROM SCRATCH for every tx in a payload and never consults the
      mempool, even though those txs arrived by gossip and their senders were already recovered at
      pool insertion. Every validator repeats ~47,618 ECDSA recoveries per block.
- [x] **🎯 EAGER PARALLEL SENDER RECOVERY — DONE, 4.0x on the execution phase, NO reth fork (2026-08-08).**
      The item below was right that recovery dominates, but WRONG about the line and the fix, and
      its caveat was wrong too. All three corrections came from measuring first.

      **Correction 1 — wrong line.** `payload_validator.rs:323` is the `BlockOrPayload::Block`
      branch. `newPayload` takes the OTHER branch: `EthEvmConfig::tx_iterator_for_payload`
      (`ethereum/evm/src/lib.rs:300`), which does RLP-decode + `try_recover` per tx. `ArcEvmConfig`
      merely DELEGATED to it — so Arc can override it in arc-evm. **The fork was never needed.**

      **Correction 2 — the caveat was wrong; it is not contention.** `recovery-probe.sh` (new)
      diffs reth's `transaction_execution` / `transaction_wait` histograms per validator. wait/tx
      held at 10.05 vs 10.86 us going from 2 to 8 competing spammers, and at ~10 us under BOTH
      state-root strategies, while our executor's own time moved 2.87 -> 4.14. Structural, not CPU.

      **Correction 3 — it was never "recovery is expensive", it was the PIPELINE.**
      `examples/recovery_bench.rs` (new) measures the floor on this box: decode 0.22 us/tx, ECDSA
      recover 33.42 us/tx serial, **4.02 us/tx across 16 threads (8.5x)**. The live loop realised
      only ~3.3x of that. reth streams recovery through an ordered per-tx channel
      (`spawn_tx_iterator` -> `for_each_ordered_in`), and that delivery — not the cryptography —
      was the limit.

      **Fix (arc-evm only, ~40 lines, env-gated `ARC_EAGER_RECOVERY=1`):** override
      `tx_iterator_for_payload` to recover the whole payload up front on rayon and hand reth an
      already-computed vector. Items are a two-state `PayloadTx::{Done,Raw}`; both funnel through
      one `recover_payload_tx`, so a bad tx surfaces at the same index and the flag-off path stays
      lazy exactly as upstream drives it. Guarded by `EAGER_RECOVERY_MIN_TXS = 30`, mirroring
      upstream's own small-block threshold, so an idle 2 blk/s lane is untouched.

      **MEASURED — same box, same blocks, 4-way A/B (val1 eager+fallback, val2 fallback-only
      control, val3/4 stock):**
      | val | config | loop/tx | wait/tx | exec/tx | exec ms/blk |
      |-----|--------|---------|---------|---------|-------------|
      | 1   | eager + fallback | **3.90 us** | **0.85** | 3.04 | **18.9** |
      | 2   | fallback only    | 15.36 us | 11.99 | 3.37 | 74.6 |
      | 3/4 | stock            | 20.8-21.6 us | 15.8-16.4 | 5.0-5.2 | 101-105 |

      → **3.9x vs the same-config control, ~5.4x vs stock; wait/tx down 93%.** Earlier run at
      ~10.3k txs/blk: 36.9 vs 175.7 ms/blk (4.8x). The execution phase is no longer
      recovery-dominated: wait fell from ~78% to 22% of the loop, and OUR executor is now the
      majority of what remains.

      **Consensus-validated:** 350 consecutive blocks, 1,567,888 txs, all 4 validators identical on
      stateRoot + blockHash + receiptsRoot at EVERY height, with val1 eager against 3 non-eager
      peers. 34 of those blocks fell below the 30-tx threshold, so both branches were exercised.
      A 200-block/1.47M-tx run before the threshold guard also agreed everywhere. Both offline
      gates IDENTICAL, no DIVERGED.

      **Not done / open:** cadence did NOT move (0.76-1.5 blk/s) — as with every EL win so far, it
      shows up in per-block exec_ms, not fleet tps, because ~2.4 s/height is consensus coordination
      (OUT OF SCOPE by the hard constraint). Not yet measured on the 4-machine fleet. Still gated
      off by default. Eager recovery does full recovery work even when tx 0 is invalid, but reth's
      own parallel path already recovers ahead speculatively, so this is not a new DoS surface.

- [ ] ~~NEW TOP PRIORITY: reuse mempool-recovered senders for newPayload txs.~~ **SUPERSEDED above.**
      Still theoretically the last ~4 us/tx (recovery that is now parallel but still performed),
      and it WOULD need the fork plus pool plumbing the payload validator does not currently have.
      Much smaller prize now that the pipeline stall is gone. Original note: up to ~10 us/tx
      (66% of the execution phase) — bigger than everything achieved so far combined. Lives in the
      EL (reth's tx iterator), so it is INSIDE the constraint, but it does need the reth fork
      (`experiments/reth-fork/apply-fork.sh`, already proven to build). Sketch: in
      `tx_iterator_for`, look the tx hash up in the pool and reuse its recovered sender; fall back
      to `try_into_recovered()` on a miss. Correctness is easy to keep — a wrong sender changes the
      state root, so the existing gates + live root agreement catch it immediately.
      CAVEAT on sizing: the `wait` metric is what PARALLEL recovery could not hide, measured on a
      box also running 8 spammers, so the recoverable share may be smaller on the fleet. Re-measure
      there before/after.
- [ ] (superseded) the live/offline gap is now the story. Offline the executor path is 45/35 ms for
      47,618 transfers (~0.8 us/tx) but live exec is ~16.7 us/tx at a fifth the block size. So
      ~95% of live per-tx execution cost is OUTSIDE ArcBlockExecutor — reth's state provider stack
      and engine-tree per-tx loop. Further micro-optimisation INSIDE the executor has little left to
      give (arithmetic is already 0.09 us/tx). MEASURE THAT GAP before optimising anything else.
- [x] (done) cut receipt + commit overhead (78% of what remains). Hypothesis: the
      cost is `db.commit(state)` per transaction on `State<DB>` with bundle tracking — 3 accounts x
      47,618 txs = ~143k TransitionAccount records, each with allocations. Idea: keep a per-block
      overlay of pending account changes in the executor, serve fast-path reads from it, and commit
      ONCE at `finish()` — ~16k transitions instead of 143k. Receipts stay per-tx (they are cheap
      and must keep cumulative-gas order).
      RISK TO SETTLE FIRST (measure before writing): committing once at the end changes how
      BundleState records reverts/original_info. The final PLAIN state (hence the state root) should
      be identical, but revert data matters for reorgs — verify with the gates AND by inspecting the
      bundle, not just balances. Also confirm nothing between txs reads committed state directly.
      **PRE-CHECK DONE (iteration 3) — hypothesis CONFIRMED.** Isolated `db.commit()` with a
      realistic 3-account diff per tx, bundle tracking on. Full decomposition of the ~54 ms:
        * **db.commit() per tx ..... 27.8 ms (51%)  <- THE single biggest cost**
        * receipts + misc .......... ~14.5 ms (27%)
        * 2 state reads/tx ......... 7.1 ms (13%)
        * pure arithmetic .......... 4.6 ms (9%)
      => Committing once per BLOCK instead of once per TX targets 51% of the executor path.
      47,618 commits x 3 accounts = ~143k TransitionAccount records collapse to ~16k (one per
      touched account). Plausible saving ~25 ms of 54 ms (~2x on top of what we already have).

      DESIGN for next iteration (not yet implemented):
        * per-block overlay `HashMap<Address, (AccountInfo original_at_block_start, AccountInfo current)>`
          in the executor; fast path reads overlay-first then DB; fast path writes ONLY the overlay.
        * `commit_transaction` still builds the receipt per tx (cheap, and cumulative-gas order must
          be preserved) but skips `db.commit`.
        * at `finish()`, emit ONE EvmState from the overlay — `Account::from(original_at_block_start)`
          with `info = current` — and commit it once, BEFORE the existing system-contract calls.
        * INVALIDATION: any general-EVM tx must flush the overlay to the DB first (same rule the
          blocklist/beneficiary caches already use), because arbitrary code reads live state.
      WHY THE REVERT SEMANTICS SHOULD HOLD (verify, do not assume): reth keeps reverts per BLOCK
      (`merge_transitions` runs per block), not per tx, and a revert is against the block-start
      value — which the overlay preserves via `original_at_block_start`. Per-tx transition
      granularity is not needed for block-level unwinding. VERIFY by inspecting the bundle
      (reverts + plain state), not just balances, in addition to both gates.
- [ ] (deprioritised, was STEP 1) BATCH EXECUTION

 inside ArcBlockExecutor.** This is the unlock
      for every other execution win, and nothing fundamental blocks it — the earlier "can't batch"
      note applied to reth's GENERIC side (it cannot construct `E::Result`); inside arc-evm the types
      are concrete.
      WHY: measured layering of live execution shows compute is ~1% of cost —
        pure transfer arithmetic 0.09 us/tx | + executor machinery 1.35 | + live node 7-9.
      ~85% is STATE READS + per-tx bookkeeping. The fast path does 5 reads/transfer:
      basic(recipient), basic(sender), basic(beneficiary), blocklist SLOAD(sender),
      blocklist SLOAD(recipient). **3 of the 5 are constant for the whole block** (beneficiary never
      changes; the blocklist contract is not written by transfers) => cache once per block, 5 -> 2.
      Batching additionally enables: ONE parallel prefetch pass for every touched account, the
      verified 4.25x sender-partitioned parallel scheme (needs the full tx list), and amortised
      receipt/bookkeeping work.
      DESIGN (already validated in mission 1): buffer during VALIDATION only
      (`!ctx.extra_data.is_empty()`, the discriminator Arc's own finish() uses) so the BUILDER — which
      inspects per-tx results for inclusion — stays strictly per-tx; execute the batch in `finish()`;
      feed results through the EXISTING `commit_transaction` in ORIGINAL tx order so receipts/gas/
      bloom remain production code. The engine loop ignores the per-tx return value and tolerates
      receipts appearing only at the end, and reth's receipt-root task FAILS CLOSED (0 streamed
      receipts -> returns nothing -> validator computes the root from final receipts). All verified.
      MUST PRESERVE EXACTLY (or the root diverges and consensus halts):
        * per-tx block gas-limit check at the same position in the sequence;
        * nonce order within each sender;
        * identical error semantics — safest is: any ineligible/failing tx aborts the batch and the
          whole block re-runs serially;
        * receipt order + cumulative gas in ORIGINAL tx order.
- [ ] (deprioritised) transport A/B — payment lane over HTTP vs IPC.** Needs: payment EL to
      expose an IPC socket on a shared volume, CL flags `--payment-eth-socket` /
      `--payment-execution-socket` pointing at it (both already exist), and the socket mounted into
      both containers. Compare block time + cadence at full 1-Ggas blocks, same load.
- [ ] (superseded) measure engine-API ingestion cost directly. Timestamp the CL's `newPayload` request against
      reth's reported `elapsed`, at full 1-Ggas blocks. Cheapest route: reth debug/trace logs on the
      authrpc handler, or compare CL-side round-trip time vs EL-side elapsed.
- [ ] STEP 2: if large — try IPC for the payment lane (config-only) and re-measure.
- [ ] STEP 3: whatever the data says next (persistence tuning, frozen root, parallel exec).

## Rules
- Small verified increments; never leave the repo broken or a chain half-deployed.
- Tear down cleanly if the box is left idle.
- Record findings here and in CLAUDE.md every iteration, including negative results.


## ⚠️ MISSION 4 ITER 15: ITER-14's "1.8-2.6x HEADROOM" DOWNGRADED TO UNCONFIRMED — attribution bug + chain-age confound (2026-08-09)

Attempted the systematic drain-frontier sweep and instead found two problems with iter-14's
headline, both mine.

**1. The v1 drain harness's time attribution was WRONG.** `dt*frac/span_blocks` reduces
algebraically to the ALL-block average — diluting full blocks with the fast empty blocks after the
backlog runs out, biasing LOW. Iter-14's 60M "208 ms" came from a 16-of-24-full drain; corrected
for its composition it is ~237 ms. Today's early "93-114 ms" prints were 21-25 full blocks diluted
by 80+ empties — worthless. (drain-test.py v2, now committed, samples at 1 s and keeps only
intervals whose blocks are ALL >=95% full. The v2 rewrite initially FAILED TO LAND — a shell died
mid-heredoc and the old file kept running; caught because the output format didn't match. Verify
the file content, not the write command.)

**2. Drain height is strongly CHAIN-AGE dependent.** Same chain, same config: fresh ~237 ms
(corrected iter-14 estimate) vs **513 ms after ~1.5 h of churn** — this one measured cleanly
(38-of-39 blocks full, dilution negligible). 513 is barely below the 534-559 "sustained" numbers.

**WHERE THE TRUTH CURRENTLY STANDS:** on a FRESH chain there is real evidence of substantial
consensus headroom at 60M (~237 ms, roughly 2.3x the sustained number) — but it is a corrected
estimate from a mis-instrumented run, not a clean measurement, and it decays with chain age at a
rate that itself needs measuring. The honest claim is: "the sustained frontier is delivery-shaped
(mechanism proven: builder-deadline fill-pacing), and fresh-chain capacity is meaningfully higher,
magnitude 1.05-2.3x UNCONFIRMED pending clean fresh-chain drains." The deck/paper stay untouched.

**THE CLEAN EXPERIMENT (next):** per point, boot a FRESH chain -> immediate blast -> v2 drain x3 ->
teardown. Sizes 60M/100M x stock/spec = 4 fresh boots, ~80 min. Also record chain height at each
drain so the age-decay curve comes out of the same data. Blast phase note: after any KILLED drain
cycle, spammer nonce state is dirty and the next spam start can die silently — always verify pool
growth (>10k in 20 s) before trusting a blast.

## 🚨🚨 MISSION 4 ITER 14: THE FRONTIER WAS A DELIVERY CURVE — true consensus capacity is 1.8-2.6x higher (2026-08-09)

The drain test (new `fleet/drain-test.py`): pre-fill the mempool with a large backlog, STOP all
spam, and measure the chain draining 100%-full blocks — fill time is zero because the transactions
are already there. What remains is pure consensus capacity. Fleet, stock, unpaced:

| size | drain height (full blocks) | drain tps | reported "sustained" | gap |
|------|---------------------------|-----------|----------------------|-----|
| 60M | **208 ms** (16 full blocks, 2,856 tx) | **13,705** | 534-559 ms / ~5,300 | **2.6x** |
| 100M | **385 ms** (12 full blocks, 4,761 tx) | **11,423** | 697 ms / 6,827 | **1.8x** |

**THE MECHANISM, now understood end-to-end:** the payload builder's 500 ms deadline keeps the
build job's live `best_transactions` iterator open, pulling transactions AS THEY ARRIVE. Under
any spam configuration we own (delivery plateau ~5.3-5.8k tx/s), the builder sits waiting for the
pool to feed it to fullness, getPayload (await-in-progress) waits for the builder, and the height
equals txs-per-block / delivery-rate. Blocks come out "100% full" — which every prior sweep took
as proof of saturation — while actually being FILL-PACED. "100% full" was never a sufficient
saturation check; the drain test is.

**WHAT THIS REVISES (a lot):**
- The sustained frontier (520 ms/2,287 ... 697 ms/6,827, "knee at 100M") is a DELIVERY curve.
  True capacity: >=13.7k tps at 60M / >=11.4k at 100M — and in drain mode 60M BEATS 100M, so the
  "knee" and possibly the whole bigger-blocks story need re-derivation.
- The "height = 450 ms floor + ~100 us/tx" model was fit to delivery-tainted points. The
  consensus-split decomposition's vote-gap term must have largely been fill-wait inside the
  BUILDER (the CL build_time = 137-265 ms at 60M included the builder waiting for txs; in drain
  the whole height is 208 ms).
- Iter-13's "prebuild is invisible because 60M is ingest-bound" stands, and generalises: EVERY
  operating point we measured was ingest-shaped.
- Deck/paper: current frontier slides state capacity numbers that are LOWER BOUNDS by a measured
  1.8-2.6x. Do NOT rewrite yet — re-derive the frontier with drain tests first (one clean pass).

**CAVEATS (stated):** n=1 per size (the second/third blasts failed to build backlog — the -l
nonce-resync races the previous drain's tail; fixed with settle+verify guards in the harness);
drains are short (12-16 full blocks from a ~48k backlog capped by pool config 200k and delivery
during blast); agreement not re-checked within drains (chain healthy, early check passed at boot).
Drain-mode is also not a product operating mode — real traffic arrives continuously — but it is
the correct measure of CONSENSUS capacity, and it says the lane has ~2x headroom the delivery
tooling has been hiding.

**NEXT (one systematic pass):** drain-frontier sweep — sizes 25/40/60/100/200M x stock-vs-spec x
n>=3 drains each, bigger backlogs (raise pool caps or chain blasts), + a delivered-rate sweep at
fixed size to map the transition from fill-paced to capacity-paced. THEN rewrite the frontier
chart, deck, and model in one pass.

## 🎯 MISSION 4 ITER 13: THE MECHANISM NUMBER LANDS — and the fleet's 60M wall turns out to be DELIVERY (2026-08-09)

Four results in one session (tailscale re-authed mid-iteration by the user).

**1. THE CLEAN MECHANISM MEASUREMENT — unpaced 40M pair, single box, same day, 100% full:**

| 40M UNPACED | stock | spec |
|---|---|---|
| natural height | 485 ms (sd 0.5%) | **437 ms (-10%)** |
| tps | 3,930 | **4,355 (+10.8%)** |
| CL build | 121-153 ms | 49-69 ms |

**This is the prebuild's true floor cut, finally visible**: in a consensus-bound regime (fill time
~360 ms << height) with no pacer to clip it, removing the build from the critical path is worth
-48 ms / +10.8% tps. THE mechanism number for the paper.

**2. EVERY 60M MEASUREMENT FOR TWO DAYS WAS DELIVERY-BOUND.** The tell: fill time == height in all
of them. Single-box 60M unpaced: stock 535 / spec 541 with delivery ~5.3k -> 2,856/5,300 = 538 ms.
Fleet 60M (control 534, dual-ts 538, chain-anchored 538): delivery 5,311 -> fill 538 ms. Doubling
fleet spammers made it WORSE (549 ms, 96% full, tps DOWN to 4,992 — the known over-offering
ingress cost), so ~5.0-5.3k is the current spam tooling's delivery plateau at 60M cadence. **No
build-side treatment can move an ingest-paced height.** RETROSPECTIVE CORRECTIONS: (a) iter-8's
single-box "554 -> 514" is reinterpreted as a CPU artifact — freeing build cores sped the
CO-LOCATED spammers (fleet spam is remote -> no such effect -> fleet nil); (b) the iter-9/10 fleet
nils were delivery-masked, not only hit-rate-limited.

**3. CHAIN-ANCHORED PUBLISH: WORKS.** On-fleet hit rates: val1 92-98%, val2 95-100%, val3 75-89%
(parent-misses collapsed vs the 53/68 of the single-step rule). val4 (wifi alien2) remains chronic
(39-53%, parent-miss 76-87): its FCU lag outruns even the chained window. Accepted as straggler
reality — 3 of 4 validators now speculate at near-perfect rates.

**4. FLEET 50M SPEC PACED: 524 ms HOLDS (sd 0.4%) — within the stock range (522-537).** At sizes
below the pacer, both arms pin the floor; the win is stability margin by design.

**WHERE THIS LEAVES PREBUILD:** mechanism proven (-10% where the regime allows; builds halve
everywhere; 3/4 validators ~95%+ hits). At current PRODUCT configs it buys pacer margin, not tps,
because: <=50M -> pacer clips; 60M -> delivery-limited at ~5.3k (needs >5.7k to make 500 ms
feasible). The tps unlock is delivery-side or bigger blocks (100M+, where earlier frontier data
shows 6.8k delivered).

**⚠️ NEW OPEN QUESTION — THE BIGGEST ONE YET: how much of the fleet frontier is DELIVERY
EQUILIBRIUM rather than consensus capacity?** Every "sustained, 100% full" point was measured with
demand tuned to ~just fill the block — and at 100M the landed rate (6,830 tx/s) EQUALS
txs-per-block / height there too. The "knee at 100M" may partly be where spam delivery scaling
saturates, not where consensus does. Needs a designed experiment: at a FIXED size, sweep DELIVERED
rate (not offered) and look for the height response. Until then, treat the frontier as an upper
bound on latency and a lower bound on capacity.

## ✅ MISSION 4 ITER 12: CHAIN-ANCHORED BINARY REGRESSION-PASSES SINGLE-BOX; fleet still blocked on tailscale (2026-08-09)

Tailscale required re-auth again (fleet unreachable) -> single-machine work per the standing rule.
Ran the two owed items in one 28-min single-box run on the chain-anchored binary (`3485bd62`, sha
verified inside the container), spec on all 4, 60M, blocks 100% full:

**REGRESSION: PASS.** All four validators healthy on the NEW publish path:
| | v1 | v2 | v3 | v4 |
|---|---|---|---|---|
| hit rate (~600 proposals each) | 92% | 91% | 91% | 93% |
| parent-miss | 3 | 25 | 1 | 0 |
| ts-miss | 48 | 32 | 55 | 41 |

Window: 530 ms / 5,394 tps at 100% full (val1 CL build 108.8 ms — window average including
misses). Dual-ts build economics confirmed: ~4,815 builds over ~3,170 heights = ~1.5 builds/height
(the second candidate is skipped when t1 == t0), so the extra CPU is as designed.

**MEMORY with dual-ts speculation: growth ~245 MiB/min at 60M single-box — comparable to the known
stock baseline (280-415 at 100M fleet), i.e. speculation adds no visible memory penalty.** BUT the
run ended the same way every long single-box run does: all four pay ELs marched from ~400 MiB to
5.8 GiB and **val2 was OOM-killed at the demo's 6 GiB cap at minute ~28** (`oom=true exit=137`) —
the KNOWN unbounded-under-load growth, now with a third confirmed kill. This preempted the formal
120-block agreement check (one EL down -> connection refused): the run's health evidence is the
100%-full lockstep window + the hit-rate symmetry, NOT a completed 4-way agreement check. Recorded
as such; the fleet rerun must do the agreement check early, not last.

**OPS:** the demo's 6 GiB pay-EL cap bounds any single-box soak to ~25 min under 60M load. Either
raise the cap for soaks or schedule checks before minute 20. (Also: agreement checks BEFORE
long-tail measurements from now on.)

NEXT (unchanged, needs tailscale re-auth): re-ship `3485bd62` -> verify per-host shas -> fleet spec
60M/75M windows -> expect val3/4 parent-misses to collapse -> projected fleet 60M ~515-520 ms HOLDS.

## 🔧 MISSION 4 ITER 11: CHAIN-ANCHORED PENDING PUBLISH BUILT — fleet rerun blocked on a wedged ship (2026-08-09)

**The straggler fix, designed around the trap in the naive version.** "Publish pending on ANY valid
insert" is UNSAFE as stated: if the parent's state is not resolvable when publishing, the pending
BlockState silently anchors PAST unpersisted ancestors and speculative builds would produce
INVALID payloads (served on a hit -> rejected proposal -> liveness hit). The safe fix
(fork commit `794870f`):
- chain-state: new `set_pending_block_chain(chain_newest_first)` — builds nested BlockStates so
  the overlay is exact through arbitrarily deep unpersisted ancestry.
- engine tree: when parent != canonical head and the env is set, assemble `[executed] +
  tree.blocks_by_hash(parent)` (the tree keeps the full executed chain back to the persisted
  anchor) and publish the chained pending. Publishing only proceeds when the chain RESOLVES — a
  wrong overlay is impossible by construction. Replaces iter-8's single-step parent==pending rule,
  which the stragglers defeated with chains of unpublished pendings (53-68 parent-misses/window).

Built in the bookworm container (sha `3485bd62`), image overlaid + glibc smoke-tested, fork patch
reverted, repo clean.

**Fleet rerun NOT done:** ship-images wedged mid-transfer to ginnythui (wired host, stuck >20 min
on a 2-5 min copy) and was killed. Remotes still run the dual-ts binary `bfce1b06`. A boot/ship
RACE was also caught and stopped before it produced a mixed-version fleet measurement (the boot
script was launched while the ship was still copying — the remotes would have come up on the old
binary and the window would have measured a version mix; rule: NEVER boot while a ship is in
flight; verify per-host binary sha before any fleet run).

NEXT: re-ship (idempotent, sha-checked) -> verify 3485bd62 on all four -> spec 60M window (+75M)
with fl2_measure -> expect val3/4 parent-misses to collapse; projected fleet 60M ~515-520 ms =
HOLDS. Then the owed mem-soak with speculation on.

DECK (separate commits): opening now carries the two-problem arc explicitly — Problem 1 (shared
state, solved by the lane, with measured spoiler) -> Problem 2 (coordination: a bigger block grows
execution ~11 us/tx but consensus ~87 us/tx). This closes the contradiction between the original
problem slides and the results section.

## 🎯 MISSION 4 ITER 10: DUAL-TIMESTAMP CONFIRMS THE PHASE HYPOTHESIS — fleet ts-misses collapse 90-421 -> 2-8 (2026-08-09)

Implemented dual-timestamp speculation (build BOTH `t0 = max(parent_ts, now)` and `t0+1`, stash up
to two candidates per parent, serve whichever matches). Rebuilt via the container recipe, shipped
(sha-verified bfce1b06), and re-ran the same-day fleet A/B at 60M with the FIXED instrumentation
(per-window counter deltas + remote CL build_time via tailscale — both gaps from iter 9 closed):

| | control (env off) | spec (dual-ts) |
|---|---|---|
| height | 534 ms (sd 1.3%) | **525 ms** (sd 0.5%) |
| tps | 5,353 | 5,439 |
| ts-miss / window | — | **2-8 per validator** (was 90-421) |
| CL build val1/val2 | 137-158 ms | **54-63 ms** |
| CL build val3/val4 | 137-248 ms | 102-153 ms |
| hit rate | — | val1/2: 93-94%; val3: 68%; val4: 58% |

**The phase-lock hypothesis is CONFIRMED by its own cure**: the timestamp-miss class is gone,
phase-independent, exactly as designed. Fleet height moved 534 -> 525 (the hold boundary), tps
+1.6% — real but bounded by the residual miss class.

**REMAINING GAP — miss_parent on the stragglers** (val3: 53, val4: 68 per window; the wifi and
slower machines). The fork publishes pending when parent == canonical OR parent == current
pending; on those hosts the parent's OWN pending was evidently never published either (a chain of
unpublished pendings under FCU lag), so the condition still fails. NEXT REFINEMENT: under the env
flag, publish pending on ANY valid insert (drop the parent conditions entirely — Arc-safe: one
proposer per height, no side-chains). If val3/4 then hit like val1/2, projected fleet 60M ≈
515-520 ms = HOLDS.

Measured hit-rate economics: val1/2 at 93-94% halve their proposal build (137-158 -> 54-63 ms
CL-side). Every remaining ms is in the straggler misses.

DECK restructured per user request (separate commit): Four-Levers promoted from appendix beside
the anatomy slide with a v2.5 scheduling row; Demo 2 updated 2,000/s -> 5,000-6,800 sustained;
Summary rewritten around the frontier + cost model; dead skip-root idea marked resolved.

## 🚨 MISSION 4 ITER 9: THE SINGLE-BOX PREBUILD WIN DOES NOT TRANSFER TO THE FLEET — hit-rate collapse, phase-lock hypothesis (2026-08-09)

Fleet session, fork image on all 4 machines (content-verified), one image both arms (the fork
edits are env-dead, so env-off IS the stock control — and doubles as drop-in validation of the
fork binary). Distributed spam, all points 100% full, 6-min windows, same day:

| | control (env off) | spec (env on) |
|---|---|---|
| 60M | 534 ms / 5,353 tps | **538 ms / 5,304 tps — NIL** |
| 75M | 559 ms / 6,394 tps | 546 ms / 6,539 tps — marginal |

vs the single-box same-day A/B at 60M: 554 -> 514 (-40 ms). **The win does not transfer.**

**WHY — the hit rate collapsed:** fleet 47-69% vs 88-92% single-box, with miss_timestamp exploding
(90-421 per validator vs ~20-40) and miss_parent at 207 on alien2 (the wifi/15GB straggler).

**PHASE-LOCK HYPOTHESIS (fits all observations):** the prediction `ts = max(parent_ts, now)` is
evaluated ~0.45 s before the real request; it misses when the wall-clock second boundary falls
inside that window. Whether it does depends on the PHASE of the block cadence against the 1-second
timestamp grid. Single-box heights were 509-514 ms — two heights ~= 1.02 s, near-resonant, so the
boundary stayed out of the speculation window and **the 88-92% hit rate was partly RESONANCE LUCK,
not robustness**. Fleet heights are 534-546 ms — two heights ~= 1.07-1.09 s, the phase drifts every
height, the boundary sweeps through the window, and ~30-50% of predictions miss.

**THE ROBUST FIX (next): DUAL-TIMESTAMP SPECULATION.** Build BOTH candidates — t0 = max(parent_ts,
now_secs) and t1 = t0+1 (skip when equal) — stash both, serve whichever matches. Kills the
miss_timestamp class BY CONSTRUCTION, phase-independent; costs a second build on cores that are
idle anyway plus a second stash slot. (alien2's miss_parent is the straggler variant and is
partially inherent.)

**MEASUREMENT GAPS in today's fleet windows (recorded so the rerun fixes them):** (1) counters were
read CUMULATIVE and the chain idled ~40 min before load (head 2562 at load start) — in-window hit
rates may be worse than the 47-69% cumulative; any rerun must snapshot counter DELTAS per window.
(2) `fl_measure.py` does not capture the remote CLs' `block_build_time`, so today's fleet windows
lack the mechanism metric entirely — add tailscale-side CL metric snapshots.

**ANSWER TO "what about SSZ decode" (asked directly):** measured, it is real but second-order:
stream+decode = 18.2 us/tx of the ~98 us/tx fleet marginal, and the fixed decode share inside
newPayload is 6-11 ms/block. The dominant slope term remains the vote gap (68.4 us/tx = peers
receiving + DECODING + EXECUTING the block + two vote rounds gated on the slowest machine) — so
dissemination-and-agreement, of which SSZ decode is one modest slice. Compact blocks would attack
the 18.2; deferred execution the peers-execute share; neither is EL-side.

Deck: anatomy-of-a-height slide added (page 17), prebuild status stated honestly (single-box
proven, fleet nil pending the dual-timestamp fix).

## 🎯 MISSION 4 ITER 8: FORK FIX LANDS — 60M HOLDS 500 ms WITH PREBUILD: 514 ms, 5,560 tps (2026-08-09)

The first genuinely fork-requiring change of the project, and it delivers the mission goal:
**a bigger block holding the latency target.**

**Fork change** (committed in ~/reth-fork as `290b95c`, both edits dead unless
`ARC_SPECULATIVE_BUILD=1`): (1) engine tree also publishes the pending block when the inserted
block extends the CURRENT pending block (upstream requires parent == canonical head, which loses
the race with the decide FCU on ~40% of heights past the pacer floor); (2) `set_pending_block` can
anchor the new pending state on the current pending state, so the overlay never skips a block's
changes. Sound on Arc: one proposer per height, no competing side-chains. EVM lane and any
deployment without the env are bit-for-bit upstream.

**Gate first:** build_gate against the fork-patched workspace — IDENTICAL under both flags, and
the reference block hash is byte-identical to the upstream-built binary's (0x5e4f0528...).

**RESULT (same-day, same box, 6-min windows, 100% full blocks):**

| | stock 60M | FORK+spec 60M |
|---|---|---|
| height | 554 ms — misses | **514 ms — HOLDS** (sd 1.8%) |
| tps | 5,159 | **5,560** |
| CL build | 262-269 ms | **94-105 ms** |
| miss_parent | — | **3** (was 145 with the Arc-side fallback) |
| hit rate | — | 92% |

Same-day 50M references: stock 509 HOLDS / spec 509 HOLDS (both pacer-pinned, 4,673 tps). So with
prebuild the operating point moves 50M -> 60M at target: **4,673 -> 5,560 tps at <= ~510 ms
(+19% at the latency promise)**, single-machine. Correctness: 150 consecutive blocks / 428,400
txs, all 4 identical on root+hash+receipts+bloom on the fork image, all-spec.

**DOCKER-FROM-FORK, the documented long pole, now has a working recipe** (and its landmine has a
name): a host-built binary needs glibc 2.38/2.39 and CRASH-LOOPS in the bookworm-based image
("GLIBC_2.38 not found"). Recipe that works: `apply-fork.sh` -> build INSIDE
`rust:1.93-bookworm` with the workspace AND ~/reth-fork mounted at IDENTICAL absolute paths (so the
[patch] file paths resolve), CARGO_TARGET_DIR=target-docker (gitignored) -> overlay image = FROM
arc_execution:upstream-backup + COPY binary -> verify by binary sha + `--version` smoke test in the
runtime base -> `apply-fork.sh revert`. Upstream image kept as `arc_execution:upstream-backup`.

**OPEN:** 75M next (needs load the single box may not deliver — watch %full); then the fleet; the
memory soak with speculation on; and the standing rule stays — flag OFF by default, and the fork
image is NOT what `make build-docker` produces (rebuilding overwrites arc_execution:latest with
upstream; re-run the overlay recipe after).

## 🔬 MISSION 4 ITER 7: SAME-DAY STOCK-vs-SPEC AT 50M + 60M — the fallback is NOT enough above the pacer floor (2026-08-09)

Quiet box (pre-flight load 0.05). Two fresh chains, same-day, identical loads, images rebuilt with
the canonical-head fallback. 6-min windows:

| window | height | tps | CL build | verdict |
|--------|--------|-----|----------|---------|
| STOCK 50M | 509 ms (sd 1.4%) | 4,673 | 198-201 ms | **HOLDS** |
| SPEC 50M | 509 ms (sd 0.3%) | 4,673 | **90-111 ms** | **HOLDS** |
| STOCK 60M | 554 ms (sd 3.0%) | 5,159 | 262-269 ms | misses |
| SPEC 60M | 586 ms (sd 1.4%) | 4,874 | 169-220 ms | misses — and WORSE than stock |

**FINDING 1 — at 50M both arms pin the pacer floor (509 ms, identical heights, identical tps).**
The prebuild halves the build (198->90-111) and the whole saving is pacer headroom, reconfirmed
same-day with sd 0.3%. Note tonight's box regime is faster across the board (stock 50M HOLDS at
509 vs 537-539 in earlier runs) — same-day pairs are the only valid comparison, again.

**FINDING 2 — the canonical-head fallback FAILED at 60M** (miss_parent still 145 on val1), and the
refined root cause explains why v1 ever worked at all: **the pending trigger's window at 50M was
CREATED by the pacer slack.** Natural height < floor -> the CL pauses after decide -> FCU lands,
pending publishes, the watcher has the whole vote gap. Above the floor (60M natural 554 > 500)
there is NO slack: newPayload(N) races FCU(N-1) (~40% of heights), reth skips the pending publish
(parent not yet canonical), and the fallback (canonical head AFTER FCU) starts a ~180 ms build
racing a request that arrives within milliseconds. It cannot win.

**FINDING 3 — under the miss-storm, speculation HURTS: 586 vs stock 554 at 60M.** Wasted spec
builds + stale-stash misses cost ~6% cadence. The flag must stay OFF for >50M workloads until the
visibility gap is fixed.

**CONSEQUENCE — this is finally the fork's job.** The EL cannot see the newPayload'd block during
the vote gap when reth withholds the pending publish; no Arc-crate trigger can fix visibility.
The correct fix is the one-line relaxation in reth's engine tree (`insert_block_or_payload`):
publish the pending block for a valid insert even when the parent is not yet canonical — sound on
Arc, which has no competing side-chains; gate it behind the same ARC_SPECULATIVE_BUILD env so the
EVM lane and default deployments are untouched. Then docker-from-fork (host-build + COPY), the
documented long pole. That is the next iteration.

## 🔬 MISSION 4 ITER 6: 60M EXPOSED A TRIGGER GAP — miss_parent storm; canonical-head fallback added (2026-08-09)

Quiet box (load 0.11 pre-flight — checked BEFORE booting this time). All-spec chain, paced 500 ms:

| window | height | txs/blk | CL build | verdict |
|--------|--------|---------|----------|---------|
| 50M | 534 ms (sd 9.1%) | 2,380 (100%) | 89-103 ms | speculation healthy |
| 60M | 579 ms (sd 1.3%) | 2,856 (100%) | **148-235 ms** | speculation DEGRADED |

At 60M a NEW miss class dominated: **miss_parent** (val3: ~46% of its proposals; zero in every
50M run). Logs show no build errors — the stash was simply stale.

**ROOT CAUSE (v1 design assumption violated):** reth sets the pending block ONLY when the
newPayload'd block's parent is already canonical. Once heights slow past ~550 ms, `newPayload(N)`
racing `FCU(N-1)` becomes common; reth then SKIPS the pending update, the watcher never sees N,
and the stash keeps the previous height's build -> guaranteed miss_parent on that proposal. The
single-trigger watcher worked at 50M by timing luck, not by design.

**FIX (same crate, ~15 lines): canonical-head fallback trigger.** Each tick the watcher takes the
pending block if it is at/ahead of the canonical head, else falls back to the canonical head block
itself. The fallback window is shorter (decide -> next get_value, widened by the pacer slack) but
converts a guaranteed miss into a possible hit. Build gate re-run both flag settings: identical,
non-empty; full binary typechecks. NOT yet re-measured live.

The 60M row above is therefore a measurement of speculation-v1's failure mode, NOT of the
prebuild's potential at 60M. Re-measure 50/60/75 with the fallback + same-day stock controls next.
(50M quiet-box spec = 534 ms sd 9.1% vs yesterday 527 sd 2.3% — consistent.)

## ⚠️ MISSION 4 ITER 5: NO VALID DATA — box contended, gas-at-500ms sweep aborted (2026-08-09)

Attempted the money measurement (largest block holding the 500 ms pacer WITH prebuild on all 4).
Fresh all-spec chain at 50M read **594 ms (sd 12.1%)** and a second window **611 ms (sd 17.5%)**,
against yesterday's **527 ms (sd 2.3%)** on the IDENTICAL config and load. Load average was **38 on
16 cores** during the windows (top consumers were our own containers, but the same demo footprint
measured cleanly yesterday; 6 users logged in — daytime desktop use is the likely difference).
CL build times also inflated (114-144 ms vs 81-115), consistent with global CPU contention rather
than anything about the prebuild.

**All numbers from this iteration are DISCARDED.** Quoting them would repeat the exact mistake the
protocol exists to prevent (measuring the environment, not the chain). The sweep needs either a
quiet box (overnight) or the fleet.

STANDING RESULTS UNCHANGED: prebuild halves the proposer build at 88-90% hit rate with zero
divergence (iter 3-4); the 500 ms pacer converts the saving into headroom (iter 4). NEXT (unchanged):
gas-at-fixed-500ms sweep with prebuild, on a quiet box or the fleet; fresh-chain unpaced pair.

## 🎯 MISSION 4 STEP 3: ALL-4 CADENCE A/B — build halved network-wide; the PACER absorbs the saving (2026-08-09, iter 4)

Two fresh chains, identical 50M config and load (6 spammers x 900/s, blocks 100% full), 12-min
windows, measured from the same chain age. Run A all-stock; Run B all-4 `ARC_SPECULATIVE_BUILD=1`.

| | Run A (stock) | Run B (all-speculative) |
|---|---|---|
| height | 539 ms (sd 1.2%) | **527 ms** (sd 2.3%) |
| tps | 4,419 | 4,513 (+2.1%) |
| CL `block_build_time` | 186-218 ms | **81-115 ms** |
| hit rate | — | **88-90%** on all four |
| agreement (100 blocks) | all 4 | all 4 |

**MECHANISM: fully confirmed at network scale.** Every validator's proposal build halved
(-52% avg), hit rates 88-90%, zero divergence, zero stalls, 1,373 blocks.

**CADENCE: +2%, not the projected +15-20% — and the explanation is the PACER.** The demo applies
`BLOCK_TIME_MS=500` (malachite stable-block-times): the CL paces each height to a 500 ms target,
so once the natural height would drop below the floor, the CL just waits. The prebuild saving is
absorbed as PACER HEADROOM rather than raw cadence — the product's stable-latency behaviour
working as designed. Stock ran 39 ms over the floor; speculative runs 27 ms over. The correct next
question is NOT "how much faster" but **"how much BIGGER a block now holds the 500 ms promise"** —
the mission's actual goal (max tps at fixed latency).

**UNPACED PROBE — INVALID, recorded as such.** Flipped `set-block-time 0` on the live spec chain,
5-min window: 553 ms natural height, HIGHER than paced. Confounded: chain ~25 min old under
continuous load (state several GB larger; per-tx cost is measurably state-dependent) and sd 7.9%.
A valid natural-height comparison needs two FRESH unpaced chains of the same age. Do not quote it.

**OPS:** nohup children die with a timed-out harness shell — spawn spammers via `setsid` from a
detached script (cost one aborted window this iteration).

**NEXT:** (a) fresh-chain unpaced pair to pin the natural-height saving; (b) the money
measurement — WITH prebuild on all 4, sweep gas at the fixed 500 ms pacer for the largest block
that holds, vs mission 3's answer (25M holds, 50M misses at 537); then the fleet.

## 🎯 MISSION 4 STEP 2: SPECULATIVE PREBUILD LIVE-VALIDATED — proposer build time HALVED, 85.5% hit rate (2026-08-09, iter 3)

Single-machine 4-validator demo at 50M, ~5.4k tx/s offered (blocks filling at ~2/s cadence),
`ARC_SPECULATIVE_BUILD=1` on val1's payment EL ONLY, val2-4 stock. Chain verified producing via
RPC heads before any measurement.

**MECHANISM CONFIRMED FIRST, from CL source:** `generate_block` calls `get_payload` IMMEDIATELY
after FCU-with-attributes (no fixed wait), and `wait_for_payload` makes getPayload await the
in-flight build — so getPayload latency ≈ real build time, and a speculative hit removes it
entirely. This is why the win is visible on the CL's own clock, not just in EL sub-metrics.

**RESULTS (cumulative over ~170 proposals each):**

| validator | CL `block_build_time` (both lanes) |
|-----------|-------------------------------------|
| **val1 (speculative)** | **83.8 ms** |
| val2 (stock) | 168.8 ms |
| val3 (stock) | 164.6 ms |
| val4 (stock) | 163.1 ms |

**The proposer's build_block HALVED (-80 to -85 ms)** — and `block_build_time` is the CL-side
wall clock of the whole get_value build (EVM lane + payment lane, sequential), so this is genuine
critical-path time removed from val1's proposal turns, not a relocated sub-metric. val1's
remaining ~84 ms ≈ the EVM-lane build (untouched) + the 14% of payment misses.

**HIT RATE: 85.5%** (148 hit / 24 miss_timestamp / 1 miss_empty over ~10 min) — well above the
predicted 50-70%. At ~2 blk/s most consecutive blocks share the wall-clock second, so the
`max(parent_ts, now)` prediction usually cannot miss. All misses were the expected second-rollover
kind; zero miss_parent / miss_other — the learned-attributes approach predicts fee_recipient and
prev_randao perfectly. 685 speculative builds ran (one per height, as designed — every validator
speculates; only the proposer's turn can hit).

**CORRECTNESS: 200 consecutive blocks / 476,000 txs, all 4 validators identical on stateRoot +
blockHash + receiptsRoot + logsBloom**, val1 speculative against 3 stock peers. Zero stalls, zero
CL timeouts, no crash-restarts.

**MEMORY: no visible stash cost** — val1_el_pay 1.951 GiB vs val2_el_pay 1.962 GiB under identical
load, despite val1 running ~4x more builds (one per height vs one per own-proposal).

**NOT YET CLAIMED: cadence.** With the flag on 1 of 4 validators, expected height movement is
~0.25 x 0.855 x ~85 ms ≈ 18 ms of a ~550 ms height — invisible in single-machine ±15% noise. The
cadence claim needs a separate all-4 run (correctness is now established against stock peers, so
an all-4 cadence experiment is legitimate — same justification as the block-time sweeps). That is
the next iteration: two runs, all-4 OFF vs all-4 ON, same load, height + tps compared; then the
fleet.

## ✅ MISSION 4 STEP 1: SPECULATIVE PREBUILD IMPLEMENTED — and NO reth fork was needed (2026-08-09, iter 2)

`ARC_SPECULATIVE_BUILD=1` (default OFF), all in `arc-execution-payload` (new `src/speculative.rs` +
two hooks in `payload.rs`). Offline-gated; NOT yet live-validated.

**The design collapsed to zero fork changes**, because two facts checked out in reth 2.3 source:
1. **State visibility:** the engine tree ALREADY publishes a newPayload'd block as the PENDING
   block whenever its parent is the canonical head (`insert_block_or_payload` ->
   `set_pending_block`) — Arc's situation every height — and `state_by_block_hash` resolves the
   pending slot. So the production build function can build on N during the vote gap unmodified.
2. **Trigger:** no public subscription for the pending slot, but polling `pending_block()` at
   25 ms from a watcher thread costs <=5% of the 350-900 ms window. Spawned from
   `build_payload_builder` (our crate) when the flag is on.

**Attribute prediction** (from read-only CL source): timestamp = `max(parent_ts, now_secs)` — the
CL's own formula, second-granularity, payment lane copies the EVM lane's value and the lanes are
lockstep, so prediction misses ONLY when the wall-clock second rolls over inside the vote gap
(expected hit rate roughly 50-70%; measure live). `fee_recipient`/`prev_randao` are LEARNED from
the last real request (recorded in `try_build`) rather than hardcoding CL behaviour;
`parent_beacon_block_root = N.hash` (Arc convention); withdrawals always `Some([])`. First height
after boot never speculates (nothing learned yet).

**Correctness trap found at design time — the pool must be pre-filtered.** During the vote gap the
pool still contains N's transactions (pruning happens at canonicalization), and the build loop's
`mark_invalid` on a stale nonce REMOVES ALL DEPENDENT TRANSACTIONS — every sender chain would be
dropped and speculative blocks would come out near-empty. `FilteredBest` skips exactly N's tx
hashes so the iterator starts each sender at the post-N nonce, matching the real post-prune build.

**Serving path:** `try_build` records real attrs, then serves the stash via `BuildOutcome::Freeze`
iff parent + timestamp + fee_recipient + prev_randao + pbbr + empty-withdrawals ALL match; any
mismatch increments a labelled miss counter (`arc_speculative_build_outcome_total{outcome=
hit|miss_timestamp|miss_parent|miss_other|miss_empty|built}`) and falls through to the normal
build. Fail-safe by construction: a wrong prediction costs idle CPU, never a wrong block.

**Gate leg 3 added to `build_gate`:** speculative stash vs fresh build byte-equal for identical
inputs (hit serves Freeze, hash equality asserted); a timestamp mismatch MUST fall through (miss
path asserted). All legs pass; stock vs `ARC_PARALLEL_TRANSFERS=1` outer diff identical (10 lines,
non-empty); the 2,030-tx reference hashes are UNCHANGED from iter 1 — the hook does not perturb
the normal path. Full `arc-node-execution` binary typechecks.

**NOT DONE YET (next iteration):** live single-machine validation — `make build-docker`, 4-validator
demo with `ARC_SPECULATIVE_BUILD=1` on val1's payment EL only vs 3 stock peers, 100+ blocks of
root+hash+receipts+bloom agreement, HIT RATE from the new counter, height/cadence movement
(`reth_consensus_engine_beacon_new_payload_latency` + measured height are the only metrics that
count), memory soak alongside. Also worth knowing: with the demo's 500 ms builder deadline the
serving benefit only shows if getPayload arrives before the deadline would have elapsed — the win
mechanism is the CL's getPayload returning immediately instead of after the build.

## ✅ MISSION 4 STEP 0: BUILD GATE LANDED — the builder path is now covered offline (2026-08-09, iter 1)

The blocking prerequisite for speculative prebuild. New
`crates/execution-payload/examples/build_gate.rs` drives the REAL production build function
(`arc_ethereum_payload` — the same code both `try_build` and the invalid-tx-filtering wrapper
call) entirely offline:

- **Real storage, real roots:** `create_test_provider_factory_with_chain_spec` (MDBX) +
  `reth_db_common::init_genesis` (writes headers, hashed state AND trie), wrapped in
  `BlockchainProvider`. State roots are computed by the real trie, not mocked.
- **The injection seam is production-shaped:** `arc_ethereum_payload`'s `_pool` argument is unused
  and `best_txs` is a caller-supplied closure, so the gate feeds a fixed-order iterator of
  `ValidPoolTransaction<EthPooledTransaction>` (2,030 transfers from the prefunded localdev dev
  accounts, junk sigs + `Recovered::new_unchecked` — nothing in the build path re-recovers).
- **Checks:** (1) determinism — two in-process builds must produce the same block hash (catches
  map-iteration-order nondeterminism); (2) flag equivalence — stock vs `ARC_PARALLEL_TRANSFERS=1`
  outer-diff must be byte-identical (the fast path runs on the BUILDER via
  `execute_transaction_with_commit_condition`, and this is the first offline coverage of that);
  (3) gas/tx-count arithmetic. RESULT: determinism OK, flag runs IDENTICAL (8 lines, non-empty
  guard — an earlier invocation of this gate passed VACUOUSLY on two empty outputs because a panic
  went to suppressed stderr; the runner now fails on empty output. Trap noted for all future gates).
- Executor gates re-run as regression after the dep additions: still IDENTICAL.

**Found while building it — localdev genesis root discrepancy (benign here, worth knowing):** the
chainspec's sealed genesis header DECLARES state root `0xbc32...` but the root computed over the
inserted alloc is `0x0c6b...` (reth's `insert_genesis` and `init_genesis` both agree on the
computed one). The builder derives child roots from the DB so everything downstream is
self-consistent, and the live chain is unaffected (all nodes share the same convention). Possibly
zero-valued storage slots in the genesis JSON being treated differently by the two root
computations. The gate prints both so any change surfaces in the diff.

**NEXT (STEP 1):** the speculative-prebuild implementation itself, in the fork: on
`newPayload(N)=VALID`, start a build with predicted attributes; serve on exact match; re-seal on
timestamp-only mismatch; discard otherwise. The gate grows the third leg then: prebuilt-vs-fresh
byte equality for the same (parent, fee recipient, tx set).

Deps added: `reth-db-common` (workspace, same v2.3.0 tag), dev-deps of arc-execution-payload
(arc-evm, arc-execution-config/test-utils, reth-provider/test-utils, reth-ethereum/evm,
alloy-eips). No production-code changes in this iteration.

## 🎯 THE BUILD IS 98% REAL COMPUTE AND ALL OF IT IS PRE-COMPUTABLE — the case for speculative building (2026-08-09, iter 15)

Question raised: the EL is idle 350-900 ms during voting, and proposer selection is RoundRobin, so
why not build the next block optimistically then? Two objections I raised were wrong and one stands.

**WRONG #1 — "the EL cannot know whether it is the proposer".** RoundRobin is deterministic
(`arc_consensus_types::proposer::RoundRobin`). The EL has no validator identity today, but that is
where the information lives, not whether it exists: the EL could infer it (it only ever receives
`FCU + payloadAttributes` when it is the proposer -- observe the period), be told it (two static
flags), or simply not care (all four speculate, three discard, on cores idle 85% of the time). The
only wrinkle is that RoundRobin selects on (height, ROUND); a failed round changes the proposer for
the same height, so a wrong guess wastes idle CPU -- not correctness.

**WRONG #2 — "that is a CL change".** Asking earlier is ONE implementation. An EL-side speculative
build (start on N's post-state when `newPayload(N)` returns, serve it if the later attributes match)
is entirely inside the EL. It needs a reth fork rather than a flag, but the fork exists and builds.

**FIRST, THE FREE VERSION — three existing flags, all measured, all NULL.** Per-validator build time
(`fleet/build-time.sh`; RoundRobin means each validator builds ~1/4 of blocks, so its own metric is
directly comparable), stock Run A then treated Run B, 50M, 8 min each, WITH an in-run stock control:

| validator | flag | build A | build B | net vs control |
|-----------|------|---------|---------|----------------|
| ginny | share-execution-cache-with-payload-builder | 93.7 ms | 94.6 | **+1.8%** |
| ginnythui | share-sparse-trie-with-payload-builder | 114.9 ms | 114.9 | **+0.9%** |
| alien2 | suppress-persistence-during-build | 92.9 ms | 92.1 | **+0.0%** |
| papaduck | STOCK (control, both runs) | 147.0 ms | 145.7 | (-0.9% drift) |

Nothing. Notably `share-sparse-trie` is documented as replacing "the payload builder's blocking
`state_root_with_updates()` with the sparse trie, computing the state root concurrently with
transaction execution" -- precisely the post_execution cost below -- and it moved nothing.

**THE BUILD IS REAL WORK, NOT WAITING.** `arc_payload_total_duration_seconds` records ONE build
attempt (confirmed: 217 builds over 866 blocks = exactly one per proposal), and Arc's stage
histogram splits it. At 100% full 50M blocks (2,380 tx), build = 97.9 ms:

| stage | ms | share |
|-------|-----|-------|
| state_setup | 0.08 | 0.1% |
| pre_execution | 0.09 | 0.1% |
| **tx_execution** | **56.40** | **57.6%** |
| **post_execution** (state root etc.) | **39.33** | **40.2%** |
| assembly_and_sealing | 0.37 | 0.4% |

(At 1/3-full blocks the same measurement gives 17.3 ms split 28%/69%, so post_execution has a large
fixed component and tx_execution scales with tx count. Only quote the full-block numbers.)

**CONCLUSION: 98% of the build is executing the transactions and rooting the result -- and BOTH are
pre-computable given the parent state.** Nothing in the build is waiting on the CL or the network.
So the ~98 ms is genuinely movable into the vote gap, where the EL has 350-900 ms of idle time.
50M would go 551 -> ~453 ms, i.e. **under the 500 ms target, roughly doubling tps at 2 blk/s**
(~4,400 vs ~2,300). That is the largest single win identified anywhere in this mission.

**HONEST CAVEATS BEFORE ANYONE BUILDS THIS.** (a) The estimate assumes a speculation HIT -- right
parent, right fee recipient, mempool not materially changed. Real hit rate will be below 100% and
misses need a top-up path, not a discard. (b) Arc's `reward_beneficiary` credits the fee recipient
on EVERY transaction, so the speculative state is bound to one proposer identity -- this is not a
generic mempool prewarm. (c) It only attacks the ~450 ms floor, not the ~100 us/tx slope; expect one
step-change, then the payload-size wall again. (d) **The gate does not cover BUILT blocks**, only
validated ones. The builder is exactly where a wrong speculative state becomes a wrong block, and
this mission has already produced four consensus bugs, three invisible to state-only comparison.
Extend the gate first.

## 🔬 CONSENSUS vs EXECUTION ACROSS BLOCK SIZE — the fixed floor, measured (2026-08-09, iter 14)

Asked directly: as the gas limit grows, how does the height split between consensus and execution?
Previous answers came from one or two sizes. Swept the range on the 4-machine fleet, every point
100% full, ~6 min each, averaged across all four validators (`fleet/consensus-split.sh`,
chart `consensus-split-chart.py`).

| gas | txs/blk | height | exec | root | np-other | build+stream | vote gap | EL total | consensus |
|-----|---------|--------|------|------|----------|--------------|----------|----------|-----------|
| 25M | 1,190 | 506 ms | 32.2 | 3.8 | 6.1 | 110.7 | 353.4 | 42.1 (8%) | 464.1 (**92%**) |
| 50M | 2,371 | 551 ms | 58.3 | 5.8 | 7.2 | 127.8 | 351.5 | 71.3 (13%) | 479.3 (**87%**) |
| 100M | 4,761 | 829 ms | 109.7 | 7.5 | 10.0 | 164.5 | 537.5 | 127.2 (15%) | 702.0 (**85%**) |
| 200M | 9,517 | 1398 ms | 191.1 | 7.8 | 11.2 | 286.5 | 901.3 | 210.1 (15%) | 1187.8 (**85%**) |

**FINDING 1 — THERE IS A ~450 ms FIXED CONSENSUS FLOOR, independent of block size.** Between 25M and
50M the transaction count DOUBLES and consensus time barely moves: 464.1 -> 479.3 ms, i.e. **12.9 us
per extra transaction**. Extrapolating that flat segment back to zero transactions gives a ~450 ms
intercept. **That floor is why 2 blk/s is only reachable with small blocks** — at a 500 ms target
roughly 90% of the budget is spent before the first transaction is executed. It also means an empty
block costs nearly as much as a 2,400-tx one.

**FINDING 2 — ABOVE ~2,400 TX THE FLOOR GIVES WAY TO ~100 us/tx, which is 4-6x the cost of EXECUTING
the same transaction:**

| step | execution | consensus | ratio |
|------|-----------|-----------|-------|
| 25M -> 50M | 22.1 us/tx | 12.9 us/tx | 0.6x (still on the floor) |
| 50M -> 100M | 21.5 us/tx | 93.2 us/tx | **4.3x** |
| 100M -> 200M | 17.1 us/tx | 102.1 us/tx | **6.0x** |

So each additional transaction costs several times more to AGREE ON than to RUN. That is the whole
reason bigger blocks stop paying, and it is not something a faster EVM can touch.

**FINDING 3 — the commitment gets CHEAPER per transaction as blocks grow.** State root: 3.19 -> 2.45
-> 1.58 -> **0.82 us/tx** across an 8x range, and only 3.8 -> 7.8 ms in absolute terms. Execution
also amortises slightly (27.1 -> 20.1 us/tx). The two EL costs both improve with scale; only
consensus degrades.

**CONSEQUENCE FOR THE DESIGN.** The height is `~450 ms floor + ~100 us/tx above ~2,400 tx`. Latency
is therefore bought almost entirely from consensus, at both ends: the floor sets the minimum, and
the per-tx term sets the slope. Execution is 8-15% of the height at every size measured and shrinks
per transaction. Nothing EL-side moves either term.

## 📌 MISSION 3 CLOSED — one authoritative chart, no experiment this iteration (2026-08-09, iter 13)

Ran NO fleet experiment. The prompt's exit condition ("say so plainly rather than manufacturing a
marginal win") has been met and documented across iterations 8-12; another 1-2 h fleet run would be
manufacturing. Did the cheap outstanding deliverable instead.

**PROBLEM FIXED: three charts in the repo disagreed with each other** on the same gas sizes — the
same class of hazard that let "9.5k/15k tps at 1 Ggas" sit on a slide long enough to become the
standing impression that bigger blocks buy throughput.

| gas | blocksize-chart | blocktime-chart | frontier-long |
|-----|-----------------|-----------------|---------------|
| 25M | 504 ms, 2,363 | 529 ms, 2,249 | **520 ms, 2,287** |
| 50M | 511 ms, 4,660, "HOLDS" | 522 ms, 4,563 | **537 ms, 4,430 (does NOT hold)** |
| 100M | — | 644 ms, 7,398 | **697 ms, 6,827** |

`blocksize-chart.py` was still asserting "50M HOLDS 2 blk/s", a claim retracted twice. Both older
charts now carry a SUPERSEDED / short-window banner naming the authoritative one, and
`frontier-long-chart.py` is marked authoritative. Data kept as historical record; all three still run.

**Gates re-run as a standing regression check: all digests IDENTICAL** (state, receipts, logs,
across pool / closed / mixed).

### FINAL STATE OF MISSION 3

**Throughput — closed.** Sustained, 4 machines, 15 min per point, 100% full, all validators agreeing:
25M = 520 ms / 2,287 tps · 50M = 537 ms / 4,430 · **100M = 697 ms / 6,827 (best)** · 200M = the
limit stops binding and lands on top of 100M. Only 25M holds 2 blk/s, and at 1.92 not 2.00.
The ceiling is consensus coordination: **88% of a transaction's marginal cost is outside execution**
(per extra tx: 15.2 us proposer build, 11.4 us own newPayload, 18.2 us stream+decode, 68.4 us vote gap).

**Memory — characterised, not a leak.** Grows ~0.65-0.93 KB per transaction under load, releases at
~40-50% of that rate when load stops. No config knob moves it (block buffer, cross-block cache, state
cache all eliminated by measurement). Size for the longest continuous burst, ~0.35 GB per saturated
minute above idle.

**Correctness — four fast-path consensus bugs found and fixed** (missing per-transfer log; legacy
fee shape; zero-value logs; plus the eager-recovery artifact reverted). Gate now covers state +
receipts + logs + legacy + EIP-2930 + zero-value + mixed general-EVM interleaving.

**What is left needs the constraint lifted or a decision:** compact blocks (ship tx hashes — peers
already hold them), pipelining execution against the next height, or a heap profile for the memory
behaviour. None is EL-config work.

## ✅ IT IS NOT A LEAK — MEMORY IS RECLAIMED, JUST 2.4x SLOWER THAN IT ACCUMULATES (2026-08-09, iter 12)

The one question left worth answering, because it changes the operational advice: is the growth a
hard leak, or reclaimable? Ran all 4 STOCK at 100M, load for 20 min, then let the chain keep
producing IDLE for 14 min while still sampling memory.

| node | peak (min ~22) | final (min 34) | released | release rate | growth rate | release/growth |
|------|----------------|----------------|----------|--------------|-------------|----------------|
| ginny | 6,885 MiB | 5,198 MiB | -1,687 | 130 MiB/min | 294 | 0.44 |
| ginnythui | 8,631 | 6,671 | -1,960 | 151 MiB/min | 348 | 0.43 |
| papaduck | 11,868 | 9,753 | -2,115 | 163 MiB/min | 411 | 0.40 |
| alien2 | 6,938 | 5,166 | -1,772 | 148 MiB/min | 283 | 0.52 |

**NOT A LEAK.** Every node released memory as soon as the transaction flow stopped, monotonically,
on all four machines. (Memory kept rising ~2 min past the load ending — the backlog draining — then
turned over.)

**BUT RELEASE IS ~2.4x SLOWER THAN GROWTH** (0.40-0.52 ratio). That single number explains
everything observed: under continuous load the node accumulates at ~350 MiB/min and can only give
back ~150, so it climbs monotonically until either the load stops or the cap is hit. It also
explains why nothing in the config mattered — the memory is genuinely in use while transactions are
flowing; it is not a cache that could be shrunk.

**THIS CHANGES THE OPERATIONAL ADVICE.** Previously: "budget >=24 GB and restart periodically".
Corrected: **size for the longest expected CONTINUOUS burst, not for peak throughput.** A lane with
quiet periods recovers on its own; a lane saturated 24/7 will still reach any cap eventually, and
that is the case that needs either more RAM or a restart policy. At the measured rates, 100M
sustained needs roughly (burst_minutes x 0.35 GB) of headroom above idle.

**MISSION 3 IS CLOSED FOR GOOD.** Throughput: closed and measured (100M / ~6,800 tx/s / ~700 ms
sustained; 25M / 2,287 tx/s / 520 ms if 2 blk/s is the promise; ceiling is consensus coordination,
88% of a transaction's marginal cost sits outside execution). Memory: characterised, config
eliminated, and now shown to be reclaimable rather than a leak. Correctness: four fast-path
consensus bugs found and fixed, gate extended to state + receipts + logs + legacy + 2930 +
zero-value + mixed. There is no further EL-side throughput work that measurement supports.

## ❌ MEMORY GROWTH IS CONFIG-INDEPENDENT — AND MY "16% FROM PERSISTENCE" WAS A MACHINE ARTIFACT (2026-08-09, iter 11)

**RETRACTION FIRST.** Last iteration I reported that lowering the persistence threshold slowed
memory growth 16% (val1 298 vs val2 353 MiB/min, matched 62 GB hardware). That was wrong. I never
checked what the SAME machine pair does UNTREATED: in the all-stock baseline ginny already grew 15%
slower than ginnythui (233 vs 274, ratio 0.850). The persistence run's ratio was 0.844. **The
treatment effect was -0.7%, i.e. nothing.** Cross-machine A/B requires the untreated ratio for that
pair as the control; I compared treated-vs-control across different boxes and read the hardware
difference as a result.

**THIS ITERATION.** Probed the remaining large caches, per validator, same chain/load/size, 35 min:
val1 `--engine.cross-block-cache-size 256` (down from a 4096 MB default, 16x smaller), val4
`--engine.disable-state-cache` (off entirely), val2/val3 stock.

| run | ginny | thui | pduck | alien | ginny/thui | effect vs baseline |
|-----|-------|------|-------|-------|------------|--------------------|
| all-stock baseline | 233 | 274 | 323 | 194 | 0.850 | — |
| persistence 2/4 | 298 | 353 | 415 | 288 | 0.844 | **-0.7%** |
| cache 256 MB / cache OFF | 294 | 348 | 411 | 283 | 0.845 | **-0.7%** |

**NEITHER KNOB DOES ANYTHING.** Shrinking the cross-block cache 16x, and disabling the state cache
outright, change the growth rate by under 1% once normalised. The three runs' absolute rates are
also within ~1.5% of each other (294/348/411 vs 298/353/415), across three different configs.
**The growth is config-independent, so it is not any cache that reth exposes a flag for.**

**WHAT IT SCALES WITH.** Consistent across all three runs and every config: **+3,060-4,440 MiB per
1000 blocks**, i.e. **~0.65-0.93 KB per transaction processed** (blocks are 4,761 tx). Growth
tracks transactions, not time and not blocks. That is the signature of per-transaction retention
that is never released — a leak, or an unbounded structure with no knob.

**CONFIG IS EXHAUSTED FOR THIS PROBLEM.** Three candidate mechanisms are now eliminated by
measurement (in-memory block buffer, cross-block state cache, state cache entirely). What remains
needs a heap profile of the payment EL under load, or an upstream reth question — a different kind
of work from anything in this mission, and not config-only.

**OPERATIONALLY, UNTIL THEN:** budget >=24 GB per payment EL at 100M, and expect to restart ELs
periodically under sustained load; a node at its cap dies and silently corrupts throughput
measurements before it does (that is how the 200M frontier row came out wrong).

## ❌ PERSISTENCE THRESHOLD IS NOT THE MEMORY CULPRIT — ~16%, NOT A FIX (2026-08-09, iter 10)

Tested the mitigation named last iteration: LOWER the persistence threshold so the in-memory block
buffer is retired sooner (the earlier experiment RAISED it and hurt cadence). Config-only, per
validator: val1 + val4 got `--engine.persistence-threshold 2 --engine.memory-block-buffer-target 4`,
val2 + val3 stayed stock. Same chain, same blocks, identical load, 35 min.

| node | RAM | config | start -> end | rate | per 1000 blk |
|------|-----|--------|--------------|------|--------------|
| val1 ginny | 62 GB | **LOW** | 462 -> 9,700 MiB | **298 MiB/min** | 3,179 MiB |
| val2 ginnythui | 62 GB | stock | 1,042 -> 11,981 MiB | 353 MiB/min | 3,764 MiB |
| val3 papaduck | 78 GB | stock | 3,033 -> 15,892 MiB | 415 MiB/min | 4,425 MiB |
| val4 alien2 | 15 GB (11 GiB cap) | **LOW** | 689 -> 9,618 MiB | 288 MiB/min | 3,073 MiB |

**[RETRACTED -- see the iteration-11 entry above: normalised against the untreated ratio for this machine pair the effect is -0.7%, i.e. nothing.]** ~~MATCHED-HARDWARE RESULT (val1 vs val2, both 62 GB): 298 vs 353 MiB/min = 16% slower.~~ Real, and
in the right direction — but every node is still marked STILL CLIMBING and still adding
3.1-4.4 GiB per 1000 blocks. **So the in-memory block buffer accounts for only ~16% of the growth;
~84% is something else. The leading hypothesis is ruled out.**

val4 (the 11 GiB canary) SURVIVED this window where it OOMed in the stock run — but it finished at
9,618 MiB and was ~6 minutes from the cap at its measured rate. **The flag buys time proportional to
the 16%, not safety.** Do not treat it as a fix.

Weak secondary signal, worth one line: comparing the two STOCK nodes, the 78 GB machine grew 17%
faster than the 62 GB one (415 vs 353 MiB/min), which is consistent with reth sizing some cache
against available RAM. Confounded with hardware, so it is a hypothesis, not a result.

NEXT (still EL-side, still in scope): the buffer is exonerated, so the candidates are reth's
state/trie caches or a genuine leak. Cheapest next step is to look for cache-sizing flags on the
node binary and A/B those the same way; a heap profile would settle it but is a larger lift.
Whatever runs next, `fleet/mem-soak.sh` must run alongside it — a node quietly approaching its cap
distorts throughput measurements before it dies.

## 🚨 PAYMENT-EL MEMORY GROWS UNBOUNDED UNDER SUSTAINED LOAD — OOM REPRODUCED AND PREDICTED (2026-08-09, iter 9)

Followed up the OOM flagged last iteration instead of chasing more tps. It is real, systematic, and
a bigger problem than any throughput number here.

45 min of continuous load at the recommended 100M operating point (~8.5k tx/s offered, blocks
100% full), sampling every payment EL's container memory once a minute
(`experiments/dual-el/fleet/mem-soak.sh`):

| node | RAM | start | after 35 min | rate | per 1000 blocks | verdict |
|------|-----|-------|--------------|------|-----------------|---------|
| ginny (local) | 62 GB | 4,298 MiB | 11,766 MiB | +233 MiB/min | +2,525 MiB | STILL CLIMBING |
| ginnythui | 62 GB | 5,585 | 14,356 | +274 MiB/min | +2,965 MiB | STILL CLIMBING |
| papaduck | 78 GB | 8,011 | **18,340** | +323 MiB/min | +3,492 MiB | STILL CLIMBING |
| papaduck-alien2 | 15 GB, **11 GiB cap** | 4,328 | **OOM-KILLED** | — | — | `oom=true exit=137` |

**PREDICTED THEN CONFIRMED.** At minute 20 alien2 sat at 8,964 MiB and was climbing ~194 MiB/min,
so I predicted it would cross its 11,264 MiB cap around minute 32-35 — it did, `oom=true exit=137`.
That is the same failure that corrupted the 200M frontier row, so the earlier OOM was not a fluke.

**GROWTH DECELERATES BUT DOES NOT PLATEAU.** Local went 288 -> 209 -> 168 MiB/min across the run,
yet the last third still added +2,100 MiB. After 2,958 blocks / 35 min nothing had flattened, and
papaduck reached 18.3 GiB for a chain of 16k accounts and ~3k blocks. Whether this is a true leak
or cache growth that eventually bounds, it is far past any reasonable working set for this workload.

**CONSEQUENCES**
- **Any node capped below the trajectory dies.** 11 GiB is not enough at 100M; the three uncapped
  nodes were at 11.8-18.3 GiB after 35 min and rising. Budget >=24 GB for a payment EL at this
  load, or find the cause.
- **It invalidates long-run measurements silently.** A dying node slows the fleet, which makes
  blocks fill, which looks like a *capacity* result. That is exactly how the 200M row came out as
  "100% full at 0.61 blk/s, sd 17.4%, -20.4% drift". Any sustained run from now on must sample
  memory alongside throughput.
- **It is EL-side and therefore in scope** — unlike everything else remaining on this mission.

**NOT YET DIAGNOSED.** Hypotheses in rough order: reth's in-memory canonical block/state buffer
growing faster than persistence retires it; per-block bundle/trie overlays retained; and (weakest)
mempool backlog from the ~1,700 tx/s of surplus offered load, which the 200k pending/queued caps
should bound to a few hundred MiB. NOTE the earlier persistence experiment RAISED the threshold
(64/128) and made cadence worse; the untested direction is LOWERING it so the in-memory buffer is
retired sooner. That is the obvious next test and it is config-only.

## 🎯 SUSTAINED FRONTIER — 15 MIN PER SIZE, 4 MACHINES (2026-08-09, iter 8)

The definitive measurement: one gas limit at a time, held under continuous load for a full 15
minutes, chain sampled every 60 s, demand tuned per size to ~1.15x its expected capacity.
Harness `experiments/dual-el/fleet/frontier-long.sh`, chart `frontier-long-chart.py`.

| gas | txs/blk | %full | latency (min-max) | throughput | spread | exec | root | persist |
|------|---------|-------|-------------------|------------|--------|------|------|---------|
| 25M | 1,190 | 100% | **520 ms** (505-532) | 2,287 tx/s | 1.6% | 12.0 | 1.3 | 48.6 |
| 50M | 2,380 | 100% | 537 ms (527-566) | 4,430 tx/s | 1.8% | 26.0 | 2.2 | 61.7 |
| **100M** | 4,761 | 100% | **697 ms** (662-779) | **6,827 tx/s** | 3.6% | 56.9 | 2.6 | 97.2 |
| 200M | 4,892 | 51% | 690 ms (524-1176) | 7,092 tx/s | 3.2% | 63.2 | 3.4 | 203.6 |

All four validators agreed at every size (50-block check per size).

**FINDING 1 — THE GAS LIMIT IS A DIAL THAT ONLY ACTS WHILE IT BINDS.** At 200M the same demand no
longer fills the block, so 200M lands on top of 100M: 4,892 vs 4,761 txs/block, 690 vs 697 ms,
7,092 vs 6,827 tx/s. **Raising the limit past what demand can fill changes nothing at all.** This
reframes every earlier "bigger blocks" result: the limit is a CAP, and only the binding case is a
measurement of the limit. Forcing 200M to bind (2x demand, short runs) gives 9,523 txs at ~1,230 ms
and ~7,700 tx/s -- latency nearly doubles for ~10% more throughput.

**FINDING 2 — LONG WINDOWS MATTER, AND THEY REFINE THE HEADLINE.** Spread over 15 min is 1.6-3.6%,
against the +-12-15% short windows carry here. The refined numbers move: 50M is **537 ms / 1.86
blk/s**, not the 1.92-1.94 short runs suggested, so **25M (520 ms) is the only size that holds
2 blk/s** and even it is 1.92, not 2.00. The earlier "50M holds 2 blk/s" claim is hereby corrected
for the second and final time -- it is a 537 ms operating point.

**FINDING 3 — A LONG-RUN MEMORY LEAK ON THE SMALLEST NODE, NOT A BLOCK-SIZE CLIFF.** During the
first 200M window val4's payment EL (papaduck-alien2, 15 GB RAM, 11 GB container cap) was
OOM-killed (`oom=true exit=137`) ~6 min in, after surviving 15-min windows at 25/50/100M. That run
is therefore INVALID -- its "100% full at 0.61 blk/s, sd 17.4%, -20.4% drift" was the signature of
a dying node, not of 200M. **The OOM did NOT reproduce**: a fresh process at 200M ran 13 min clean
(sd 3.2%, no drift). So it is cumulative memory growth over ~1.5 h of sustained load crossing an
11 GB cap, not a property of 200M. Worth tracking, but do not report it as a block-size limit.

**FINDING 4 — the commitment still never enters the tradeoff.** State root 1.3-3.4 ms at every
size, flat across a 8x range of block size. Persistence is the EL cost that actually scales
(48.6 -> 203.6 ms).

**OPERATING RECOMMENDATION: 100M / ~6,800 tx/s / ~700 ms** as the throughput point, or **25M /
2,287 tx/s / 520 ms** if the promise is literally 2 blocks per second. Both sustained for 15
minutes with all validators agreeing.

## 📊 DECK ALIGNED WITH THE MEASUREMENTS (2026-08-09, iter 7)

The deck had been headlining 1 Ggas at "9.5k / 15k tx/s" as the throughput achievement. That is the
number that produced the standing impression that bigger blocks bought throughput. Rewritten to
compare 100M against 1 Ggas directly (7,398 tx/s @ 644 ms vs 7,272 @ 5,478 ms) and to carry the
marginal-cost table, which is the mechanism. Also corrected a bullet of mine that claimed state root
was "20-50 ms at 9.5k tx/block" -- that mixed the synchronous and asynchronous measurements; on the
fleet it is 2.3 ms at 4,761 tx and 0.3 ms at 39,832, and it does not grow with block size. The
non-reproduction of the 9.5k/15k figure is stated on the slide.

NO further EL experiment was run this iteration: mission 3 is closed and every ranked lead is a
measured negative or a measured knee. Running another sweep would be manufacturing work.

## 🔬 WHERE A BIGGER BLOCK'S MILLISECONDS GO — MEASURED, AND MISSION 3 CLOSED (2026-08-09, iter 6)

Two things this iteration: (a) the 50M "holds 2 blk/s" headline REPRODUCES, and (b) the marginal
cost of a bigger block is now attributed, using Arc's own `reth_arc_payload_total_duration_seconds`
(proposer build) alongside the beacon-engine metrics. Both points 100% full, 4 machines,
distributed spam.

| gas | txs/blk | height | build | newPayload | exec | root | vote gap | remainder | tps |
|------|---------|--------|-------|-----------|------|------|----------|-----------|-----|
| 50M | 2,380 | 515 ms | 68.1 | 43.5 | 35.9 | 3.0 | 347.6 | 124.2 | 4,619 |
| 200M | 9,523 | 1,216 ms | 176.7 | 124.8 | 117.2 | 1.7 | 836.2 | 254.5 | 7,834 |

**REPRODUCIBILITY CONFIRMED: 50M = 1.94 blk/s / 515 ms / 4,619 tps**, against 1.92 / 522 / 4,563
measured on a different chain instance. Within 1.5%. The mission headline stands (this check
mattered — an earlier single-run 50M claim had to be requalified when it failed to repeat).

**THE MARGINAL COST OF A TRANSACTION IS ~98 us OF HEIGHT — and only ~11 us of it is our execution:**

| component | +ms (50M -> 200M) | us / extra tx | share of the growth |
|-----------|-------------------|---------------|---------------------|
| proposer build | +108.6 | 15.2 | 15.5% |
| newPayload (own execution) | +81.3 | 11.4 | 11.6% |
| **vote gap** | **+488.6** | **68.4** | **69.7%** |
| remainder (stream + decode) | +130.3 | 18.2 | 18.6% |

**~70% of what a bigger block costs lands in the VOTE GAP** — the window from this validator
finishing `newPayload` to the next forkchoiceUpdated arriving. That window is not idle network
time: it contains the OTHER validators receiving, decoding and executing the same block, then two
vote rounds. So each transaction is effectively executed ~5x across the network (proposer builds it
once, four validators validate it) and every one of those executions sits on the critical path of
the round, with the quorum gated by the SLOWEST of them.

That is also why the state root cannot be the answer: it is 1.7-3.0 ms and it does not grow with
block size (1.7 ms at 9,523 txs vs 3.0 ms at 2,380 — noise, not scaling).

**MISSION 3 IS CLOSED.** Goal was "a block larger than 25M that still holds 2 blk/s": **50M does,
at 4,619 tps (1.95x the 2,363 baseline), 100% full, all four validators agreeing.** Best overall
operating point is 100M at 7,398 tps / 644 ms. All four ranked leads are closed: persistence is a
measured negative (it inflates state-root cost), the fine-grained sweep found the knee, the
delivery-bound flaw is fixed by distributed load, and state-root-fallback was applied throughout.

**No further tps at 2 blk/s is reachable without touching consensus.** The evidence is the table
above: 88% of the marginal cost of a transaction is outside our execution, in build + stream +
vote. The addressable levers are all in the CL path — ship transaction hashes instead of full
transactions (peers already hold them in their mempools, so the proposal duplicates ~1.2 MB at
100M), pipeline execution of height N against consensus on N+1, and homogenise the validator set so
the quorum is not gated by the slowest machine.

## ✅ MISSION 3 GOAL MET ON REAL HARDWARE — 50M HOLDS 2 blk/s AT 4,563 TPS (2026-08-09, iter 5)

Measured the fleet's LOW-LATENCY end, which had never been tested (previous fleet points started at
200M). 4 machines, distributed spam, stock config:

| gas | spammers | txs/blk | %full | blk/s | latency | tps | exec | root | persist |
|------|----------|---------|-------|-------|---------|-----|------|------|---------|
| 25M | 16 | 1,190 | 100% | 1.89 | 529 ms | 2,249 | 11.4 | 0.9 | 48.2 |
| **50M** | 16 | **2,380** | **100%** | **1.92** | **522 ms** | **4,563** | 22.9 | 1.2 | 52.9 |
| 100M | 16 | 2,845 | 60% | 1.81 | 553 ms | 5,144 | 29.6 | 2.3 | 91.1 |
| **100M** | 32 | **4,761** | **100%** | 1.55 | **644 ms** | **7,398** | 50.0 | 2.3 | 109.3 |

**GOAL MET: 50M gas — twice the 25M baseline — holds 2 blk/s at 1.92 blk/s / 522 ms with 100% full
blocks, at 4,563 tps (1.93x the 2,363 tps baseline).** All four validators agreeing throughout.
This is the mission's success criterion, on real hardware, load-saturated.

**BEST OVERALL POINT: 100M saturated — 7,398 tps at 644 ms.** That beats every larger block on BOTH
axes: more tps than 200M (7,150 @ 864 ms) and than 1 Ggas (7,272 @ 5,478 ms), at a fraction of the
latency. **100M -> 1 Ggas is 10x the block for ZERO extra throughput and 8.5x the latency.**

So the fleet frontier has a knee at ~100M, and everything beyond it is pure latency cost. The
operating rule is "smallest block that reaches the plateau", not "biggest block that fits".

Note the two 100M rows are the over-offering tradeoff again, and here it is worth taking: 16 -> 32
spammers costs 553 -> 644 ms (+16%) and buys 5,144 -> 7,398 tps (+44%). At 200M the same doubling
bought only +6% tps for +45% latency. The tradeoff is favourable at the knee and unfavourable past
it, which is another way of saying where the knee is.

## 🎯 FLEET FRONTIER — RECONCILES THE "10k tps" CLAIM WITH THE 4.7k SINGLE-BOX NUMBER (2026-08-09, iter 4)

Question raised: the deck says 9.5k tps at 1 Ggas, the single-box sweep tops out at 4,748. Both are
real; they are different hardware AND different block sizes. Re-measured on the actual 4-machine
fleet (ginny + ginnythui + papaduck + alien2), stock config, distributed spam (one spammer set per
machine against its LOCAL payment EL) so the comparison with the recorded 9.5k is apples-to-apples:

| gas | spammers | txs/blk | %full | blk/s | latency | tps | exec | root | persist |
|------|----------|---------|-------|-------|---------|-----|------|------|---------|
| 200M | 16 | 6,175 | 65% | **1.16** | **864 ms** | **7,150** | 61.8 | 2.3 | 66.9 |
| 200M | 32 | 9,523 | 100% | 0.80 | 1,250 ms | 7,616 | 98.9 | 2.3 | 80.1 |
| 500M | 16 | 15,430 | 65% | 0.53 | 1,876 ms | **8,226** | 177.0 | 1.0 | 101.0 |
| 1 Ggas | 16 | 39,832 | 84% | 0.18 | 5,478 ms | 7,272 | 479.3 | **0.3** | 210.1 |

**FINDING 1 — on the fleet, THROUGHPUT SATURATES near 7-8k tx/s and only LATENCY changes.** Across
a 5x block-size range (200M -> 1 Ggas) tps moves 7,150 -> 7,272 (i.e. not at all, within variance)
while latency goes 864 ms -> 5,478 ms, **6.3x worse**. Bigger blocks buy nothing on real hardware.
**So the right operating point is the SMALLEST block that reaches the plateau: 200M at ~864 ms.**

**FINDING 2 — the 9.5k/4.7k gap was hardware + block size, not a regression.** Single box runs all
four validators (12 containers) on 16 shared cores and every validator re-executes every block; the
fleet gives each validator its own machine. At the same 200M the fleet does **7,150 tps @ 864 ms**
vs the single box's 6,785 @ 1,404 ms — same throughput plateau, **1.6x better latency**.

**FINDING 3 — OVER-OFFERING LOAD IS COUNTERPRODUCTIVE, quantified.** Doubling spam at 200M (16 ->
32 spammers) filled blocks 65% -> 100% and bought +6% tps (7,150 -> 7,616) while costing +45%
latency (864 -> 1,250 ms). Filling the block is NOT the goal; the ingress cost of the surplus
(admission + gossip for txs that will not fit) exceeds what the extra fullness returns. This is the
same effect measured single-box (50M: 1.96 blk/s light-load vs 1.47-1.61 saturated) and it means
**"100% full" is the wrong success criterion for a latency-sensitive lane.**

**FINDING 4 — the paper's thesis gets STRONGER at scale.** State root on a 39,832-tx block:
**0.3 ms.** On 15,430 txs: 1.0 ms. The commitment cost stays flat-to-negligible as blocks grow by
5x, exactly as claimed. Execution is what scales (61.8 -> 479.3 ms).

Today's 1 Ggas number (7,272 tps) is somewhat below the recorded 9.5k avg / 15k peak; that run was
72% full at 34,363 txs/blk against today's 84% at 39,832. Different chain age and machine state;
both sit on the same 7-9k plateau. Peak-vs-average also differs — 15k was a peak, 7.3k here is a
120 s average.

## 🎯 2-D SWEEP: BLOCK TIME x GAS — BLOCK TIME IS NOT A THROUGHPUT KNOB (2026-08-09, iteration 3)

First sweep where EVERY point is 100% full: distributed load (2 local + 8 ginnythui + 5
papaduck-alien2 spammers over tailscale, against this box's payment EL). Local-only load saturates
at ~6,000 tx/s and could never fill 100M+ blocks, which is what invalidated every previous
high-gas row. Harness: `experiments/dual-el/blocktime-sweep.sh`, chart `blocktime-chart.py`.

| target | gas | txs/blk | blk/s | latency | tps | exec | root | persist | verdict |
|--------|-----|---------|-------|---------|-----|------|------|---------|---------|
| 250 ms | 50M | 2,380 | 1.61 | 620 ms | 3,840 | 33.7 | 30.5 | 63.0 | missed |
| 250 ms | 100M | 4,761 | 1.10 | 910 ms | 5,234 | 83.9 | 44.5 | 86.5 | missed |
| 250 ms | 200M | 9,523 | 0.71 | 1404 ms | **6,785** | 174.1 | 47.9 | 127.2 | missed |
| 500 ms | 50M | 2,380 | 1.47 | 680 ms | 3,500 | 32.5 | 31.5 | 160.2 | missed |
| 500 ms | 100M | 4,761 | 1.06 | 947 ms | 5,025 | 78.5 | 44.1 | 161.6 | missed |
| 500 ms | 200M | 9,523 | 0.68 | 1460 ms | 6,522 | 160.6 | 51.7 | 243.7 | missed |
| 1000 ms | 50M | 2,380 | 1.00 | 1001 ms | 2,378 | 21.7 | 20.7 | 66.1 | **HELD** |
| **1000 ms** | **100M** | **4,761** | **1.00** | **1003 ms** | **4,748** | 75.5 | 45.7 | 111.1 | **HELD** |
| 1000 ms | 200M | 9,523 | 0.67 | 1492 ms | 6,382 | 162.7 | 49.1 | 265.4 | missed |

**FINDING 1 — the target block time buys NOTHING.** Under saturation the chain runs at its natural
cadence, set by the GAS LIMIT. Compare the same gas across targets: 50M gives 1.61 / 1.47 / 1.00
blk/s at 250 / 500 / 1000 ms. Asking for 250 ms instead of 500 ms changes nothing (the chain is
already slower than both); asking for 1000 ms actively THROTTLES it (50M could do ~1.5 blk/s and is
paced down to exactly 1.00, costing ~1,100 tps). **`targetBlockTimeMs` is a ceiling, never a floor.**
It is a product/latency-predictability knob, not a performance one.

**FINDING 2 — the gas limit IS the frontier, with steep diminishing returns.** Natural cadence
(averaging the unpaced 250/500 rows): 50M ~1.54 blk/s / ~3,670 tps; 100M ~1.08 / ~5,130; 200M ~0.70
/ ~6,650. So **4x the gas buys 1.8x the tps and costs 2.2x the latency.**

**FINDING 3 — offered load beyond what fills a block STILL costs cadence.** At 50M with 6 LOCAL
spammers the chain did 1.96 blk/s; at the same 50M and the same 100%-full blocks, with 15
distributed spammers, it does 1.47-1.61. Block composition is identical, so the delta is pure
INGRESS cost: RPC/mempool admission and gossip for transactions that will not fit anyway. This
retro-explains why the earlier "40M holds 2 blk/s" reading was obtained under light load — it is
real, but it is a light-load number.

**FINDING 4 — the only configuration that both saturates and HOLDS its target is 100M @ 1000 ms:
4,748 tps at a stable, predictable 1.00 blk/s**, with ~8% cadence headroom (natural ~1.08). That
headroom is what makes it hold. 200M is faster on paper (6,785 tps) but holds no target at all and
lands at 1.4-1.5 s blocks.

**ANSWER TO "best tps at 1 blk/s": 4,748 tps at 100M gas, held at exactly 1.00 blk/s.**
**ANSWER TO "2 blk/s": NOT reachable at any gas size under saturating distributed load** — the
fastest saturated point in the whole sweep is 1.61 blk/s (620 ms) at 50M, and that one misses its
own 250 ms target. 2 blk/s remains reachable only at <=40M with light offered load.

## ⚠️ REQUALIFICATION + THE LOAD GENERATOR IS NOW THE CEILING (2026-08-09, iteration 2)

**50M does NOT reliably hold 2 blk/s — it is MARGINAL.** A reverse-order sweep on a fresh chain
(60 -> 55 -> 50, so the biggest block got the freshest chain) gave:

| gas | forward sweep | reverse sweep |
|------|---------------|---------------|
| 50M | **1.96 blk/s / 511 ms** | **1.72 blk/s / 582 ms** |
| 55M | 1.69 / 591 ms | 1.62 / 615 ms |
| 60M | 1.75 / 573 ms | 1.58 / 631 ms |

Same size, same load, same config, ~12% apart. So the earlier chain-age suspicion was WRONG — the
reverse order ruled it out — and the real explanation is plain run-to-run variance (this box has
always shown +-15%). **The reproducible holding point is 40M: 1.98 blk/s, 504 ms, 3,777 tps.**
50M should be quoted as "marginal, 1.7-2.0 blk/s", not as the ceiling. The headline from iteration 1
was over-fitted to a single run.

**THE 1 blk/s QUESTION CANNOT BE ANSWERED ON THIS BOX — the spammer, not the chain, is the limit.**
Sweep at 100/200/300/400M with **12** spammers:

| gas | txs/blk | blk/s | tps | %full |
|------|---------|-------|------|-------|
| 100M | 3,183 | 1.72 | 5,468 | **67%** |
| 200M | 3,807 | 1.53 | 5,831 | **40%** |
| 300M | 5,393 | 1.15 | 6,179 | **38%** |
| 400M | 3,649 | 1.60 | 5,832 | **19%** |

Every row is delivery-bound, and doubling spammers 6 -> 12 barely moved delivered tps (5,186 ->
~5,500-6,200): local load generation saturates around **~6,000 tx/s** because the spammers compete
with 12 containers for 16 cores. This reproduces the mission-1 finding that MORE spammers made it
worse. Those cadence numbers (e.g. 300M at 1.15 blk/s) are produced by PARTIAL blocks and are
therefore NOT capacity measurements — they must not be quoted as a 1 blk/s result.

To answer it properly needs distributed load (fleet/spam-fleet-distributed.sh). Attempted; blocked
because `tailscale ssh` now requires interactive re-auth. ginnythui and papaduck-alien2 are online,
papaduck is absent from the tailnet.

**BEST SATURATED tps MEASURED TO DATE: 7,164 tps at 100M / 665 ms / 1.50 blk/s, 100% full** (the
earlier 4-spammer coarse sweep). Notably HIGHER than today's 12-spammer attempt at the same size —
more load generators made the measurement worse, not better.

## 🎯 MISSION 3 RESULT: 50M GAS HOLDS 2 blk/s AT 4,660 TPS — 1.97x THE BASELINE (2026-08-09)

Goal was "a block larger than 25M that still holds 2 blk/s". Achieved, and the win came from
FIXING THE MEASUREMENT, not from optimising anything.

Fine-grained sweep, one chain, gas flipped at runtime, all 4 payment ELs on
`--engine.state-root-fallback`, 75 s windows, **6 spammers so every point is load-saturated
(100% full)**:

| gas | txs/blk | blk/s | latency | tps | exec | root | persist | |
|------|---------|-------|---------|------|------|------|---------|--|
| 25M | 1,190 | 1.99 | 504 ms | 2,363 | 3.8 | 10.5 | 49.0 | HOLDS (prior sweep) |
| 30M | 1,428 | 1.99 | 504 ms | 2,835 | 15.0 | 12.5 | 54.9 | HOLDS |
| 40M | 1,904 | 1.98 | 504 ms | 3,777 | 28.0 | 19.3 | 61.0 | HOLDS |
| **50M** | **2,380** | **1.96** | **511 ms** | **4,660** | 42.4 | 27.8 | 70.4 | MARGINAL — see requalification above (1.72 on repeat) |
| 55M | 2,618 | 1.69 | 591 ms | 4,427 | 49.2 | 30.9 | 157.0 | degraded |
| 60M | 2,856 | 1.75 | 573 ms | 4,986 | 57.3 | 31.7 | 77.3 | degraded |
| 75M | 3,571 | 1.45 | 689 ms | 5,186 | 64.3 | 34.8 | 165.2 | degraded |

**The old 25M answer was an artefact of under-delivery.** The previous sweep used 4 spammers; at
50M it read 782 ms / 1.28 blk/s with persist "spiking" to 448.6 ms. Re-measured with 6 spammers:
**511 ms / 1.96 blk/s, persist 70.4 ms.** The spike was noise, and it was the entire basis for
lead #1. Doubling deliverable throughput at the latency target required no code at all.

CONFOUND (stated, not smoothed): sizes are swept sequentially on a GROWING chain, so later points
carry more state. 55M ran last (~1,700 blocks in) and came out worse than 50M in BOTH tps and
latency — some of that is chain age, not size. The true ceiling on a fresh chain may sit slightly
above 50M. Above 50M tps also flattens hard (4,660 -> 4,986 -> 5,186) while latency climbs, so
50M is close to optimal on both axes regardless.

## ❌ LEAD #1 (PERSISTENCE) — CLOSED, IT MAKES THINGS WORSE (2026-08-09)

Same sweep sequence re-run with `--engine.persistence-threshold 64 --engine.memory-block-buffer-target 128`
on all 4 (a storage-timing knob; it cannot change execution semantics, and a cadence experiment
requires all validators since cadence is a chain property):

| gas | default | tuned |
|------|---------|-------|
| 30M | 1.99 blk/s, 2,835 tps | 1.93, 2,758 |
| 40M | 1.98, 3,777 | 1.65, 3,146 |
| 50M | **1.96, 4,660** | 1.36, 3,232 |
| 60M | 1.75, 4,986 | 1.13, 3,235 |

**Worse at every size, and the mechanism is visible: STATE ROOT went 2-3x more expensive**
(12.5/19.3/27.8/31.7 -> 43.6/63.8/61.0/69.6 ms) while persist did NOT drop. Holding 64-128 blocks
in memory forces root computation to walk a much deeper in-memory overlay. Deferring persistence
relocates cost into state root.

Supporting evidence that persistence was never the limiter: fsync on this box is **1.28 ms/op**
(4k dsync), persist is async, and persist never correlated with cadence in the original data
(50M persist 448 ms -> 782 ms cadence, but 100M persist 74.9 ms -> 665 ms).

## 🚨 FOURTH CONSENSUS BUG: ZERO-VALUE TRANSFERS EMITTED A LOG — FOUND BY THE NEW GATE (2026-08-09)

Clearing the correctness debt (EIP-2930 with empty access list + zero-value transfers added to
`Workload::Mixed`) immediately caught a fourth fork: state digests matched, **receipts did not** --
stock 6,538 logs vs fast path 7,000. The 462 difference was exactly the zero-value transfers.

`ArcEvm::before_frame_init` only reaches the log builder through
`Some((from, to, amount)) if !amount.is_zero()` — **a zero-value transfer emits NOTHING.** The fast
path emitted one unconditionally. Same silent-fork shape as the missing-log and legacy-fee bugs:
receipts move, state does not.

FIXED (`executor.rs`): return no logs when `value.is_zero()`, ahead of the hardfork split.
Both gates now agree on every digest. EIP-2930-with-empty-access-list needed no fix — the earlier
`effective_gas_price` change already covers it.

Scope note: this fix is OFFLINE-validated. The live spammer never sends zero-value transfers, so a
live run would not exercise it; the gate is the appropriate test. The flag remains OFF by default.

**FOUR bugs, ONE shape.** Every fast-path bug came from assuming what a transaction is instead of
asking: assumed logs always, assumed 1559 fees, assumed a log for every transfer. Three of the four
were invisible to state comparison alone.

## 🚨 SECOND CONSENSUS BUG IN THE FAST PATH: LEGACY-TX FEES — FOUND, FIXED (2026-08-08)

Hunting the same class of blind spot as the log bug (both gates only ever ran **pure EIP-1559
transfer** blocks) turned up a second fork.

Live mixed load (`--mix transfer=60,erc20=25,guzzler=10,legacy=5`), val1 fast path vs val2-4 stock:
val1 diverged at block 50 on the **STATE ROOT** (not receipts) and stalled at 49 while the network
ran on to 125.

```
mismatched block state root:
  got 0x552d6799…  expected 0x8a4de677…
```

**ROOT CAUSE:** the fast path hardcoded EIP-1559 fee shape:
```rust
effective = min(max_fee_per_gas, basefee + max_priority_fee_per_gas().unwrap_or_default())
```
A **LEGACY (type 0) transfer with no calldata is fast-path eligible**, but legacy has no priority
field, so `unwrap_or_default()` = 0 and the formula collapses to `min(gas_price, basefee)` =
**basefee**. The correct effective price for legacy is `gas_price` outright. The sender was
under-charged and the beneficiary (which Arc credits the FULL fee) under-credited. Gas stayed
21,000 and the log was unchanged, so **receipts matched perfectly and only the state root moved** —
invisible to the receipts digest added earlier that same day.

**FIX:** ask the transaction for its own price instead of assuming a shape —
`tx.effective_gas_price(Some(basefee))`. Correct for legacy, 2930 and 1559 by construction.

**GATE EXTENDED — `Workload::Mixed`:** 7,000 txs where every 7th carries calldata (forcing the
general-EVM path, which flushes the overlay and drops the caches) and every 5th of the rest is
LEGACY. Prints a post-state digest so the two gate runs can be compared directly. It reproduced
the bug immediately (`0xefd6…` stock vs `0x1769…` fast path) and matches after the fix. Note the
calldata/invalidation half alone did NOT reproduce anything — the overlay/cache invalidation is
fine; it was purely the fee shape.

**VALIDATED:** rebuilt, re-ran the same mixed live load — 200 consecutive blocks, 497,531 txs,
composition confirmed mixed (19,038 type-2, 1,032 type-0, 6,885 with calldata), all 4 identical on
stateRoot + blockHash + receiptsRoot + logsBloom, zero invalid blocks.

**PATTERN — three bugs, one shape.** Every fast-path bug so far came from *assuming* what a
transaction is instead of asking it: assumed no logs, assumed 1559 fees. The gate now covers
state + receipts + logs + legacy + general-EVM interleaving. Remaining unmodelled shapes that are
fast-path *eligible* and still uncovered: EIP-2930 with an empty access list, and zero-value
transfers. Worth adding before this flag is ever considered for default-on.

## 🚨 CONSENSUS BUG IN THE FAST PATH — FOUND, FIXED, AND THE "1.58x" WAS MOSTLY THE BUG (2026-08-08)

**`ARC_PARALLEL_TRANSFERS=1` was forking the chain against unmodified nodes.** Arc emits a log for
EVERY native value transfer (`ArcEvm::before_frame_init` -> `crate::log`). The fast path bypasses
the interpreter and emitted **none**, so its receipts and the block logs bloom differed from stock
while post-state was byte-identical.

Found by running the first-ever A/B against UNMODIFIED peers (val1 fast path, val2-4 stock). val1
rejected the first non-empty block outright:

```
Invalid block error on new payload number=100
validation_err=receipt root mismatch:
  got 0xe67e6405... (val1, fast path)  expected 0xafdca43b... (network, stock)
```

**Why every previous validation missed it — two independent blind spots:**
1. The offline gate compared post-STATE only. Receipts carry type/status/cumulative-gas/**logs**;
   all of those can differ with state intact.
2. Every live run enabled the fast path on ALL FOUR validators via the global `PAY_EL_ENV`. Four
   nodes running the same modified code agree with each other perfectly — and all four diverge
   from stock. **An optimisation must be A/B'd against unmodified peers, never against copies of
   itself.**

Beyond consensus this also silently dropped the `Transfer` events wallets and indexers consume.

**FIX** (`executor.rs`): emit the same log the interpreter would — EIP-7708 `Transfer` from Zero5
onward with self-transfers suppressed, legacy `NativeCoinTransferred` before that — reusing
`crate::log` and the same `is_arc_fork_active` gate so it is correct by construction.

**GATE STRENGTHENED**: `parallel_transfer_bench` now prints a receipts digest + log count. Before
the fix: stock `0x93a9…`/`0x9c0b…` with 47,618 logs vs fast path `0x21e2…` with 0 logs (and the
same digest for both workloads — the tell, since receipts should depend on the recipients). After:
digests match exactly. **Both gates must now agree on the receipts digest, not just state.**

**HONEST PERF — the fast path is worth ~3%, not 1.58x.** Once it correctly builds the 4,761 logs
per block, simultaneous same-block A/B (val1 fast path vs 3 stock):

| metric | stock | fast path | ratio |
|--------|-------|-----------|-------|
| exec (sub-metric) | 87.1 ms | 79.5 ms | 1.10x |
| per-tx exec | ~7.6 us | 3.77 us | 2.0x |
| **newPayload (the metric)** | **118.3 ms** | **115.0 ms** | **1.03x** |

The previously-claimed 1.39x-1.58x was substantially the cost of the logs it was not emitting.
Log construction (`encode_log_data`) is a large share of what revm was doing for a transfer. At
1.03x of newPayload — itself ~16% of a height — this is ~0.5% of block time. **Keep it gated OFF
by default; it is not worth deploying for 0.5%.**

**VALIDATED AFTER THE FIX:** 200 consecutive blocks / 952,200 txs, mixed config (val1 fast path vs
3 stock peers), all 4 identical on stateRoot + blockHash + receiptsRoot + **logsBloom**, zero
invalid-block errors.

This retroactively invalidates the "✅ MISSION COMPLETE — 1.58x, 120 heights zero divergence"
claim from mission 1: that run had the fast path on all 4 machines, so it proved self-consistency,
not correctness.

## ❌ EAGER RECOVERY WAS A MEASUREMENT ARTIFACT — REVERTED (2026-08-08)

**The previous iteration's "3.9x faster execution phase" was not real.** It was measured with
`transaction_wait` + `transaction_execution`, which do NOT sum to the cost of `newPayload`. Eager
recovery moved work OUT of the execution loop (into iterator construction) where those two
histograms cannot see it.

Caught by decomposing the height against an independent metric,
`reth_consensus_engine_beacon_new_payload_latency`. Same box, same 4,761-tx blocks, **simultaneous**
window, eager on val1 only, all four on `--engine.state-root-fallback`:

| val | newPayload | exec | root | other | wait/tx |
|-----|-----------|------|------|-------|---------|
| 1 (eager) | **85.9 ms** | 19.9 | 22.7 | **43.2** | 0.94 us |
| 2 | 87.5 ms | 58.2 | 25.1 | 4.1 | 8.72 us |
| 3 | 81.1 ms | 55.1 | 22.3 | 3.8 | 8.15 us |
| 4 | 81.7 ms | 55.4 | 22.6 | 3.7 | 8.50 us |

Execution fell 58.2 -> 19.9 ms (-38) while `other` rose 4.1 -> 43.2 ms (+39). **Total newPayload
was unchanged within noise** (85.9 vs 81.1-87.5). Exactly offsetting.

**WHY: the `wait` histogram is pipeline OVERLAP, not waste.** reth streams recovery concurrently
with execution, so the consumer's stall is time recovery is genuinely still working — overlapped
with useful work. Recovering eagerly converts that overlap into a serial barrier before execution
starts and returns precisely what it saves. `recovery_bench.rs` remains correct about the crypto
(33.4 us/tx serial, ~4 us/tx on 16 threads) — the wrong step was assuming the live gap was idle.

REVERTED in full (`crates/evm/src/evm.rs` back to plain delegation, rayon dep dropped). Kept: the
`recovery_bench.rs` and `recovery-probe.sh` harnesses, the `PAY_EL<i>_ENV` per-validator env hook
(genuinely useful for same-box A/B), and this record. Post-revert: both gates IDENTICAL, 200
consecutive blocks / 952,200 txs with all 4 validators agreeing.

**RULE ADDED TO THE PROTOCOL: an EL optimisation only counts if
`reth_consensus_engine_beacon_new_payload_latency` moves.** Sub-metrics can be relocated.

## ❌ Hypothesis #1 (engine-API ingestion) — REFUTED, and it was the top-ranked suspect

`experiments/dual-el/height-decomp.sh` (new) splits a height using reth's beacon-engine metrics.
At 100M gas / 4,761 txs, stock config, 715 ms height:

| phase | ms | % | who |
|-------|-----|---|-----|
| newPayload (EL busy) | 113.3 | 15.8 | exec 78.2 + root 29.3 + **other 5.8** |
| newPayload -> FCU (EL IDLE) | 342.5 | 47.9 | CL voting round |
| forkchoiceUpdated (EL busy) | 1.0 | 0.1 | EL |
| remainder (EL IDLE) | 258.6 | 36.2 | next proposer build + SSZ + streaming |

**EL busy 16%, EL idle 84%.** The "unaccounted" slice inside newPayload — the only place a hidden
JSON-decode/ingestion cost could live — is **5.8 ms**, ~0.8% of the height. There is no hidden
ingestion cost to recover, so **switching the payment lane to IPC cannot move cadence** and STEP 2
is closed. (CL<->EL is localhost in both the single-machine and fleet topologies, so transport
before reth's timer starts is bounded small too.)

This also independently reconfirms the 10/90 EL/non-EL split from a completely different metric
family than the earlier measurement.

## 🎯 Block-size sweep at fixed 2 blk/s — RE-RUN AND VALID (2026-08-08)

Harness fixed (all 3 bugs below), re-run on one chain with **all 4 payment ELs on the best-known
EL config** (`ARC_PARALLEL_TRANSFERS=1` + `ARC_EAGER_RECOVERY=1` + `--engine.state-root-fallback`),
runtime gas-limit flips, 4 local spammers, 75 s windows.

| gas | txs/blk | blk/s | ms/blk | tps | %full | exec | root | persist | |
|------|---------|-------|--------|------|-------|------|------|---------|--|
| 25M  | 1,190 | **1.99** | **504** | 2,363 | 100% | 3.8 | 10.5 | 49.0 | **HOLDS** |
| 50M  | 2,380 | 1.28 | 782 | 3,043 | 100% | 9.9 | 18.6 | 448.6 | degraded |
| 100M | 4,761 | 1.50 | 665 | **7,164** | 100% | 25.7 | 27.5 | 74.9 | degraded |
| 200M | 6,805 | 1.15 | 873 | 7,795 | 71% | 30.3 | 30.0 | 86.9 | degraded |
| 500M | 9,732 | 0.65 | 1532 | 6,353 | 41% | 39.3 | 31.7 | 802.8 | BROKEN |
| 1G   | 8,143 | 0.96 | 1043 | 7,810 | 17% | 39.0 | 33.7 | 111.8 | BROKEN |

**ANSWER TO THE MISSION GOAL: 2 blk/s is achievable at 25M gas — 1,190 tx/block, 504 ms, 2,363 tps.**
Everything larger trades latency for throughput, and the trade is set by consensus coordination, not
by the EL.

- **The EL is never the constraint at ANY block size.** Synchronous EL work (exec + root) is
  14.3 ms at 25M and only 72.7 ms at 1 Ggas — at most ~8% of block time, ~3% at the target. After
  eager recovery, execution at the 2 blk/s point is **3.8 ms/block**. There is no cadence left to
  win inside the EL; this closes the optimisation line the mission opened.
- **Throughput plateaus at ~7-8k tps** from 100M upward. Bigger blocks stop buying tps and only add
  latency — consistent with the earlier finding that the dominant per-height cost scales with
  TRANSACTION COUNT (SSZ encode + proposal streaming + voting), not with block count.
- **HONEST CAVEAT — the ≥200M rows are DELIVERY-bound, not chain-bound.** Blocks there are only
  71/41/17% full, so 4 local spammers could not offer enough load; those rows measure the spammer,
  not the lane. Only the 25M/50M/100M rows (100% full) are chain-limited. The fleet with
  distributed spam previously reached 9.5k avg / 15k peak at 1 Ggas.
- **The 50M row is noise**, not signal: persist spiked to 448.6 ms and its 1.28 blk/s is worse than
  100M's 1.50 at half the size. Disk stall on this box during that window. persist is the noisiest
  column throughout (49 → 803 ms, non-monotonic in block size).
- **Practical recommendation:** 25M for a latency demo (true 2 blk/s), 100M for a throughput demo
  (7.2k tps at 665 ms — 3x the tps for 33% more latency).

Consensus: 300 consecutive blocks / 1,582,375 txs across the whole sweep, **all 4 validators
identical on stateRoot + blockHash + receiptsRoot at every height**, with all four running eager
recovery (previously only val1 did).

### The 3 harness bugs (fixed in blocksize-sweep.sh; the original run produced a "3026% full" row)
1. polled a hardcoded val1 that had parked → read STALLED while the chain ran on 3-of-4. Now polls
   whichever validator has the highest head.
2. applied the gas-limit change *while saturating load ran*, so the governance tx was starved from
   the mempool and the limit silently never changed. Now load is STOPPED for the change and the new
   limit is VERIFIED on a fresh block header before measuring.
3. host map was fleet-only. Now defaults to the single-machine demo, `FLEET='{"1":"ip",...}'` for
   the 4-machine case.

## Block-size sweep at fixed 2 blk/s — ATTEMPTED, INVALID, 3 harness bugs found (2026-08-08)

Goal: hold latency at Arc mainnet's 500 ms and find the largest payment block the CL sustains.
`experiments/dual-el/blocksize-sweep.sh` sweeps the on-chain gas limit at runtime. **The run produced
numbers, and they are WRONG — do not quote them.** Tell-tale: "3026% full", i.e. gasUsed far above
the limit the row claimed to test. Every row actually measured 1 Ggas.

Three separate harness bugs, all now understood:
1. **Polled a hardcoded val1.** val1's CL parked, so the sweep read STALLED at every size while the
   chain ran fine on 3-of-4 quorum. FIXED: poll the highest-head validator.
2. **The governance tx never landed under saturating load.** `updateFeeParams` competes with ~47k
   spam txs that all pay the SAME fixed 20 gwei (our own fixed-fee design), so it is effectively
   FIFO behind a huge backlog and never gets included. => the sweep must set the gas limit with
   load OFF, then apply load, then measure — per size.
3. **`set-lane-economics.sh` targets `127.0.0.1`** = val1 only. With val1 parked, the controller tx
   went to a node not following the chain and sat there forever. => point governance txs at a
   HEALTHY validator (or heal val1 first); consider an env override for the RPC target.

NOTE this is a genuine operational finding, not just a test artifact: **on a saturated fixed-fee
lane, governance transactions cannot get in.** A fixed fee removes the fee market that would
normally let an urgent tx bid its way in. Worth a design note — an exempt/priority path for
controller txs, or admin submission via a reserved lane.

NEXT ITERATION should re-run the sweep as: for each size -> stop load -> set gas limit against a
healthy validator -> verify it took -> start load -> measure 70 s -> record.
