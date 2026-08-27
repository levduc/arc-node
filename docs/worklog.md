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
