# 100k tps payment lane: dedicated builder + consensus-on-hash + deferred execution

One-page design. Every number below is measured on the 4-machine fleet (canonical drain
campaign 2026-08-12/13, per-stage ledger 2026-08-11); nothing is projected except the
explicit budget targets.

## Why the current design tops out at ~17-20k

Measured law: `latency(n) ≈ 70 ms + 49 µs/tx` → asymptote ~20k tps (plateau observed
16.6-19.2k from 150M gas up). The 49 µs/tx is serial, per height:
~20 µs slowest-validator re-execution (incl. sig recovery) + ~18 µs transport/decode +
~10 µs build/coordination. Votes are hash-sized ≈ 0. The EL itself is 8-15% of a height —
every EL-side lever is measured and exhausted (MISSION-EL.md). The slope IS consensus
coordination; only removing per-tx work from the critical path changes the asymptote.

## Design (four rules)

1. **Total STF for transfers.** An invalid transfer (bad nonce / insufficient balance)
   deterministically becomes a NO-OP; it never invalidates a block. Consequence: every
   ordered tx list is a valid block, so validity never gates agreement. Builder
   misbehavior is bounded to wasting block space (an economics problem, not safety).
2. **Consensus on the block hash only; execute during the vote gap.** Validators vote on
   the (hash-sized) header while executing the body in the background — the 350-900 ms
   vote gap where the EL is measured idle today. Execution can never contradict a vote
   (rule 1), so it leaves the critical path entirely.
3. **Lagged state root, k = 1-2.** Block N's header commits stateRoot(N−k). Keeps
   divergence detection (all four historical consensus bugs were caught by root
   comparison), sync, and light clients — at the measured cost of 0.2-0.5 µs/tx.
   The root is NOT dropped; a tx-merkle-root alone commits to ordering, not balances.
4. **Availability before voting.** A validator only votes on a hash whose body it holds
   (or has an availability attestation for, ePBS/PTC-style) — otherwise the chain can
   finalize an unexecutable block. Dissemination therefore STAYS on the critical path:
   compact blocks (ship 32 B hashes / 6-8 B short IDs; peers reconstruct from their pools,
   12 MB → 0.6-3 MB) first, erasure-coded broadcast for builder egress second.

Plus: **dedicated builder with fallback** — one beefy builder per height (PBS also fixes
ingress: users submit direct to builder, bypassing the measured ~8-10k gossip-admission
ceiling); on builder timeout, round-robin falls back to today's validator self-build.
Slow height, never a halt.

## Budget to 100k tps (1 blk/s, 100k-tx ≈ 2.1 Ggas ≈ 12 MB blocks)

| pipelined stage            | measured today            | needed at 100k | verdict |
|----------------------------|---------------------------|----------------|---------|
| header consensus rounds    | ~70-150 ms fixed          | unchanged      | fine    |
| dissemination              | 18 µs/tx (full bodies)    | 2-4 µs/tx      | compact blocks + coding |
| sig verification (parallel)| ~4 µs/tx (16 threads)     | 0.4 core-s/s   | fine    |
| execution + root (KEPT)    | 2-4 µs/tx fast path       | 0.2-0.4 s/s    | fine off-path |
| **persistence**            | **~44 µs/tx**             | **< 10 µs/tx** | **WALL — batching/NVMe work** |
| **ingress/admission**      | ~8-10k tps ceiling, pool-lock 6-10× | 100k/s | **WALL — direct-to-builder + sharded admission** |

External validation: commonware Constantinople (70k tps @ 250 ms, 50 validators) is
essentially this recipe (simplex + erasure-coded broadcast + separated execution).

## PBS design (builder/proposer separation, concretely)

Keep malachite's RoundRobin **proposer** unchanged (consensus leader, signs/streams the
proposal). The **builder** is a separate always-on service — a beefed-up payment-EL node —
that admits transactions, builds continuously, and serves payloads. Phase 1 needs zero new
message types: the Engine API already IS the builder interface; the proposer's getPayload
simply points at the builder instead of its local EL.

Per-height flow:
- continuous: users -> builder RPC (admission/sig-verify/pool = the ingress wall, solved at
  one sharded node); builder PRE-ANNOUNCES admitted txs to validators (their pools become
  receive-only caches — kills the measured 6-10x pool-lock build inflation on validators,
  and makes compact-block reconstruction possible despite centralized ingress; steady tx
  streaming replaces bursty 12 MB block dissemination).
- at decide(N-1): builder already holds block N — RoundRobin is deterministic, so the next
  proposer's fee_recipient/parent/timestamp are known in advance. This subsumes the parked
  speculative-prebuild work: every miss class (miss_timestamp, miss_parent, FCU races)
  existed because a validator had to GUESS attributes; the builder doesn't guess, it is the
  pipeline — hit rate ~100% by construction.
- proposer fetches header + tx-hash list, streams a compact proposal; validators
  body-complete-check (availability rule) then vote; execution in the vote gap.

Design picks:
1. **Selection**: one designated builder + 1-2 standbys, proposer-side timeout -> fallback
   to local self-build (today's path kept verbatim). No auction/relay machinery — fixed-fee
   lane has ~no MEV; rotate the builder set per epoch against censorship.
2. **Ingress/dissemination hybrid**: centralized submission + pre-announcement (above);
   coded broadcast only for residual missing txs.
3. **Fees**: fee_recipient stays the PROPOSER (Arc's per-tx credit semantics unchanged —
   the semantics all existing gates validate); builder compensated at protocol/ops level.
   No builder-pays-proposer commitment game.

Builder failure matrix: slow/dead -> timeout -> self-build (slow height, no halt);
equivocating bodies -> only one matches the voted hash; censorship -> visible (empty
blocks despite pre-announced txs) -> rotate; junk txs -> deterministic no-ops (total STF).

## Phase-1 experiment spec: remote-builder getPayload (the cheapest real number)

**Goal:** measure the builder-separation rung alone — expected capacity ~22-25k tps (law:
removing the ~10 µs/tx build/coordination slice → 70 ms + ~39 µs/tx). A fleet drain number
here validates the whole ladder's arithmetic before any vote-on-hash work is committed.

**Setup:** builder = a 5th payment-EL (stock image, pool-caps trio, big RAM/NVMe) on the
beefiest box (papaduck NVMe). All users/spammers submit to the BUILDER; builder peered to
validator ELs (admin_addPeer) so txs pre-announce via existing gossip.

**Two code touches discovered while speccing (both small, both CL-side — this branch
explicitly lifts the no-CL constraint for design work):**
1. **getPayload/validate split.** The CL uses ONE Engine per lane for both build and
   validate. Add optional `--payment-builder-endpoint`: `get_value` (build path) uses the
   builder engine when configured + healthy, falls back to local on timeout (same skip-round
   semantics as the 509f404 fix); `received_proposal_part`/newPayload/FCU validation stays
   on the LOCAL engine untouched. Additive; None = today's behavior byte-for-byte.
2. **Builder follow feed.** The builder EL must track the canonical head to build on it.
   Cheapest: ONE designated CL (the builder's co-located validator) also forwards
   newPayload+FCU to the builder engine — the dual-EL "second engine" pattern, additive.
   (Fallback ops heal if builder falls behind: reth p2p backfill via forkchoiceUpdated,
   as in revive-val.sh.)

**Run plan:** fresh fleet chain + builder; all 4 CLs get `--payment-builder-endpoint` →
builder authrpc (shared payment-jwt, tailscale IP; 2-8 ms LAN RTT is one getPayload per
height — negligible). Then `drain-campaign.sh` at 150/300 M, n≥2, vs the canonical table
as same-methodology control. Success = capacity moves 16-19k → ≥22k with all-validator
agreement over 100+ blocks. Gates first: both offline gates + build_gate on the CL change;
val-behavior A/B is inherent (fallback path = stock).

**Explicitly out of scope for phase 1:** vote-on-hash, total-STF, lagged root, compact
blocks — phase 1 keeps today's consensus rules exactly, so its number isolates the
builder slice alone.

## Phase-1 RESULTS (2026-08-17/18, fleet, paired same-chain arms)

### Persistence profiling, first pass — per-table byte attribution (2026-08-18, 1.57M txs)

Table/segment growth on val1's payment EL across a 25-min sustained window (reth_db_table_size +
static-file segment gauges, host port 19001; ~479 B written per 122 B tx = ~4x byte amplification
before MDBX page-COW):

| table/segment | share | B/tx | note |
|---|---|---|---|
| transactions (static) | 45.6% | 218 | append-only already (V2) |
| receipts | 30.6% | 147 | fully derivable for plain transfers -> synthetic candidate |
| account-change-sets | 12.8% | 61 | UNWIND data — Arc has BFT finality, no reorgs: pure waste |
| transaction-senders | 10.6% | 51 | recomputable cache |
| ALL state tables (HashedAccounts etc.) | ~0.2% | ~1 | closed account set: state is NOT the write cost |

Persist duration histogram same window: 185.1s over 2,732 saves = **67.7ms/block at only 569
txs/block** — fixed per-block overhead dominates small blocks (44µs/tx figure holds for full ones).
IMPLICATION: receipts + change-sets + senders = **54% of bytes are droppable/derivable on a
payment lane without touching the tx bodies**; state tables are irrelevant. The surgery list from
the caveats section is confirmed in priority order: (1) drop change-sets (no reorgs on Arc),
(2) synthetic receipts, (3) lazy senders.

FINAL-RUN CAVEATS (methodology): sustained-at-1G window was mis-governed (POOL_TARGET=12000 caps
the pool below one 47,618-tx block -> 569 txs/blk, 1.1k tps — NOT a capacity statement; 1G
sustained needs POOL_TARGET ~100-150k and remains admission-bound ~8-10k regardless); the
post-churn 1G drain read 3,432ms/13.9k = the documented aged-chain artifact (age 3,232 after 25min
churn) — the canonical 1G capacity remains the fresh-chain n=2: **29.4k/29.1k tps @ ~1.62s**.

### Deferred exec SOLIDIFIED at 1G — ~29k tps with zero reth modifications (2026-08-18, n=2)

Two fresh fleet chains, deferred on all 4 CLs (env-verified), drain ladder incl. 1G, 50-height
agreement checks both campaigns, sync-assert audit fixed (structural-Valid vs engine-Invalid is
the legal byzantine disagreement; assert kept for the bug direction only):

| gas | campaign 1 | campaign 2 |
|-----|-----------|-----------|
| 150M | 451ms / 15,832 | 439ms / 16,284 |
| 300M | 844ms / 16,924 | 673ms / 21,217 |
| 500M | 1,082ms / 22,001 | 1,082ms / 22,014 (identical!) |
| 1G | 1,620ms / **29,390** | 1,639ms / **29,055** |

New height law under deferred ≈ **290ms + 28µs/tx** (was 70ms + 49µs/tx gated). 1G rows are
3 full 47,618-tx blocks each (pool 190k = 4 blocks, resolution-limited). 300M has spread.
The 30k-without-reth-mods target is effectively met; past it = persistence/dissemination/
admission work (see the caveats above).

### Compact payment proposals (ARC_COMPACT_PAYMENT_PROPOSALS) — BUILT, LOCAL GATE PASSED, FLEET A/B PENDING (2026-08-18)

Live payment proposals stream the payment lane as 32B tx hashes (COMPACT_LANE_BIT marker in the
lane-frame length prefix; ~74% smaller payment section, 2.9MB -> 0.77MB at 24k txs). Receivers
rebuild the exact payload from their local payment EL (batched eth_getRawTransactionByHash,
hash-verified per tx) BEFORE validation/storage — stores, decide, restream and value-sync all
still carry full payloads; sync stays full-format by design (decided txs leave the pools).
Decode is unconditional, only emission is flag-gated (mixed new-binary fleets interop; old
binaries fail closed naming the flag). The deferred-exec structural check doubles as the
byte-exact reconstruction gate (tx root -> block hash). Restream frames by own flag: uniform
fleets correct; cross-format restream in mixed-flag fleets fails signature verification safely.

Found en route: **reth's JSON-RPC batch limit is 100 requests** and oversized batches get a
SINGLE error object, not an array (first live run stalled at h82 on it) — the fetch sub-batches
at 100, concurrent over HTTP, sequential over IPC.

Local live gate (4 validators, deferred+compact stacked, congest load): 120-height dual-lane
agreement, ZERO reconstruction errors / assembly failures on all 4 CLs, restart leg OK.
Commits 521ca79, 1b3b6d5, 063b67b, 2fcc813 + sub-batch fix. 267 lib tests.

PENDING: fleet paired drains (deferred-only vs deferred+compact) to price the ~18µs/tx
transport+decode term — blocked on tailscale re-auth at time of writing. RISK to watch on the
fleet: pool-gossip lag on live ingress (a tx in the proposer's pool but not yet in a remote
pool = reconstruction miss = Nil round); zero misses locally, but fleet gossip is cross-machine.

### Vote-on-hash increment 1 (ARC_PAYMENT_DEFERRED_EXEC) — MEASURED POSITIVE (2026-08-18)

First rung of consensus-on-hash: validators vote on the payment lane after STRUCTURAL validation
only (block-hash consistency with recomputed tx root + Arc pbbr/requests-hash conventions, lane
lockstep, parent link); real EL2 execution runs concurrently in the vote gap (fire-and-forget at
validation) and is anchored at decide BEFORE commit (idempotent newPayload; INVALID => loud
deterministic halt before anything persists). Sync path still fully re-executes. Commits 7d081ce,
bc6c1e4, 2a2e06d + requests-hash fix.

Paired same-day fleet drains (fresh chain, control first, deferred arm on the OLDER chain =
anti-deferred bias; FILL_TARGET=190k, n=1/size/arm):

| gas | control | deferred | delta |
|-----|---------|----------|-------|
| 150M | 486ms / 14,690 tps | 420ms / 16,987 | **+15.6%** |
| 300M | 912ms / 15,671 | 845ms / 16,915 | **+7.9%** |
| 500M | 1,284ms / 18,547 | 1,013ms / 23,510 | **+26.8%** |

**23.5k tps @ 500M breaks the ~20k fitted asymptote of the old law.** Per-tx saving ≈ 5-11µs
(about half the ~20µs/tx slowest-peer re-exec term: the gap-hidden execution still has to fit,
and transport/decode is untouched). Decide-anchor wait measured 1.3-1.8ms/height on the local
demo (execution always finished within the vote gap). Live gates passed: 120-height agreement
both lanes x4 validators + CL restart leg; the first (buggy) build also demonstrated the loud
no-fork halt when the structural check rejects (all validators vote Nil, chain stalls, zero
divergence — recovered by fixing + restarting CLs on the same chain).

Found en route: real Arc headers carry Prague requestsHash = sha256(empty); into_block_raw does
not set it (nor pbbr) — both must be set explicitly during reconstruction or every real block is
rejected. Unit tests alone were circular (same code built and checked the hash); the live gate
caught it immediately.

STATUS: experiment-fleet configuration, default OFF. Before any wider use: total STF in the EL
(a byzantine proposer can craft a structurally-valid-but-unexecutable payload => today that is a
synchronized halt at decide, attributable to the signed proposer, but still a halt); n>=2
reproduction; sync-under-load leg; the undecided-store validity dedup assert audit (risk 6 of
the implementation plan).

NEXT RUNGS (same doc, in order of expected value): compact blocks / tx-hash proposals (the
~18µs/tx transport+decode term), lagged state root, erasure-coded broadcast.

### v1.2 (feed+kick at validation time) — MEASURED NEGATIVE, REVERTED (2026-08-17)

Hypothesis: kick the builder when the block is *validated* (received_proposal_part) instead of at
decide, so the builder executes N in parallel with the vote and has the whole gap for 3 timestamp
candidates (t0..t0+2). Paired same-day fleet drains (fresh chain, control first, FILL_TARGET=190k):

| gas | control | v1.2 | delta |
|-----|---------|------|-------|
| 150M | 467ms / 15,296 tps | 491ms / 14,549 | **-5%** |
| 300M | 848ms / 16,855 | 918ms / 15,560 | **-8%** |
| 500M | 1,349ms / 17,645 | 1,477ms / 16,071 | **-9%** |

Hit rates: val1 47%, val2 82%, val3 (builder-co-located) 31%. Miss class = candidates=0.

Why it failed (three structural mechanisms, from live logs):
1. At capacity cadence the builder must EXECUTE the fed block (~300-700ms at these sizes) before
   the head-check passes and speculative builds start; get_value often arrives first → empty stash.
   Decide-time (v1.1) is actually better-positioned in time.
2. Stretched rounds blow even the 3-timestamp window (observed ts_needed = ts_prebuilt+4..6s).
3. The regression itself: ALL FOUR validators serialize + ship the full multi-MB payload to the
   builder at validation time, plus the decide reconciliation feed = 8 full-payload feeds/height —
   builder contention + CL-side serialization on the critical path. val3 (same machine as the
   builder EL) had the worst hit rate.

Reverted (ea6e92c); v1.1 (decide-time kick, dual timestamp) stands as the rung's result:
+7% @150M / parity @300M / +11% @500M, 54-82% hit rate. Further prebuild gains are gated on a
pending-visibility redesign, not on kick timing — parked. Next rung: vote-on-hash.

- **v0 (on-demand remote build): -5..-10% capacity.** The build never left the critical
  path (proposer waits for the remote build), and the builder's follow-feed catch-up
  execution + 2 head-check RTTs landed ON the path. Withdrawn, replaced by v1.
- **v1.1 (prebuilt + dual-timestamp): +7% @150M (483->450ms), parity @300M, +11% @500M
  (1,414->1,277ms, 18,649 tps)** — control-first ordering, builder arm at OLDER chain age
  (conservative), fills all >=190k. Hit rate under drain load 54% (misses = empty stash:
  the kick's two builds compete with the loaded builder inside the decide->get_value gap;
  every miss falls back to stock) — so the per-hit win is ~2x the headline and hit-rate
  work is direct headroom. The dual-timestamp trick killed the second-rollover miss class
  outright (was 7/10 misses).
- Mechanism validated end-to-end: builder follows via decide-feed, p2p-backfills itself
  from genesis mid-chain, all-validator agreement throughout, fallback never broke a
  height. Ops findings: builder needs the lane's Engine API V4/V5 fork config (-38005
  otherwise); the value-sync livelock env fix had been remote-only (local val1 wedged on
  every fall-behind until 7857704).
- Ladder position: measured +7-11% at ~54% hits vs the rung's ~+10 us/tx prediction —
  consistent. The big prize remains vote-on-hash (execution off the height entirely).

## Failure modes

- Builder withholds body after header commit → availability rule blocks the vote; height
  falls back to self-build. Bond/slash optional hardening.
- Builder packs invalid txs → deterministic no-ops (rule 1); fee economics discourage it.
- Executor divergence → lagged root mismatch at N+k → loud halt, same alarm as today,
  k blocks late.
- Builder death → fallback path; liveness degrades to today's cadence, never below.

## Where the work lands (honest)

- **malachite/CL (the real project):** vote-on-hash + availability rule + compact blocks +
  coded broadcast + lagged-root header field. Out of scope under the current
  no-CL-changes constraint — this doc is the case for lifting it.
- **EL/arc-evm (days, consensus-critical):** total-STF transfer semantics; deferred root
  plumbing. Both must pass the offline gates (built + validated blocks) and the
  val1-vs-3-stock live A/B before trust — four bugs say assume nothing.
- **ops:** persistence overhaul (batch writes, NVMe on all validators — measured 55×
  spread between home-disk and NVMe fsync), admission sharding.

## Open questions

1. k (root lag) sizing vs worst-case execution stall; behavior when executor falls > k behind.
2. Availability mechanism: full-body-before-vote (simple, keeps dissemination on path) vs
   attestation committee (faster, more machinery).
3. Fee handling for no-op transfers (charge intrinsic gas? free? spam vector analysis).
4. Does the 2.1 Ggas block need chainspec bounds raise (current payment-lane cap 1 Ggas —
   crates/execution-config/src/chainspec.rs).
5. MEV/censorship surface of a single builder on a payment lane (fixed-fee lane shrinks
   MEV, but censorship monitoring still needed).
