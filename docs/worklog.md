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

## 2026-09-07 (night) — local lean testnet via quake; v0.1.0 tagged; e2e on main base

**Changed**
- `lean-lane-v0.1` (worktree `~/arc-lean-v0.1`, tag `lean-lane-v0.1.0`, 9 commits on
  origin/main bd4ab47): + `quake: cl_env passthrough` (c11f59a — manifest-level and
  per-node extra env for CL containers, rendered into compose; CL gets
  host.docker.internal) · + `local lean testnet` (scripts/lean-testnet.sh, Makefile
  testnet-lean{,-load,-status,-down}, crates/quake/scenarios/localdev-lean.toml, docs
  §4b). The spammer commit got a fixup: quake links the spammer lib and needed
  `..(*config).clone()` + the new SpammerArgs fields (was a compile break in the series).
- `~/lean-lane`: single branch `main` (renamed from master), tag `v0.1.0` (d1dd733);
  README says how to clone over tailscale ssh from this machine. No hosted remote yet.
- Docker images `arc_consensus:latest` / `arc_execution:latest` now built from the
  v0.1 tree (main base). The branch images are kept as `:lean-branch`.

**Measured** (5 validators on this machine, quake localdev-lean, latency emulation on,
100 M lean budget, main-based images)
- `up`: EL and lean at height 5 on all five within ~30 s, identical lean commitments.
- 3-min fan-out load, 1,500 tx/s, N=10, 200 accounts, spread over all 5 lean nodes:
  349 blocks in 181 s = **1.93 blk/s**; 269,094 / 270,410 sent txs included =
  **1,487 tx/s, 14,867 payments/s**; per-block txs min/med/max 175/757/1384; gas
  **median 54 % of budget, 1 block ≥95 %, 0 empty** → delivery-bound (spammer rate),
  NOT a chain ceiling; pools empty at end. 5/5 lean nodes byte-identical at 388 and 484.
- CL health: restarts 0, parked 0, transient 0, Invalid 0 on all five; the only ERRORs
  are boot-time p2p dial refusals before peers were up.
- This is the first run of the lean lane on the **main base** (reth 1.11.3). It closes
  the "compiled but not fleet-run on main" caveat for a single machine; the 4-machine
  fleet numbers remain branch-only.

**Broke / retracted**
- `make genesis` (hardhat) fails in a fresh worktree without submodules — quake start
  hit the same; `git submodule update --init --recursive` fixes it. Noted, not a code bug.
- The spammer commit alone broke `quake` compilation in the series (fixed via fixup).

**Decided**
- Generic `cl_env` in quake instead of lane-specific quake code; the lane scenario is
  pure TOML.
- Scenario has no full node (mixed-flag fleets fail closed by design).

**Open**
- Push targets: neither repo has a remote it can push to from here; user to decide.
- A chain-bound number on this base needs the spammer at ≥3k tx/s or N≥50 (this run
  was 54 % full). `make testnet-lean-load LOAD_RATE=3000 FANOUT=50` is the next probe.
- `lean-lane-node run --help` still starts a node instead of printing help.

## 2026-09-07 (late night) — sustained probe; series rebased onto new main (reth v2.2.0); e2e again

**Changed**
- `lean-lane-v0.1` rebased from bd4ab47 (reth 1.11.3) onto origin/main 97f8da0
  (v0.8.0 sync, reth v2.2.0; 138 upstream files touched incl. types/block.rs, the
  validation path, spammer). Now **6 commits** (2fd4fd4 types · e24daef consensus-db ·
  e7ed8bc eth-engine · 03a5e76 CL arms · docs · local testnet), tag
  `lean-lane-v0.1.0` MOVED to the new tip (never pushed). Safety branch of the old
  series: `lean-lane-v0.1-pre-rebase`.
- Dropped from the series: value-sync env tunables (upstream now has
  `ARC_SYNC_REQUEST_TIMEOUT`/`ARC_SYNC_BATCH_SIZE`), the arc spammer fan-out commit
  (upstream rewrote the spammer; the lean repo's copy drives the lane), the quake
  `cl_env` commit (upstream has `cl.env`/`el.env` tables). Kept: +2 template lines so
  CL containers resolve host.docker.internal.
- Adapted to upstream's refactor: `self_reported_block_hash` + `value_id()` in the
  vote path, `establish_block_validity(.., lean_shim, ..)`, transient → upstream's
  `SyncedValueOutcome::LocalTransientError` (+ my counter), `ExtendedCommitCertificate`
  sync form kept. `TransientDependencyError` is now a `wrap_err` context over the
  real cause so upstream's `EngineApiRpcError::try_from` still finds it (one upstream
  test caught this); `is_transient` checks both the context chain and the source chain.
- lean-testnet.sh: `up` now runs `quake clean --all` (a compose file rendered by an
  older quake carried `--arc.denylist.enabled`, which the new EL rejects) and refuses a
  non-fresh EVM chain; `load` has POOL_TARGET.

**Measured** (this machine, 5 validators, latency emulation on, 100 M lean budget)
- Pre-rebase base, 10 min, 3,000 tx/s offered, N=50, pool-target 1,500, 800 accounts:
  110–119 heights/min all along, 1,135 consecutive blocks at 369 txs = **100 % full**,
  418,815 txs / 20.9 M payments (~1.9 blk/s, ~700 tx/s, ~35k payments/s), 0 restarts.
- Rebased series on main 97f8da0 (reth v2.2.0), 4 min same load: **117–119
  heights/min**, every block 369 txs = 100 % full, 181,424 / 182,900 txs included
  (9.07 M payments), 5/5 lean nodes identical, 0 restarts/parks/transient/Invalid.
- Tests on the rebased tree: 403/99/194/106/31/20/1 green; only the environmental
  `cli_db_migrate` test fails (passes with isolated HOME).

**Broke / retracted**
- `quake start --force` neither wipes data nor necessarily re-renders compose: two
  separate `up` failures (fresh lean chain under an old EVM chain; stale EL flags).
- A `git stash push -- <path>` taken mid-rebase popped back with the whole staged
  index and conflicted; recovered by checkout + re-extracting the single file.
- This branch (`lean-lane-integration`) is NOT rebased; it stays the measured
  reth-2.3.0 campaign tree. Fleet scripts here still assume it.

**Open**
- Fleet run of the v0.1 series from the new main (needs the 3 remotes rebuilt from it).
- Push targets for both repos (arc origin refuses; lean repo has no remote).

## 2026-09-14 — v0.2 header-binding: guide + local testnet restart leg

Branch `lean-lane-v0.2` in `/home/papaduck/arc-lean-v0.1` (worktree; base
`af5b633`), lean node `/home/papaduck/lean-lane` branch `v0.2`. Task 9 of the
`2026-09-14-lean-lane-header-binding` plan: the code (five commits
`275a1cd`..`af5b633`) binds the lean block into the EVM header via
`prev_randao` instead of the v0.1 two-lane value commitment — consensus votes
on the plain EVM block hash, the header/lean binding is checked at validate,
decide anchors by commitment, the CL no longer stashes lean bytes between
validate and decide. This session updated the guide and local-testnet script
for that design and proved it, including a CL restart mid-run.

**Changed**
- `c5d354f` (arc-lean-v0.1, `lean-lane-v0.2`) — `docs/lean-lane-integration.md`:
  retitled v0.2; §1 series table now lists the five v0.2 commits (was the
  four-commit v0.1 base); §2 shim table gains the two v0.2 verbs
  (`arc_newBlock`/`arc_getBlockBytes` by commitment); §3 per-phase table:
  propose builds the lean block first and binds its commitment as
  `prev_randao`, validate checks the header/lean binding before anything
  else, decide anchors `arc_newBlock{commitment}` with the lean node holding
  the bytes end to end, sync serve looks lean bytes up by header commitment
  instead of an offset scan; §4 gets a prevrandao-semantics paragraph (the
  field was already documented non-random; it is now the proposer-chosen
  lean commitment instead of a hard-coded zero) and a note on what the new
  `restart` subcommand proves. `grep -n "value_id\|commit_lanes\|stash"
  docs/lean-lane-integration.md` is empty (every such mention rewritten
  around the header-commitment design, without using those words even when
  describing what was removed).
- `c5d354f` also: `scripts/lean-testnet.sh` gains `restart <n>` —
  `docker restart validator<n>_cl` only (the lean node underneath stays up),
  then polls `arc_getHead` on every lean node until validator `n` is within 3
  heights of the tip. `bash -n` clean.
- CL delta vs `origin/main` across the four lane crates (`malachite-app`,
  `eth-engine`, `types`, `consensus-db`) is unchanged by this session (docs/
  script only): 27 files, +1,997/−129 lines — the ~27 files, +2.0k/−0.13k
  figure the task started from.

**Measured** (local 5-validator quake testnet, this branch's freshly built
docker images, `lean-lane-node`/`spammer` from `/home/papaduck/lean-lane`
`v0.2`, 100 M lean budget, N=50 fan-out, pool-target 1,500, 800 funded
accounts):
- `up`: ~26 s to 5/5 validators at lean height 4, byte-identical.
- 240 s fan-out load: spammer sent 143,500 txs over 243.5 s (589 tx/s
  offered, pool-target-1,500 governed — well under the 3,000 tx/s asked for).
  That send rate is a spammer-side number, not a chain result; block
  fullness was sampled only in the 66 s window below, not across the full
  240 s, so no payments-delivered total is reported for the whole load.
- `restart 3` fired at T+96 s into the load (`docker restart validator3_cl`
  only): validator3 read back within 3 lean heights of the tip immediately —
  `docker restart` completes in about a second on this box and its lean node
  was never touched, so there was nothing to resync.
- Two `status` samples 60 s apart straddling the restart: lean height
  215→310, **86.4 heights/min (~1.44 blk/s)**, every block in both samples
  **369 txs = 100 % of the 100 M budget** — the only window fullness was
  actually sampled in. `agreement: all 5 lean nodes identical` at both
  samples and again at load end (height 388) and after the pool drained
  (height 421, 0 txs); fullness outside the 215→310 window was not sampled.
  Cadence here is below the steady-state 117–119/min recorded for the v0.1
  base in the entry above; this is one sample spanning a restart, not a
  characterised restart cost.
- `docker inspect validator{1..5}_cl --format '{{.RestartCount}}'` reads 0 on
  all five. **This is not "validator3 wasn't restarted"**: Docker's
  `RestartCount` only counts restart-policy-triggered restarts, not a manual
  `docker restart`. `State.StartedAt` is the real signal — validator3 reads
  `02:21:54`, the other four `02:20:05` — confirming only validator3's
  container restarted and the other four never did.
- `docker logs validator3_cl 2>&1 | grep -c "Manual intervention"` = 0.
  Grepping all five CLs' full logs for an `ERROR` mentioning the lean lane:
  none. The few plain `ERROR` lines present (validator2/3/4, 7/3/2
  respectively) are all timestamped `02:20:05`, at boot — libp2p
  `NoPeersSubscribedToTopic` / dial-negotiation noise before the mesh formed,
  unrelated to the lane or the restart.
- Teardown verified: `docker ps -q | wc -l` = 0, `ss -ltnp | grep -c
  ':856[1-5] '` = 0 after `down`.

**Broke / retracted**
- Nothing broke. One correction to the brief's own expectation: it reads
  `RestartCount` as the restart signal for validator3; Docker does not
  increment that field for a manual `docker restart` at all (only for
  restart-policy restarts), so the correct evidence is `State.StartedAt`
  (see Measured). Worth fixing in the next brief that asks for this check.

**Decided**
- Kept `restart` scoped to the CL container only, per the task brief: the
  lean node staying up is exactly the case the v0.1 CL-side byte copy used to
  make fragile (a CL that reboots while its lean node is down could park);
  with the header binding the CL carries nothing across the reboot, so this
  leg is a reasonable proxy for that failure mode even without also killing
  the lean node.
- Left the pool-target-1,500 throttle as configured (589 tx/s sent instead of
  the 3,000 offered) rather than raising it to chase a higher number: the
  point of this run was the restart leg and the 100 %-full/agreement/no-park
  invariants, not a throughput record, and every block stayed full the whole
  time regardless of the send rate.

**Open**
- The 86.4 heights/min sample here vs. 117–119/min for the v0.1 base is a
  single before/after pair around one restart, on shared hardware also
  running other work this session — not yet enough to call a restart-cost
  number. A repeat run with a `status` sample immediately before the restart
  too (three samples, not two) would isolate it.
- Task 10 (fleet runner under `~/arc-runs/<run-id>`) is next per the plan:
  ships this same v0.2 series to the 4-machine fleet and repeats the
  restart-CL and kill-lean legs there, plus records fleet numbers in guide
  §5.

## 2026-09-14 (evening) — v0.2 on the 4-machine fleet: runner + first fleet run

Branch `lean-lane-v0.2` in `/home/papaduck/arc-lean-v0.1` (worktree; base
`d879069`), lean node + spammer from `/home/papaduck/lean-lane` branch `v0.2`.
Task 10 of the `2026-09-14-lean-lane-header-binding` plan: build the fleet
runner and take the header-binding series to the four machines for the first
time. Run id `v02-0914-1945`; every byte a run writes on a machine lives under
`~/arc-runs/<run-id>/`.

**Changed**
- `9647e01` (arc-lean-v0.1) — `crates/quake/scenarios/fleet4-lean.toml`,
  `scripts/fleet-split-compose.py`, `scripts/fleet-lean.sh`,
  `scripts/fleet.env.example`. The runner is `lean-testnet.sh`'s multi-host
  sibling: same `up/load/status/down` shape, plus `health`, `restart-cl <n>`
  and `kill-lean <n> <secs>`. The splitter is a parameterised port of
  `experiments/dual-el/fleet/gen-fleet.py` — CL multiaddrs and EL enodes
  retargeted to `<tailscale ip>:<published port>`, EL p2p published (quake
  already publishes the CL's `2700{n-1}:27000`), volumes made absolute under
  the run root, remote networks stripped of the `172.21.0.0/16` ipam block and
  the static addresses.
- `6ef593f` (arc-lean-v0.1) — guide §5 gains the "fleet, v0.2 on main 97f8da0"
  row block with both arms, the legs, and the caveats.
- Deviation worth keeping: the scenario is rendered with `quake setup --force`,
  not `start --force` + `stop`. `setup` writes the same
  compose/assets/validatorN tree without starting a container, so no CL ever
  boots against a lean node that is not up yet (the boot-park landmine) and the
  validator dirs stay 32 KB of pure config instead of 5.6 MB of chain data that
  would have to be root-deleted before shipping.

**Measured** (fleet: ginny-alienware + GinnyThui + papaduck + papaduck-alien2,
150 M lean budget, N=50 ⇒ a 100 %-full block is **553 txs**, `21000 + 5000×50 =
271,000` gas each; 10 min per arm, sampled every 60 s to
`.quake/fleet-runs/v02-0914-1945/run.jsonl`)

| arm | blk/s | tx/s | payments/s | fullness |
|---|---|---|---|---|
| one spammer per machine (`DISTRIBUTED=1 GENERATORS=8`) | 0.73 | 404 | **20,227** | **553/553 = 100 %** on every one of the 8 rate samples |
| one spammer here → all four over the tailnet (the brief's command) | 1.54 | 491 | 24,539 | 202–474 of 553, 38 % → 81 % — **delivery-bound** |

Both arms: 0 CL restarts (`RestartCount` 0/0/0 at every sample), 0
`Manual intervention` on all four CLs, **100 % of rounds decided at round 0**
(`grep -oE 'round=[0-9]+'` on all four CLs is 7145/7079/7786/6848 × `round=0`,
no round ≥ 1), four distinct proposers taking turns (so no parked CL and no
pure sync-follower), lean blocks byte-identical at every `status`. Nothing was
saturated at the full-block cadence: CLs 1–3 % CPU, ELs 4–17 %, lean nodes
60–100 % of **one** core, spammers 2–4 %.

Track A on the fleet, both legs pass:
- `kill-lean 3 60` — lean node 3 killed at height 1031; the other three advanced
  to 1062 (+31) while `arc_getHead` on `100.70.62.92:8560` refused connections;
  validator3's CL neither restarted (`RestartCount` 0 → 0) nor parked; on
  relaunch the node reached the tip in **2 s** (1064/1064) and its EL followed
  (1077 vs a tip of 1078); agreement byte-identical at 1065.
- `restart-cl 3` — `State.StartedAt` moved 02:44:43 → 02:57:27 while
  `RestartCount` stayed 0; rejoined within 3 of the tip in **2 s**; 0 parks; all
  four equal at 1085 byte-identical.

Shipping was verified by effect, not by exit code: images by the sha256 of
`/usr/local/bin/arc-node-{consensus,execution}` *inside* the image on both ends
(all three remotes were carrying three-week-old images), the binaries and fund
file by `sha256sum | cut -c1-16` after transfer, the quake tree by comparing
`assets/genesis.json` (`79074ff653636201` on all four).

**Broke / retracted**
- Nothing broke, and nothing earlier is retracted. One caution about my own
  first arm: the brief's load command (`-r 3000 -g 2 -a 800` from one machine
  against all four) does **not** fill blocks on a fleet. In backpressure mode a
  single sender offers roughly `generators / RTT`, and the tailnet RTT is not
  the loopback's — it offered 472 tx/s against a chain eating 460–590, so the
  pools sat at 150–470 pending against the 553 a full block needs. The lane also
  does not propagate transactions between lean nodes (guide §6, "the proposer
  packs what it has"), so spreading one sender over four targets gives each
  proposer a quarter of it. That arm measures delivery, not the chain (§6), and
  is recorded as such rather than as a throughput number.
- Ops hazard hit and self-corrected, worth writing down: I edited
  `fleet-lean.sh` while a `load` was running it. **bash reads a script
  incrementally** — changing byte offsets under a running bash can make it
  resume mid-line. Reverted to the exact original bytes immediately and
  re-applied after the run. Never edit a shell script that is executing.

**Decided**
- `DISTRIBUTED=1` (one spammer per machine, disjoint `--account-offset` ranges,
  each against its own node over loopback) is the runner's saturating mode, and
  it is what a fleet throughput number should be taken with. Kept the
  single-sender path as the default so the brief's command still works and the
  difference is visible.
- Evidence for a restart is `State.StartedAt`, not `RestartCount` — docker does
  not increment the count for a manual `docker restart`. Both are now printed:
  `StartedAt` proves the bounce, `RestartCount` proves there was no crash. (This
  was the correction recorded in the previous session; it is now in the script.)
- `down` deletes nothing. Removing a run is the operator's `rm -rf
  ~/arc-runs/<run-id>`, on purpose: a lean chain's bytes are bound by
  certificates and a laggard can never sync a span that was destroyed.

**Open**
- **The cadence question.** At 100 % full the fleet held only 0.73 blk/s. The
  campaign's `lean, N=100, 150 M` row is 1.93 blk/s for a block of almost the
  same wire size (814 KB at N=50×553 vs 824 KB at N=100×287) but **half** the
  signatures (287 vs 553). That points at per-transaction work rather than
  bytes — but nothing was CPU-saturated (lean node 60–100 % of one core, CLs at
  1–3 %), and it is a cross-run comparison against a different base on a
  different day, n=1 each. The clean experiment is a single fleet session
  sweeping N ∈ {50, 100} at a fixed 150 M budget with `DISTRIBUTED=1`, which the
  runner now makes a two-line change.
- Per-validator proposer-turn attribution in the health gate (it catches parked
  and hung CLs, not slow ones) is still not automated; this run did it by hand
  from the CL logs.
- 91 `WARN … sync … Received response for unknown request ID` lines on
  validator3 over the run — benign-looking sync races, never investigated.

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01YPWyXFV8A1u4RpuQmquB7S

## 2026-09-14 (evening, follow-up) — Task 10 review round 1: two fixes

Review of the fleet run found a script bug and an under-specified disclosure.
Both fixed; the fleet was **not** brought back up for this round, so the
sampler change is untested against a live fleet.

**Changed**
- `3bf1796` (arc-lean-v0.1, `lean-lane-v0.2`) — `scripts/fleet-lean.sh`: the
  sampler's table header said `pool r2/r3/r4` while the body printed only the
  CL restart counts into that column, so the promised pool depth was never in
  the table at all; pool depth was read for node 1 only and survived just as
  the JSONL `pool_node1`, and node 1's own restart count was never sampled.
  Now every sample reads `txpool_status` pending depth **and** `RestartCount`
  on all four validators and prints two separate labelled columns,
  `pool1/2/3/4` and `restarts1/2/3/4`, with the JSONL row carrying
  `pool: [p1..p4]` and `restarts: [r1..r4]`. A failed probe records `-1` for
  that node instead of shifting the row.
  Verified offline against stubbed RPCs — `sample_loop` extracted from the file
  and driven with a deliberately shallow node 3 and an unreachable node 4
  prints `2203/2197/61/-1` and `0/0/0/2`, and the emitted rows parse with both
  arrays at length 4. `bash -n` clean, and `git diff -U0` confirms no hunk
  falls outside `sample_loop`. **Not exercised against a live fleet** — the
  next run is its first real test.
  Per-node pool depth is the column that matters here: the lane does not
  propagate transactions between lean nodes, so a proposer packs only what its
  own pool holds and one shallow pool caps fullness while every other signal
  still looks healthy. That is exactly what the first run had to be diagnosed
  by hand.
- `e47e535` (arc-lean-v0.1, `lean-lane-v0.2`) — guide §5 fleet row block now
  discloses which arm the mid-run script edit landed in.

**Broke / retracted**
- Sharpening yesterday's disclosure rather than retracting it: the edit I made
  to `fleet-lean.sh` while it was running was during **arm B — the
  `DISTRIBUTED=1` 100 %-full arm the headline number comes from**, not the
  delivery-bound arm A. Arm A's load ran 19:45:32–19:55:36 and was already
  finished; arm B's ran 19:57:44–20:07:49, and the two-line comment-header edit
  was in place for **at most ~37 s** inside it (bounded by wall-clock checks at
  19:58:58 and 19:59:35, around arm B's t≈75–110 s marks) before being
  reverted.
- **The exact-revert claim has no hash evidence.** I reverted by removing the
  two inserted lines; I did not take a sha256 of the file before the edit, and
  the pre-edit state is not in git, so there is nothing to compare against. All
  I can show is behavioural: the run continued to completion and the sample
  series shows no discontinuity across that window (heights 1153 → 1200 → 1246
  at 47.00 / 45.25 blk/min, every block 553/553). The sampler script was edited
  during this arm's load for ~37 s and reverted; the sample series shows no
  discontinuity; **treat the 100 %-full row as n=1 pending the N-sweep re-run.**
- The rule stands and is now paid for twice: never edit a shell script that is
  executing. If it must happen, hash the file first.

**Decided**
- The N-sweep re-run (N ∈ {50, 100} at a fixed 150 M budget, `DISTRIBUTED=1`)
  now carries two jobs, not one: settle the cadence question, and be the first
  live exercise of the corrected sampler.

**Open**
- Unchanged from the entry above: the cadence question, automated
  proposer-turn attribution in the health gate, and the benign-looking
  `Received response for unknown request ID` sync warnings.

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01YPWyXFV8A1u4RpuQmquB7S

## 2026-09-14 (night) — v0.2 final fix wave: two halts closed, CI green, local run

**Changed** (branch `lean-lane-v0.2` in the `arc-lean-v0.1` worktree, off
`e47e535`; six commits, logic first, fmt last)
- `5d7480b` — **Critical: a network block must produce the lean bytes its header
  names.** `validate_consensus_block` gated every lean check on
  `lean_payload.is_some()`, so a frame without `LEAN_LANE_BIT` whose EVM header
  carried a non-zero `prev_randao` was voted Valid on the EVM lane alone
  (`lean_binding_ok()` is vacuously true with no payload) and then anchored a
  commitment nobody had: SYNCING to the 30 s deadline → height failure → restart
  → same block → forever. Now, for a block that arrived from the **network**,
  the node is asked `arc_getBlockBytes{commitment}` and the commitment is
  recomputed from the answer: matching bytes are used for the rest of the lean
  arm as if framed; no bytes / other bytes → Invalid with
  `"lean lane: header commits to unknown lean block {c}"`; unreachable node →
  transient, no verdict. Origin is an explicit `lean_bytes_required` argument
  (network: proposal parts live and pending, sync = true; store-loaded
  re-validation and self-built = false). Seam: new `LeanBytesResolver` trait on
  the shim, automocked.
- `baf551a` — **Critical: re-proposal and restream of a store-loaded lean block
  broke its signature.** The store keeps the EVM payload only, so `get_value`'s
  reuse arm and `RestreamProposal` re-framed the value WITHOUT the lean trailer
  under the stored Fin signature that covered the framing WITH it — every
  receiver rejected the parts. `rehydrate_lean_payload` fetches the bytes back
  by the header's commitment and verifies them; re-proposal falls back to a
  fresh build when it cannot, restream declines outright. The comment claiming a
  uniform fleet flag made restream framing match was wrong and is gone: the flag
  fixes the format, not the content.
- `13da65d` — the decide anchor's 30 s was never a bound (one shim call carries
  its own ~15 s transport retry); every call is now wrapped in the remaining
  slice of the budget. `LeanAnchor` trait makes the loop testable — seven tests
  covering promote, SYNCING polling, a different commitment, transient, hard
  error, the deadline, and a call that never returns. Deleted
  `state_has_no_lean_stash`, which asserted nothing.
- `aa6968d` — the eight clippy denials in `types/block.rs` (`try_into().unwrap()`
  on statically sized slices → `first_chunk` helpers, so short bytes decode-error
  instead of panicking; three arithmetic sites → saturating), plus the lane test
  that was missing: lean bytes under a zero `prev_randao` must NOT read as bound.
- `a815140` — guide + spec vs implementation: no "5 s grace"; sync receive
  validates and **stages** (append is at the decide anchor); the two new rules
  written down; mid-chain activation stated as foreclosed. Spec §4/§10: staged
  entries retained while `number >= head`, cap 32, with the memory bound.
- `843f15e` — `cargo fmt --all` + the remaining workspace clippy denials (six
  `arithmetic_side_effects` in the lean arms, five `too_many_arguments`).
- lean-lane `7f62966` (tag `v0.2.0-rc1` moved) — `locate_by_commitment` no longer
  holds the commitment-index mutex across the log read; docs §3 gains the v0.2
  sync-serve row and corrects the certificate line.

**Measured** (5-validator localdev-lean on one host, images rebuilt from the
final tree, N=50 / 100 M, 800 accounts, pool-target 1500)
- 180 s load, two status samples 69 s apart under load: lean heights 123 → 188 =
  **65 blocks / 69 s = 0.94 blk/s, every block 369 txs = 100 % of budget** ⇒ 348
  tx/s, **17,391 payments/s**. Agreement: all 5 lean nodes byte-identical at the
  sampled height, both samples and after the load (228).
- Offered rate `400.7 tx/s` (governed down from 3,000 by `--pool-target 1500`);
  74,088 txs sent in 184.9 s.
- validator3's CL restarted mid-load at 21:08:42: **back within 3 heights of the
  tip in 1 s** (123/123), no `Manual intervention`.
- Every CL log, whole run: `invalid signature` 0, `Invalid` 0, `unknown lean
  block` 0, `Manual intervention` 0, `restream` 0, and no line containing
  `lean lane` on any validator. Teardown clean (`docker ps -q | wc -l` = 0).
- `cargo test` over the four CL crates: all green bar the known environmental
  `test_migrate_command_without_home_flag`, which passes 8/8 when its **test
  binary** (not cargo) gets a fresh HOME. `cargo test -p lean-lane-node`: 33
  passed. `cargo fmt --all -- --check` quiet;
  `cargo clippy --all-targets --all-features -- -D warnings` clean for the whole
  workspace.

**Broke / retracted**
- Retracting the v0.2 guide's claim that sync receive "executes both lanes": it
  validates and stages; the lean append happens once, at that height's decide
  anchor. Retracting the decide row's "5 s grace" — there was none, and until
  this wave there was no enforced 30 s either.
- Retracting the restream comment's implication that a uniform fleet flag makes
  restream framing match the original: it makes the FORMAT match, and a
  store-loaded lean block's CONTENT differs, which is precisely the signature
  break fixed here.
- Note against the earlier "flag off is upstream behaviour" checks: they were
  never in question, but the two halts above were both reachable with the flag
  **on** and neither was caught by any test or by the fleet runs — a healthy
  fleet does not produce either input. The unit tests are their whole coverage.
- Process: `HOME=$(mktemp -d) cargo test …` relocates `.cargo`/`.rustup` and
  rebuilds the world. Override HOME for the test **binary**, not for cargo.

**Decided**
- `process_pending_proposal_parts` passes `lean_bytes_required = true` even
  though the fix brief's parenthetical said both `started_round` call sites
  should pass `false`: those blocks come from network proposal parts (parked
  until the round started), and `false` there would leave the Critical-1 hole
  open on that path. The genuinely store-loaded call site
  (`validate_undecided_blocks`, spec §5.5) passes `false`.
- The anchor trait method is `anchor_by_commitment`, not
  `new_block_by_commitment`: the latter collides with `LeanShim`'s inherent
  method and makes call sites ambiguous to a reader.
- Validation's peer catch-up gets a named 5 s budget. The constant is a
  judgement call (under the local pacer's slack); the argument for it is only
  that unbounded was worse — it runs inside the vote window.

**Open**
- The restream and reuse paths were **not exercised live** (`restream` 0 in every
  log — a 3-minute 100 %-full run never changed rounds). Add a `pause <n>` verb
  to `lean-testnet.sh` to force round changes and cover both halves of Critical 2
  on a real chain.
- Same for Critical 1's Invalid path: nothing healthy produces a header
  commitment without bytes.
- `lean_bytes_required` is a bool at five call sites; a `BlockOrigin` enum would
  make a future call site impossible to get silently wrong.
- Unchanged from the previous entry: the cadence question on the fleet,
  proposer-turn attribution in the health gate, the sync `unknown request ID`
  warnings.

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01YPWyXFV8A1u4RpuQmquB7S

## 2026-09-15 — v0.2 cadence regression found and fixed: the decide anchor was scanning the log

**Changed**
- lean-lane `v0.2` f76c189 (tag `v0.2.0-rc2`): `arc_newBlock{commitment}` no longer runs the
  bounded backward log scan. Idempotency is answered by the in-process index only; staged and
  queued copies are consulted before anything touching the log; the scan is the last resort of
  `arc_getBlockBytes{commitment}` (old-height sync serving). New `log_scans` stat + regression
  test `anchoring_a_staged_block_never_scans_the_log`.

**Measured**
- Standalone single node, 369-tx blocks, anchor `arc_newBlock{commitment}` latency:
  before: 32 ms @h10, 74 @50, 126 @100, 228 @200, **331 ms @300** (+1.1 ms per block of history,
  bounded only at 1024 blocks). After: 23 / 25 / 19 / 17 / **15 ms**, flat.
- Local 5 validators (latency emulation on, N=50, 100 M, pool-target 1500, 800 accounts), fixed
  node, CL images unchanged (843f15e): **119 / 118 / 111 heights/min** over three 60 s samples
  (1.85–1.98 blk/s), **every sampled block 369/369 = 100 % full**, 5/5 byte-identical at 456,
  0 restarts, 0 parked. This is the pre-v0.2 baseline (117–119) again.

**Broke / retracted**
- RETRACTED: the v0.2 cadence numbers in the two previous entries (86.4 and 56 heights/min local;
  0.73 blk/s on the fleet) were measurements of this bug, not of the header-binding design. The
  bug came from my Task 1/2 rulings (scan anchored at head; miss cache cleared on append) plus
  the anchor asking "already canonical?" before "staged?". Every decide paid a full recent-chain
  read+hash. The final reviewer flagged the double lookup as Minor; it was the regression.
- The fleet number (0.73 blk/s at 553/553) is therefore invalid as a v0.2 result and must be
  re-measured with rc2 before it is compared with the campaign's 1.93.

**Decided**
- The anchor path must never touch the log; a test pins `log_scans == 0` for it.

**Open**
- Fleet re-run with lean-lane rc2 (same runner, `scripts/fleet-lean.sh`), then the v0.1-vs-v0.2 A/B
  only if a gap remains. The two parked must-fix findings from the final review still stand.

## 2026-09-15 — fleet re-run with rc2: 1.90 blk/s opening at 100 % full, then an unexplained drift

**Changed**
- `arc-lean-v0.1` (branch `lean-lane-v0.2`) c1f469a: `scripts/fleet-lean.sh` wipes root-owned
  reth/malachite data through a `--user root alpine` container before `rm -rf`, and `up` dies
  if any EL is not fresh (height ≥ 60 within the gate) or any CL has restarted.
- a9d36a8: guide §5 carries the rc2 fleet table; the rc1 0.73 blk/s row is marked retracted
  (scan bug) and the invalid re-run is documented.

**Measured** (fleet run `v02-0914-2301`, lean-lane `v0.2.0-rc2`, 4 machines, N=50, 150 M,
DISTRIBUTED=1 one spammer per machine at 3000 tx/s offered, 10 min, health gate PASS, CL
restarts 0/0/0/0 throughout, 4/4 byte-identical at 1054, DOWN_EXIT=0):
- minute 2: **114.10 blk/min = 1.90 blk/s, 553/553 = 100 % full, 1,052 tx/s, 52,580 payments/s**
- minute 3: 110.16 · minute 4: 107.00 · minute 5: 103.28 · minute 6: 94.43 · minute 7: 90.49 ·
  minute 8: 84.59 · minute 9: 93.00 blk/min — every sample 553/553 = 100 % full; tx/s 1,015 →
  857; payments/s 50,767 → 42,858. Pools 1.5–2.5 k on all four the whole time (never starved).
- The opening window matches the v0.1 campaign point (1.93 blk/s / 55,534 pay/s at N=100/150 M).

**Broke / retracted**
- Fleet re-run #1 (`v02-0914-2243`) INVALID, not reported: validator1's root-owned reth data
  survived the runner's `rm -rf`, its EL came up stale at 1584, its CL crash-looped (restarts
  11 → 19). Cause was the runner, fixed in c1f469a; re-run #2 above is the valid one.

**Decided**
- The v0.2 header binding costs nothing measurable at the operating point; the rc1 gap was
  entirely the anchor scan. The guide's §5 says so with the retraction beside it.

**Open**
- **Cadence drift over 10 min: 114 → 85–93 blk/min at constant 100 % fullness and steady pools.**
  Not the scan (anchor flat 15–25 ms), not starvation. BRAINSTORM candidates: lean log / EL
  persistence growth, backpressure (`execution_persistence_backpressure`), host load on the
  wifi machine. Next: a 20–30 min run sampling per-validator proposer turns + lean-node / EL
  CPU, then the same run on the v0.1 series (tag `lean-lane-v0.1.0`) to see whether v0.1 drifts
  the same way. A drift that v0.1 shares is a campaign property, not a v0.2 regression.
- Two parked must-fix-before-merge findings from the final review still stand (get_value
  reuse-arm equivocation; CATCHUP_BUDGET sticky Invalid).
- Branches/tags not pushed (user pushes): lean-lane `v0.2` @ f76c189 (`v0.2.0-rc2`),
  arc `lean-lane-v0.2` @ a9d36a8, this worklog on `lean-lane-integration`.

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01YPWyXFV8A1u4RpuQmquB7S

## 2026-09-15 (overnight, unattended) — 100k payments/s reached and held on the 4-machine fleet; the 10-minute drift was a lean-node log scan on the serving path

**Changed** (nothing pushed; both branches are new and leave `lean-lane-v0.2` / `v0.2` untouched)
- arc branch `lean-lane-perf` (worktree `~/arc-lean-perf`, base `lean-lane-v0.2` @ a9d36a8):
  887f17b 85ca308 7c2a04e aacce21 07bba98 fa5af41 72fd970 fleet runner (CL/EL log capture before
  teardown, env overrides incl. `BUDGET_GAS`, `CL_EXTRA_ENV`, per-tick CPU/RSS/load per process,
  tailscale-direct + disk preflight, `scripts/fleet-heights.py`); 977f07c one `height_timing` line
  per height behind `ARC_HEIGHT_TIMING=1` + `scripts/height-timing.py`, 6138555 nine `dec_*`
  decide-path fields; 229d558 `ARC_PROPOSAL_CHUNK_SIZE`; 3a93b4b d0477af c46d67b 0c78943
  consensus-db reads key-only / prefix-range, ranged delete on the decide path.
- lean-lane branch `perf` (= `perf-cow`, worktree `~/lean-lane-cow`, base `v0.2` @ f76c189):
  96fb0f2 exact `scan_misses` invalidation; 0dd9185 `LEAN_STATS=1` periodic stats; ff90345 append
  timing + `LEAN_FSYNC=0`; a7c6200 `lean_anchor`/`lean_stage` per-call lines;
  **b627da2 backward log scan capped at the index floor** (the drift fix); cc09ca0 `arc_buildBlock`
  stages its own result; e841e8a `pre_ms`/`locate_ms`/`peer_ms`; 2982dbf announce never backfills
  a block already held; 66c1b43 the anchor waits for the stage it races instead of peer-pulling.
- Fleet data: `~/arc-runs/v02-0914-2322 … v02-0915-0353` on every machine; run logs, captured CL/EL
  logs and timing lines under `~/arc-lean-v0.1/.quake/fleet-runs/<run-id>/`; campaign ledger with
  every ruling and retraction: `~/arc-lean-perf/.superpowers/perf-100k/ledger.md`.

**Measured** (4 machines over tailscale-direct LAN, N=100 fan-out unless noted, one spammer per
machine, 500 ms pacer, every sampled block 100 % of budget, 0 CL restarts, 4/4 lean nodes
byte-identical at the end of every run; per-minute samples over 10 min unless noted)

| run | stack | budget | blk/min over the run | payments/s |
|---|---|---|---|---|
| F1 v02-0914-2322 | rc2 | 225 M | 100.3 → 57.1 (drift) | 72,069 → 40,980 |
| F2 v02-0914-2336 | rc2 | 50 M (99 % full) | 119 → 118 flat | 18.8k |
| F3 v02-0914-2352 | rc2, N=500 | 225 M | 97.4 → 63.0 (drift) | 72,221 → 46,689 |
| F4 v02-0915-0006 | + height timing | 225 M | 99.1 → 63.8 (drift) | 71,149 → 45,837 |
| F5 v02-0915-0031 | + exact negative cache | 225 M | 98.1 → 61.0 (drift) | 70,465 → 43,784 |
| **F6 v02-0915-0044** | **+ scan-free serving (b627da2)** | 225 M | **119.1 → 116.2 flat** | **82,779–85,516** |
| **F7 v02-0915-0057** | same | **300 M** | **111.6–115.2 flat** | **106,914–110,437** |
| F8 v02-0915-0111 | + 1 MiB proposal chunks | 300 M | 113.3–117.1 | 108,611–112,262 (stream 127 → 123 ms: no effect) |
| **F9 v02-0915-0124** | same as F7 | **350 M** | **107.6–109.7 flat** | **120,354–122,667** |
| **F10 v02-0915-0137** | same, **30-min soak** | 300 M | 103.1–115.2 (mean ~112 first 20 min, ~107 last 8) | **98,828–110,437; 26/28 samples ≥ 100k** |
| F11 v02-0915-0211 | same | 400 M | 94.7–105.0 (under the 1.8 floor) | 121,042–134,225 |
| F12 v02-0915-0225 | same | 450 M | 90.9–94.7 | 130,798–136,192 (block bytes saturate ~3.9 MB/s) |
| F13 v02-0915-0238 | same, N=200 | 350 M | 106.7–111.4 (= N=100) | 121,600–127,029 |
| **F14 v02-0915-0253** | same, **30-min soak** | 350 M | 97.5–113.3, mean 104.9 | **109,038–126,744; 27/27 samples > 100k** |
| F15 v02-0915-0326 | + self-stage (cc09ca0) | 400 M | 97.5–103.8 | 124,638–132,703 |
| F16 v02-0915-0340 | + no re-pull (2982dbf) | 400 M | 96.2–109.7 (first 3 min 137.6–140.2k) | 122,963–140,217 |
| F17 v02-0915-0353 | same | 450 M | INVALID (ssh expired after minute 2; that one window: 100.95 blk/min, 145,203) | — |

- Standalone lean node at full 225 M blocks: FLAT (49.6 → 47.3 ms service/block over 1024 blocks,
  n=2; accounts grow ~55/block because the spammer derives recipients from the sender nonce).
- F7 height decomposition (mean ms, 300 M, 1.65 MB): pacer/previous-anchor gap ~290, first part
  115–150, stream 116–129 (13 parts, ~13 MB/s), votes 60–120, decided→anchor 76–98, FCU 3–5;
  period 527–532 ⇒ pacer-bound with ~170 ms slack. 300 → 450 M: stream 127 → 208, decided→anchor
  90 → 203, everything else flat.
- Anchor path mix at 400–450 M (lean `lean_anchor` lines): staged 8–11 ms; `peer` on 25–56 % of
  heights at 150–300 ms (p90 480, max 1.3 s); `index` ~0.5 extra calls/height with p90 226–443 ms.
  Three causes, all in the lean node: the proposer never staged its own block (cc09ca0), the
  announce backfill re-pulled 2.5 MB blocks already held and committed them under the state lock
  ahead of the anchor (2982dbf), and the anchor did not wait for a stage still executing because
  the in-flight claim was published after decode (66c1b43). F16 shows the first two on the fleet
  (index 663 → 185, proposer peer-pulls 0); the third is only loopback-verified (10/10 staged at a
  15 ms gap) — its fleet A/B (F18/F19) did not run.

**Broke / retracted**
- RETRACTED (memo-store-growth suspect 1): "the full FlatState clone grows with txs×N accounts" —
  accounts grow ~55/block; a lone node is flat. Not the drift. Also its "CL redb materialises 62 MB
  per StartedRound" — never did (guarded); the /status and decide-path clean fixes are real but not
  the drift (68 ms only when the pending table fills).
- Cadence-drift root cause, replacing the "open" item of the previous entry: the lean node's SERVING
  path `arc_getBlockBytes{commitment}` (the CL calls it about once per height, plus peer pulls) fell
  back to `locate_by_commitment`'s backward log scan on an index miss — read + hash of every block
  from head, O(height × block bytes), on the node's runtime, delaying the anchor. rc2's negative
  cache was cleared on every append. The same code was in the v0.1 series; the campaign's
  single-window rows never exposed it. `log_scans = 0` throughout F6–F16.
- Runs discarded: v02-0915-0026 (runner put `LEAN_STATS=1` after nohup; fixed fa5af41); F17 (ssh).
- Fleet ops: the tailscale ssh session expired at ~04:00; run v02-0915-0353's containers
  (`validatorN_cl/el`) and lean nodes are still UP on all four machines. After re-auth:
  `cd ~/arc-lean-v0.1 && RUN_ID=v02-0915-0353 ./scripts/fleet-lean.sh down`. Nothing else was touched.
- `pkill -f fleet-lean.sh` killed the controller's own shell (known landmine, §6) — harmless here.

**Decided**
- 100k payments/s is met at the 2 blk/s product target: **300 M / N=100 = 107–110k at 1.86–1.92
  blk/s, held 30 min**; 350 M = 120–123k at 1.79–1.83 (30-min mean 1.75, the edge of the envelope).
  Above that the fleet is bound by ~3.9 MB/s of block bytes through consensus (stream ~13 MB/s ×
  the fixed per-height costs), not by signatures (N=200 = N=100), not by chunk size (F8), not by
  the lean node (8–11 ms staged anchor).
- Not erasure coding, not delayed execution: with the scan gone the 300 M point is pacer-bound
  with ~170 ms slack; the byte lever above 350 M is the anchor race (fixed, unmeasured on the
  fleet) and then dissemination.
- Two-tier spammer/fund rule: runs at N=100 with 800 accounts must stay ≤ 30 min (drain ~2.5–2.7k
  txs/account; bankruptcy ~4.4k).

**Open**
- F18/F19: 400/450 M with 66c1b43 (expected: `peer` path → 0, cadence back toward the pacer at
  400 M ⇒ ~150k). Then a 30-min soak at the new best point.
- Mild ~4 % late decline over 30-min soaks (F10, F14) at constant fullness; `log_scans` stays 0;
  candidates lean RSS (receipt ring → ~2 GB), page-cache pressure on validator3 (sda).
- Parked lean bugs: stage-vs-direct state digest divergence for no-op'd txs; `--snapshot-every 0`
  panics; `stage_block` reads head and clones state in two critical sections; the announce grace
  (250 ms) delays a parked-CL node's backfill by that much.
- The two must-fix-before-merge findings from the v0.2 final review still stand; `lean-lane-perf`
  and `perf` are measurement branches on top of them, not merge candidates yet.
- BRAINSTORM: deferring decided→anchor off the critical path; an index-encoded recipient
  (28 → ~12 B/payment) as the byte-law lever past the ~3.9 MB/s wall.

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01YPWyXFV8A1u4RpuQmquB7S

## 2026-09-15 (morning) — N=1 ceiling: the single-payment lane does 22.7k tx/s pacer-bound and ~32k at the 2 blk/s knee

**Changed** — nothing in code. Fleet re-authenticated by the user; leftover run v02-0915-0353 torn down.

**Measured** (same fleet/stack as the overnight entry: CL `lean-lane-perf` c46d67b image, lean `perf`
66c1b43; N=1 fan-out = one payment per signature, 100 B wire; one spammer per machine; every sampled
block 100 % of budget; 0 restarts; 4/4 byte-identical at the end)

| run | budget | txs/block | blk/min | tx/s = payments/s | offered/pool per machine |
|---|---|---|---|---|---|
| F20 v02-0915-0948 | 150 M | 5,769 | 114.3–117.1 (1.90–1.95) | **10,989–11,263** | 4,000 / 8,000 |
| F21 v02-0915-1001 | 300 M | 11,538 | 115.3–118.1 (1.92–1.97) | **22,175–22,715** | 7,000 / 16,000 |
| F22 v02-0915-1013 | 450 M | 17,307 | 108.6–110.6 (1.81–1.84) | **31,317–31,910** | 10,000 / 24,000 |
| F23 v02-0915-1022 | 600 M | 23,076 | 85.9–89.1 (1.43–1.48) | **33,016–34,253** | 12,000 / 30,000 |

- Lean-node CPU rises with signatures: 0.6–1.7 cores at 150 M, 1.3–2.4 at 300 M, 1.75–2.65 at 450 M,
  up to 2.6 at 600 M (the 20-core and 12-core boxes highest). CL 7–39 %, EL 3–26 %.
- Block bytes through consensus: 3.2 MB/s at 450 M, 3.4 at 600 M — the same wall as N=100
  (3.5–3.9 MB/s), a little lower with 10× the signatures.

**Broke / retracted**
- The campaign's "lean, N=1, 100 M: 6,401 tx/s (85 % full)" row (CLAUDE.md §5) measured the spammer,
  not the chain: with enough offered load and a deep pool target the same lane is pacer-bound at
  11.3k (150 M) and 22.7k (300 M). The "at one payment per signature the lanes are equals (~6 k tx/s)"
  finding is therefore not established for the lean lane; the EVM-lane 6 k figure was not re-measured.

**Decided**
- Single-payment ceiling on this fleet: ~32k tx/s at the 2 blk/s target (450 M), ~34k absolute (600 M
  at 1.46 blk/s). Fan-out at N=100 buys ~3.5–4× on top of it (120–136k payments/s) at the same byte wall.

**Open**
- Re-measure the EVM lane with the same offered-load discipline before comparing the lanes at N=1.
- Everything from the overnight entry (F18/F19 A/B of 66c1b43, the 30-min decline, parked bugs).

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01YPWyXFV8A1u4RpuQmquB7S

## 2026-09-15 (midday) — the two must-fix findings closed; perf merged into v0.2 on both repos; local e2e green

**Changed** (nothing pushed)
- arc `lean-lane-v0.2` fast-forwarded to bf21bcd (33 commits over a9d36a8): the overnight `lean-lane-perf`
  work, plus c8ebcd7 **get_value reuse arm declines the round** (`Ok(None)`) when a signed stored block
  cannot be rehydrated — rebuild only when the row is unsigned (signature is set by `prepare_stream`
  before the store write and before streaming, so signed ⊇ streamed); 868a849 **catch-up budget** — per-peer
  slice of the remaining 5 s, dead-peer memo, and every lag outcome (budget exhausted, peers timed out, no
  peers, local node unreachable, head ran past) returns a transient Err = abstain, never `Invalid`;
  8afb445 `lean_no_verdict_total{reason}` counter + once-per-height warn on the abstain path; 7ada6f3
  `MAX_CATCHUP_LAG = 1024` (a larger gap abstains without spending budget); 3d0366f `LeanValidation` seam +
  wiring tests (lag⇒Err, violation⇒Invalid); pre-merge review fixes 81e5f15 (fmt), 65170cf (chunk-size
  floor = default, knob can only raise), 214715d (store errors propagate on the decide path), dd4f82b
  (timer lock not held across logging), 67d4fd2, 5a55be1, b8c95c1; docs c7ed3d4, bf21bcd.
- lean-lane `v0.2` fast-forwarded to 3b5d11c (16 commits over f76c189): the overnight `perf` work plus
  pre-merge review fixes e0e8ab1 (`from_wire_bytes_with_header` crate-private, trust boundary stated,
  debug recompute), 34df293 (with `LEAN_FSYNC=0`, never resume above a hole: fall back to the newest
  snapshot the log can continue from), 880f9f7 (claims carry an owner id), 93d6974 (**one commit gate
  per append**: parent-link re-check under the lock before apply, idempotent VALID for an already-canonical
  block, conflicting block still an error — closes the double-append windows incl. the sync-queue drain
  and promote-vs-feed), 71636c6, a5b25f0; README 3b5d11c.
- Worktrees `~/arc-lean-fixA`, `~/arc-lean-fixB` removed after merge; `~/arc-lean-perf` (lean-lane-perf)
  and `~/lean-lane-cow` (perf-cow) kept.

**Measured** — local 5-validator testnet from the merged `lean-lane-v0.2` image + merged `v0.2` lean
node (`scripts/lean-testnet.sh`, localdev-lean 100 M, N=100, 3000 tx/s): **119 / 107 / 119 heights/min**
over three 60 s samples, every sampled head block **191/191 = 100 %** of the 100 M budget, **5/5 lean
nodes identical at 1105**, 0 `Manual intervention`, 0 panics, clean teardown. Same as the pre-merge
baseline (117–119). Fleet numbers for the merged tree: not re-run (the fleet A/B of 66c1b43 and the
commit gate is still open).

**Broke / retracted**
- The first e2e attempt's log was lost: `lean-testnet.sh up` wipes `.quake/lean-nodes/` where I had
  opened the log; the run itself was fine (chain at 636, 5/5 identical) and was re-sampled.
- Reviews found no Critical issue on either branch; four Important on arc (fmt gate, chunk floor could
  halt the chain on a downward A/B, swallowed store errors, timer lock across `info!`) and three on
  lean-lane (header-trusting constructor, snapshot/log hole with fsync off, claim ownership) — all fixed
  before merging. Re-reviews: lean-lane CLEAN after follow-ups; must-fix diffs CLEAN.
- Reviewer's ruling kept: the resolver `Ok(None)` row stays `Invalid` — it is only reachable for a
  network frame that shipped no lean trailer at all (a deficient proposal), not for lag.

**Decided**
- Both v0.2 branches now carry the design, the performance fixes and the two must-fix rulings; the
  measurement branches are no longer needed as separate lines.
- Tests: arc-node-consensus 461 + 20 green, clippy `-D warnings` and fmt clean; lean-lane-node 60+
  green in debug and release (repo-wide clippy/fmt were red before this work and are unchanged).

**Open**
- Fleet A/B of the three anchor fixes together (66c1b43) and of the commit gate (93d6974) at 400/450 M,
  then a 30-min soak at the new best point; the 30-min ~4 % decline; the parked lean bugs
  (stage-vs-direct digest divergence on no-op'd txs, `--snapshot-every 0`); an EVM-lane N=1
  re-measure with the same offered-load discipline; a `rustfmt.toml` + mechanical reformat for lean-lane.

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01YPWyXFV8A1u4RpuQmquB7S
