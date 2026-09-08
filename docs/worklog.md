# Work log

Append-only, one entry per session, newest at the bottom. Convention in
`CLAUDE.md` §8. Numbers without cadence + fullness are not results.

---

## 2026-08-25 — teardown, repo cleanup, planning

**Changed**
- `fe5c2bf` CLAUDE.md rewritten as a working document (2,009 → 226 lines); full
  history preserved verbatim in `docs/campaign-log.md`.
- `a598120` `lean-smoke.sh` — single-machine proof the repo runs the lane alone.
- `f007c73` `fleet.env` + `deploy-lean.sh` (sha-verified *after* transfer).
- `1bdfd39`/`666c5ad` lean lane moved into this workspace as `crates/lean-native`
  + `crates/lean-lane-node`, building against upstream reth v2.3.0; setup guide.
- `docs/roadmap.md` — scope and plan (this session's main deliverable).
- Reverted an uncommitted `assets/localdev/genesis.json` — generated-file churn
  from a demo run with different `EXTRA_ACCOUNTS` (1,293 → 294 accounts), not an
  intentional change.

**Measured**
- `lean-smoke.sh`: 40 heights, 9,856 txs, 98,560 payments, 3/3 nodes at one
  commitment; second run 25 heights, 6,336 txs — repeatable.
- Repo-built lean binary reproduces the fork binary's genesis commitment
  `0xef24da0138bf3715…` — same wire format and semantics.
- Fleet torn down; ~2.4 TB reclaimed (papaduck 63 GB of it was 37 dangling arc
  images from image ships).

**Broke / retracted**
- Nothing new. Standing retractions from 2026-08-24 (sick-validator N=1
  diagnosis; non-byte-normalised N=1 row) are recorded in the notebook and
  `campaign-log.md`.

**Decided**
- The reth fork is history: nothing in this repo depends on `~/reth-fork`.
- Historical `experiments/dual-el/fleet/*` launchers stay hardcoded as campaign
  history; the live path is parameterised.

**Found while planning (new, unverified consequences)**
- **Beneficiary is node config, not block data** (`Address::with_last_byte(0xbe)`)
  — consensus-critical input outside the block; differing config = silent fork.
- **Invalid transactions pay no fee** — with total STF, a byzantine proposer can
  include invalid txs for free, costing every validator bytes + ecrecover.
- **Transactions do not propagate between lean nodes** — only blocks do. Every
  benchmark fed all nodes directly, which hid this.
- No cross-lane value flow exists: the payment lane is an island.

**Open**
- Decisions needed: checkpoint state root (yes/no/how often); header `version` +
  `proposer` fields; invalid-tx fee policy.
- Next up per `docs/roadmap.md`: beneficiary fix, then single-machine runner.

**Decisions (Duc, same day)**
- Checkpoint state root: **yes**, designed **lagged** (block N carries the root as
  of the last boundary ≤ N−K) so neither proposing nor voting waits on it;
  verification happens at execution, mismatch = attributable halt. Use an
  incremental accumulator, not an O(n) walk — the flat state grows with users.
- Header gains **`version` + `proposer`** (one wire-format break, done with the
  checkpoint field).
- Invalid-tx fee: **charge the proposer**; do not reject the block, do not halt.
- Payment lane **may run at a different cadence** from the EVM lane (product is
  fine with 1 or 0.5 blk/s for payments).

**BRAINSTORM — cadence analysis (not measured, see roadmap §7)**
- The lane is **pacer-bound, not capacity-bound** today: 150M→225M all sit at
  ~518 ms/1.93 blk/s; only 250M slips (538 ms). Free +7 % by moving to 250M.
- Local fit height ≈ 343 ms + 0.14 µs/B extrapolates 1 blk/s → 162 k payments/s,
  but that same law underpredicts the measured 525M drain (710 ms predicted vs
  **1,155 ms measured**, 1.6× off) — superlinearity above ~1.5 MB. Honest range
  for 1 blk/s: **87 k–160 k, most likely ~110 k**. Must be measured, not assumed.
- Agreement risk if cadence slows: **`propose` timeout is 3,000 ms and does not
  scale automatically** — at 3–4.6 MB payloads a round can miss it and cadence
  collapses. Timeouts are on-chain params; raise them with the cadence.
- Per-lane cadence needs no new consensus machinery (`lean_payload` is already
  `Option`, `commit_lanes(evm, None)` is the EVM-only case) — just a deterministic
  `height % K` proposer rule, plus timeouts sized for the expensive height.

**Open (next session)**
- Run the cadence experiment (roadmap §7.2) before building per-lane cadence.

## 2026-08-25 (later) — mixed-N + parallel-recovery plan; bench side-by-side

**Measured (offline bench, this box, 16 rayon threads, 100k outputs/config)**
- ecrecover serial→parallel: N=1 30.3→3.9 µs/out (7.8×) · N=10 3.1→0.39 · N=100
  0.36→0.05. Execution serial→parallel: N=1 0.33→0.57 (SLOWER), N≥10 ~equal.
  ⇒ parallelize recovery only; execution stays serial.
- Verified in source: vote-gap staging IS wired (payload.rs:563 → arc_stageBlock;
  anchor ladder 145→95→44 ms already measured). Node's decode_block_txs +
  apply_block are both serial today; lean-native's rayon paths are unused.

**Decided (Duc)**
- Stay at 2 blk/s. Mixed-N default = equal-by-tx over {1,5,10,50,100}; keep
  equal-by-payments as the stress mix. Focus next on verification; local first.

**Planned** — roadmap §8: P1 spammer weighted `--fanout-outputs`, P2 env-gated
parallel recovery in decode_block_txs, P3 lane-bench mix spec + fullness-by-gas +
avg_n/sigs_blk, P4 smoke --mixed. Verification ladder V1–V7 (V1–V6 fully local;
only V7 touches the fleet). Adoption rule: flag default-on only after V3–V6 + 2 h
local soak with zero divergence.

**Open** — implement P1→P4, run V1→V6.

## 2026-08-25 (execution) — P1-P4 built, V1-V5 green, fork mirrored

**Changed**
- `a988b92` (arc): spammer weighted `--fanout-outputs "N:W,..."` (deterministic
  nonce-indexed cycle, exact shares — 5 unit tests); node parallel recovery
  behind `LEAN_PARALLEL_RECOVERY=1` (recovery only — bench showed parallel
  execution is neutral-to-slower); V3 differential test (3 tests: mixed blocks
  with garbage/truncated/empty/corrupted-sig entries → identical item vectors,
  including agreement on the WRONG sender a corrupted sig recovers to);
  `lean-smoke.sh --mixed` (V5 avg-N check + V4 replay-invariance node);
  lane-bench mix spec + fullness-by-gas + sigs_blk/avg_n columns.
- `96faa4d` (~/reth-fork, lean-prune): same node patch + test mirrored, builds,
  3/3 tests green there too.

**Measured (V-gates, all local)**
- V1: 81 spammer tests pass (5 new). V3: 3/3 differential green in both repos.
- V4+V5: `lean-smoke.sh --mixed 40` → 40 heights, 4,162 txs, 122,332 payments,
  avg N=29.4 (band [28,38]), 3/3 nodes one commitment, and the fresh
  parallel-recovery replay node reached the IDENTICAL head commitment.
- Offline bench (16 threads): ecrecover 30.3→3.9 µs/out at N=1 (7.8×);
  parallel execution 0.33→0.57 µs/out at N=1 (slower — stays serial).

**Open**
- V6: single-box full testnet, six 10-min arms {pure-100, mixed-by-tx,
  mixed-by-payments} × {serial, parallel} — the mechanism test (gap must appear
  ONLY in mixed-by-payments serial). Then 2 h mixed soak before the flag
  defaults on. V7 (fleet no-regression + numbers) after that.

## 2026-08-26 — V6 run: 4 clean arms, bypay unmeasurable on one box, 3 CL fatal classes found

**Measured (single box, 5 validators, 225M, 10-min arms, burns self-reported)**
| arm | cadence | payments/s | sigs/blk | anchor p50 | full | burns |
| pure100-serial   | 2.41 | 103,799 | 431   | 39ms | 100% | ~0 |
| pure100-parallel | 2.57 | 110,695 | 431   | 40ms | 100% | ~0 |
| bytx-serial      | 2.21 |  88,405 | 1,202 | 39ms | 100% | ~0 |
| bytx-parallel    | 2.52 | 100,514 | 1,202 | 41ms | 100% | tail-contaminated (understates if anything) |
- Parallel recovery on clean arms: +6-14% cadence, anchors identical. Real but
  modest at ≤1.2k sigs/blk. Consensus-identical (V3/V4) and safe.
- **bypay (5.7k sigs/blk): UNMEASURABLE on this box — 3 attempts.** All burned
  rounds attribute to val5 every time (331/498, 198/199, 205/205). Sequence:
  bypay-scale feed starves val5's EVM EL -> PayloadStatus::Syncing at
  validation -> CL treats it as FATAL -> docker restart-loop -> parks or wedges
  as a stale-height crash-loop (sync-path validation also fatal). nice -12 on
  all host-side load did not save it: once wedged, the crash-loop is
  self-sustaining. Clean bypay verdict deferred to the fleet (V7).
- Burn-adjusted (indicative only): successful bypay heights average ~0.95s vs
  ~0.45s bytx — the sig-heavy workload is ~2x slower per height even ignoring
  the sick validator.

**Broke / found**
- **THREE CL fatal-validation classes documented in one day** (all should fail
  the ROUND, not the PROCESS): (1) lean shim unreachable at validation,
  (2) EVM EL returns Syncing at validation, (3) sync-path validation failure.
  Each converts a transient hiccup into a permanently wedged validator via the
  restart-loop + boot-park. TOP robustness item for the CL.
- compose template emits dual-EVM payment args unconditionally -> `make
  testnet` cannot boot from a fresh checkout (payment-jwt.hex missing) — fixed
  locally in the generated compose; template fix owed.
- CL services lack extra_hosts host-gateway (only ELs have it).

**Decided**
- LEAN_PARALLEL_RECOVERY stays DEFAULT OFF: adoption rule required V6 pass;
  clean arms show only modest gains and the stress arm is untested. Revisit at V7.
- Every measurement row now carries a burns count (self-validating rows).

**Open**
- V7 on the fleet: bypay pair + no-regression band; then flag decision.
- CL robustness: make validation-time dependency errors round-fatal only.
- Single-box testnet left RUNNING (5 validators + 5 lean nodes, idle) for
  further local arms; teardown = docker compose -f .quake/localdev/compose.yaml
  down + pkill lean-lane-node.

## 2026-08-26 (evening) — V7 on the fleet: mechanism CONFIRMED, 4.1x at the sig-heavy mix

**Measured (4-machine fleet, 10-min arms, burns=0 on ALL arms)**
| arm | cadence | tps | payments/s | sigs/blk | full | anchor p50/p90 |
| noreg pure100 @150M   | 3.44 | 979    | 97,851 | 284   | 99% | 44/68ms |
| bypay SERIAL @225M    | 3.16 | 3,542  | 13,303 | 1,120 | 20% | 8/249ms |
| bypay PARALLEL @225M  | 3.19 | 14,454 | 54,350 | 4,523 | 80% | 42/65ms |
- **Identical feeders/offer/cadence; parallel recovery = 4.1x payments and tps at
  the by-payments mix.** Serial's bottleneck expresses as 20%-full blocks (the
  node cannot process signatures fast enough to keep blocks full) + anchor p90
  249ms (staging loses races when blocks do fill). Parallel: 80% full, p90 65ms.
- Mechanism refinement: serial decode+ecrecover runs on the node's runtime
  threads, so it also STARVES ADMISSION (same offer, 4x less landed). Parallel
  recovery moves that work to rayon and unblocks the whole node.
- No-regression: per-block content matches the campaign row (284 vs 287 tx/blk
  at 150M, 99-100% full). CADENCE IS NOT COMPARABLE: this rebuilt chain runs
  ~3.4 blk/s idle-and-loaded (no 500ms pacer in the regenerated genesis);
  campaign rows were paced at 2 blk/s. Per-height throughput is the comparable
  quantity and it reproduces.

**Broke / found (the road to clean arms — each now a rule)**
- **Single-CL restart on a live chain = permanent wedge**: chain keeps 3/4
  quorum, restarted CL must sync, sync request pipeline deadlocks, gap passes
  the ±128 serving window, unrecoverable. RULE: all CLs change together
  (halt-flip-resume) — implemented in the V7 runner's budget flip.
- **The lean-enable boundary height is UNSERVEABLE** (GetDecidedValues: "no lean
  block reproduces the certificate") — any validator syncing across it wedges.
  RULE: lean on from height 1; never enable mid-chain.
- kill+launch merged into one ssh call re-hit the pkill-matches-own-shell
  landmine (3rd occurrence) — cost V7 arm 1.
- soak4.toml was an uncommitted local file; my localdev4-based restore initially
  carried deadline=2000 (fixed to 500 from generated-compose evidence).

**Decided**
- 2h mixed soak running (parallel mode, divergence-checked 4/4 every 5 min).
  If PASS: LEAN_PARALLEL_RECOVERY flips DEFAULT ON per roadmap §8 adoption rule.

**Open**
- Soak verdict -> flag flip -> mirror to reth-fork.
- CL robustness list grew: round-fatal validation errors (3 classes), sync
  request-pipeline deadlock, ±128 serving window, boundary-height serving.

**Pacer restored to product target (user directive: match original arc).** The
regenerated genesis carried targetBlockTimeMs=250 (4 blk/s); master pins 500
(genesis.test.ts). Fixed LIVE via on-chain governance (updateConsensusParams,
no restarts — consensus params are the one fleet reconfiguration that needs no
halt-flip-resume). Cadence under soak load: 1.93 blk/s = campaign-parity.
CAVEAT recorded: tonight's V7 rows ran at the 250ms pacer — the 4.1x
serial/parallel verdict stands (shared pacer) but absolute payments/s are
inflated; re-run at 2 blk/s queued after the soak for campaign-comparable rows.

## 2026-08-26 (night) — soak PASS, flag default-ON, at-target rerun

**Soak**: 2 h mixed load, parallel mode — ✅ PASS, zero divergence (24 checks,
4/4 byte-identical each), zero validator noise, feeders self-healed at the
corpus boundary. Adoption rule satisfied.

**Changed**: `9c35e29` parallel recovery DEFAULT ON (`LEAN_PARALLEL_RECOVERY=0`
= rollback for one release); fork mirror `ac2ae4a`; default-on binary shipped
fleet-wide (sha-verified). Pacer restored 250→500 ms live via governance
(`1fd48c8`).

**Measured (2 blk/s pacer, 10-min arms, fleet)**
| arm | cadence | tps | payments/s | sigs/blk | full | anchor p50/p90 | burns |
| noreg pure100 @150M  | 1.92 | 498   | 49,842 | 260   | 90% | 71/113ms | 0 |
| bypay SERIAL @225M   | 1.79 | 3,530 | 13,268 | 1,974 | 35% | 42/320ms | 1 |
| bypay PARALLEL @225M | 1.92 | 4,577 | 17,211 | 2,379 | 42% | 43/113ms | 0 |
- **No-regression: PASS** — 49,842 vs campaign 55,534 = −10 %, inside the ±15 %
  band (90 % full, mildly delivery-limited).
- **Parallel still wins at target: +30 % payments, cadence 1.92 vs 1.79, anchor
  p90 113 vs 320 ms** (serial still loses staging races). Smaller gap than the
  4.1× at 250 ms pacer because BOTH arms are supply/build-bound at 2 blk/s:
  35–42 % fullness with ~30 k pending means the builder packs only ~2.4 k
  txs/block despite deep pools. **NEW OPEN QUESTION**: what caps the builder at
  ~2.4 k signed txs/block under deep mixed pools (per-sender pending windows?
  build deadline?) — this, not signatures, is the at-target bypay ceiling.

**Open**
- Builder-depth investigation (above) — next perf item for mixed workloads.
- Notebook entry + figure for the V6/V7 mechanism story (admission starvation).
- Fleet left RUNNING at 2 blk/s, lean default-on, idle.

## 2026-08-26 (late) — RETRACTION: no builder ceiling; it was the pool's spammer cap

Diagnostic chain (each step evidenced):
1. Offline single node, 40k-deep by-payments pool, direct arc_buildBlock:
   **8,653 txs, 100% of 225M, in 10-12ms**, five blocks straight → builder exonerated.
2. Fleet 3-min probe, fresh corpora, same rates: **1.90 blk/s, 5,649 txs/blk,
   100% full** → the lane sustains the full by-payments budget at target.
3. The at-target arm's feeder log: **62% of 2.4M sends rejected**. Per-tx error
   sampling: dominant "transaction nonce is not consistent" (cascade), trigger
   "rejected due to <sender> being identified as a spammer" = upstream reth
   `SpammerExceededCapacity` — per-sender slot cap. One capped tx opens a nonce
   gap; every later nonce of that sender then rejects; the sender's remaining
   ~12k corpus txs are dead. Avalanche across senders → 35-42% fullness.
CONSEQUENCES: the at-target bypay rows (13.3k/17.2k) understate the lane — both
arms equally supply-poisoned, so the serial-vs-parallel comparison stands but
absolutes don't. True at-target by-payments ≈ **10.8k tps / ~41k payments/s at
100% full** (probe; 10-min confirm pending). FIXES: raise/configure
max_account_slots for lean pool senders; harness rule: per-sender in-flight
depth must stay under the slot cap (deep-nonce corpora + governed feeders can
trip it); ROADMAP §2c mempool notes gain this as a measured landmine.
Also answered (Duc): upstream reth DOES parallelize ecrecover — stages
(SenderRecoveryStage rayon workers) + engine-tree streaming recovery overlapped
with execution — but as pipeline-embedded machinery, not a reusable API; our
~15-line rayon shim over reth's own per-tx recover IS the minimal reuse, pinned
by the differential test.

## 2026-08-26 (close) — pool fix validated; final at-target verdict: 3.0×

**Fix validated.** max_account_slots 256→4096 (`1ddcea1`, fork-mirrored,
deployed): feeder rejections **1,499,869 → 1** (of ~2.1M sent). Blocks back to
100% full in BOTH modes. The spammer-cap → nonce-cascade diagnosis is proven by
the cure.

**Final at-target by-payments pair (225M, 2 blk/s pacer, fixed pool, 10-min):**
| mode | cadence | tps | payments/s | full | anchor p50/p90 | burns |
| serial   | 0.63 | 3,542  | 13,318 | 100% | 317/340ms | 103 (all val3) |
| parallel | 1.86 | 10,504 | **39,484** | 100% | 103/124ms | 1 |
**Parallel recovery is 3.0× at the product target.** With full 5,651-sig blocks,
serial recovery puts ~170ms on every critical path: anchors triple, and the
slowest validator (val3) misses its propose windows entirely — every one of its
turns burns. This replaces the earlier 13.3k/17.2k rows (both supply-poisoned)
and the "+30%" claim: at target the serial mode doesn't lose 30%, it loses the
ability to hold cadence.

**Superseded/retracted chain now fully resolved:** builder exonerated (10ms
full-budget builds) → pool spammer-cap found → fix validated → true at-target
numbers measured. The lane at 2 blk/s: 49.8k payments/s pure-100 · 39.5k at the
realistic by-payments mix.

## 2026-08-27 — N=100 on the new stack: NOT parity yet; anchor regression isolated

**Measured (rebuilt fleet, 2 blk/s pacer, parallel default, fixed pool, 10-min)**
| arm | cadence | payments/s | full | anchor p50/p90 | burns | campaign ref |
| N=100 @150M | 1.91 | 43,178 | 78% | 63/103ms | 3 | 55,534 @1.93 100% (−22%) |
| N=100 @225M | 1.57 | 67,593 | 100% | 157/184ms | 6 | 83,286 @1.93 44ms (−19%) |
Two DIFFERENT failure signatures: 150M is supply-shy (harness under-delivered;
yesterday's serial noreg hit 90%/49.8k on the same fleet — pure-100 delivery at
low rates has unpinned run-to-run variance). 225M is chain-slow: full blocks but
637ms heights and anchors 3.6× the campaign's.
**Anchor regression isolated in one probe: IDLE anchors are 153ms p50 —
identical to loaded (157ms).** A fixed per-height cost on this rebuilt chain,
independent of load/signatures. The anchor span covers both lanes' commit; lean
promote is instant on empty blocks ⇒ prime suspect is EVM-lane FCU/persistence
latency on a 110k-height-old chain (campaign chain: 23k heights, 44ms anchors).
Also explains the 225M height stretch (518→637ms ≈ same adder).
**Controlled comparisons unaffected** (both sides of every pair carried the same
adder): serial-vs-parallel (3.0×/4.1×) and mixed-workload conclusions stand.
"New-stack reproduces campaign rows" remains UNPROVEN — blocked on the EVM-lane
fixed cost, not on the lean node.

**Ops (cost a morning): tailscale expiry now aborts arms BEFORE state changes**
— the 08:11 arms half-executed a budget flip (local CL restarted alone; wedge
risk — chain got lucky) because remote halts silently no-op'd. Runner gained an
ssh-effect preflight; health gate distinguishes UNREACHABLE from unhealthy.
Watcher lesson ×2: append-only reports need run-anchored patterns (row-count
conditions, not greps over history).

**Open (next session)**
1. EVM-lane anchor adder: profile FCU on the aged chain (fresh-chain A/B or
   restart EVM ELs); then re-run the two N=100 arms for the parity verdict.
2. 150M supply variance: instrument feeder acceptance during a pure-100 arm.

## 2026-08-27 (evening) — parity chase concluded: 150M in-band, 225M −16% with one residual

**Root cause of the "not parity" scare found: the rebuilt fleet was running
quake's DEFAULT latency emulation (+64-131ms tc shaping) on top of real
tailscale WAN.** The lost original soak4.toml was a perf scenario
(latency_emulation=false — cf. nightly-perf.toml); the localdev4-based restore
re-enabled it silently. Fixed in the committed scenario; stripped live from all
24 containers (verified noqueue).

**Final N=100 rows (new stack, tc stripped, parallel default, fixed pool):**
| arm | cadence | payments/s | full | anchor p50 | campaign | delta |
| 150M-r2 | 1.77 | 50,703 | 100% | 127ms* | 55,534 | −8.7% ✅ in band |
| 225M-r3 | 1.62 | 70,037 | 100% | 156ms | 83,286 | −16% ❌ just outside |
(*150M-r2 ran pre-strip; likely improves.) The 225M residual is one number:
**anchor 156ms vs the campaign's 44ms under load** (idle is 73ms) — ~+100ms per
height ≈ the whole cadence gap. Signature: staging losing races / slow promote
on this environment. tc strip moved loaded anchors not at all (155→156), so the
residual is NOT network shaping. OPEN: instrument stage/promote timings.

**New failure class + heal (afternoon):** rapid lean-node restarts height-SPLIT
the consensus — validators scattered across 3-4 adjacent heights, each voting
alone (1/4), permanent no-quorum deadlock. HEAL: stop ALL CLs together, start
together — small gaps (≤128) sync and converge (verified: 1.96 blk/s after).
RULE: don't restart lean nodes back-to-back; runner gained SKIP_LEANUP for arms
whose environment is already correct. Health gate's chain-advance check made
patient (240s — post-restart recovery grows with chain length; a 15s sample
false-aborted twice).

**Session demonstrated (summary for review):** mixed-N workloads end-to-end;
parallel recovery proven (3.0×/4.1×) and default-on after the full gate ladder
+ 2h soak; reth spammer-cap bug found+fixed (rejections 1.5M→1); N=100 parity
re-established at 150M on the self-contained stack; and a robustness catalogue
(3 CL fatal classes, single-CL-restart wedge, lean-boundary serving, ssh-expiry
half-flips, height-split deadlock) each with a written rule.

## 2026-08-27 (late) — lab handout is now the full notebook, newest-first

lean-lane-handout.tex (Duc's tufte template) now carries the complete log:
3-page current-state digest, then every dated entry 08-27→08-17 reverse-chron,
then Part 0 history. Two new entries written for 08-26/27 (previously
worklog-only). 30 pages, zero serious overfulls, render-checked. Raw
append-only record unchanged in lab-notebook.tex.

## 2026-09-07 — Track A: CL round-fatal, boot patience — LIVE PASS

**Changed** (`cb17d36`, local): `TransientDependencyError` marker in eth-engine
(downcast through wrap_err, never string-matching), attached at both sources
(lean shim past retry budget; engine SYNCING/ACCEPTED + transport). Sync-path
handler: transient → reply None (malachite re-requests) + metric, never Err to
the loop. Decide anchor: transient get_head waits out the deadline. Lean boot:
retries forever like the EVM connect. Live-proposal + proposer paths already
tolerated errors (unchanged). 289/289 unit tests + 6 new. cli_db_migrate's one
failure is environmental (this box has ~/.arc/consensus/store.db).
Also `54be3a3`: lean node bounded RPC bind retry (surfaced by the test; the
incident itself was a test-script port bug — see Broke).

**Measured — `experiments/dual-el/cl-transient-live-test.sh`** (single box, 5
validators, lean from height 1, mixed load):
- S1 kill val3 lean node 60 s under load: CL3 restarts 0→0, parked 0, fatal 0,
  chain 278→596 during the outage, 3 transient warns, val3 caught up to 599. ✅
- S2 restart CL3 while its node is STILL down: 2 boot-retry lines, parked 0;
  node back → CL3 reconnected, heights carry 4–5 signatures again. ✅
- Metric scrape on :29002 returned nothing — check the prometheus rendering
  (`_total` suffix?) next run; the 3 warn lines prove the arm fired.

**Broke / retracted**: my FIN_WAIT explanation for the bind failures was
WRONG — the test script's `local i=$1 P=$((8560+i))` evaluated before `i` was
assigned (bash), relaunching "val3" on val5's port. Bind-retry commit reworded
to the honest justification. Three live runs to get one clean.

**Open**: metric endpoint check; v0.1 restructure in progress (worktrees
~/arc-lean-v0.1-work / ~/arc-lean-v0.1, lean repo ~/lean-lane).

## 2026-09-07 (later) — v0.1 packaging: clean CL series on main + standalone lean repo

**Changed**
- Worktree `~/arc-lean-v0.1-work` (branch `lean-lane-v0.1-work`, d437585): subtractive
  cut of this branch — second reth engine (payment_engine), builder prebuild, compact
  proposals, deferred-exec mode, second-EL CLI/config fields, the 8s→120s engine
  timeouts, unrelated Cargo feature drift all removed; lean lane + Track A compile and
  all CL suites pass on the reth-2.3.0 base. Scaffolding only; not the deliverable.
- Worktree `~/arc-lean-v0.1` (branch `lean-lane-v0.1`, based on origin/main bd4ab47,
  reth v1.11.3): the CL delta re-applied on main as seven commits —
  da22eae types (dual-lane block, framing, commit_lanes) · 856d249 consensus-db
  (keyed by value_id) · 8463fc5 eth-engine (LeanShim client + TransientDependencyError) ·
  06bb4a2 CL lean arms behind ARC_PAYMENT_LEAN_LANE · 95eb551 value-sync env tunables ·
  d259ef3 spammer fan-out (optional) · b074b28 docs/lean-lane-integration.md.
  37 files, +2964 −182 (CL proper: 25 files, ~+2.2k). Each message carries the
  design reasoning per handler. Not pushed (user preference).
- New in the series vs this branch: `encode_value`/`decode_value` in types/block.rs.
  Flag off now emits/parses the EVM payload's SSZ **exactly as main** (unit test
  `encode_value_flag_off_is_stock_ssz`); flag on keeps the frame on every value
  as run on the fleet. Format is chosen by the node's flag, never sniffed.
- Standalone lean repo `~/lean-lane` (37ffaaf): lean-native + lean-lane-node +
  its own spammer copy (arc-version dep removed), `scripts/lean-smoke.sh` runs from
  the repo alone, README, docs/integration.md (contract), docs/setup.md. Cargo.lock
  pinned to the measured dependency set (alloy-json-rpc 2.1.0; newer needs rustc 1.94).

**Measured**
- `~/lean-lane/scripts/lean-smoke.sh`: PASS — 40 heights, 9,856 txs, 98,560 payments,
  avg N=10.0, 3/3 nodes at one commitment, replay-invariance leg ok (loopback,
  50 M budget, no fullness figure — a smoke test, not a benchmark).
- v0.1 series on main: `cargo test` for arc-node-consensus / arc-eth-engine /
  arc-consensus-types / arc-consensus-db all green (263/176/85/82/31/19 …) except
  `cli_db_migrate::test_migrate_command_without_home_flag`, which passes with an
  isolated HOME (pre-existing: this machine has ~/.arc/consensus/store.db).
- No fleet run this session; the fleet was torn down at the end of the Track A test.

**Broke / retracted**
- "Flag off = byte-identical to stock" was **false on the wire** on this branch:
  `frame_lanes` always emitted the 8-byte prefix, so a flag-off node differed from
  main's raw SSZ. Fixed in the v0.1 series only (encode/decode by flag). This branch
  still has the always-prefixed form; it only matters for mixing with main-built nodes.
- The v0.1 series is compiled and unit-tested on main, **not fleet-run on main**.
  All numbers in §5 come from this branch (reth 2.3.0).
- The lean node's `run --help` starts a node with defaults instead of printing help
  (hand-rolled arg parser) — it ran for 10 min in the lean repo during doc writing;
  killed, stray `lean-lane-data/` removed and gitignored. Not fixed.

**Decided**
- Wire mode by flag rather than sniffing: a raw SSZ payload's first 8 bytes could
  collide with a valid prefix at ~2^-24 per block; unacceptable for consensus.
- Per-file-group commits, with Track A described inside the arms commit rather than
  split out by hunk (it is interleaved in the same functions); the review guide in
  docs/lean-lane-integration.md is the per-change map.
- Lean repo carries its own spammer instead of depending on arc's.

**Open**
- Fleet-run the v0.1 series on the main base (needs the EVM-lane docker images from
  main + the lean repo binaries) before calling it shipped; `lane-bench.sh` here still
  assumes this branch's paths.
- `lean-lane-node run --help` should print help; the `bench` subcommand needs the same.
- Track B items (checkpoint root, header version+proposer, charge-proposer,
  beneficiary, tx propagation) are listed as open decisions in the guide, not started.
- Metric scrape `<none>` on :29002 in the Track A test still unexplained.
