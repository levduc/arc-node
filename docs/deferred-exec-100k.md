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
