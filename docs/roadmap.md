# Lean payment lane — scope and plan (drafted 2026-08-25)

Written after the measurement campaign closed, against the code as it stands.
Each item says what is **true today** (verified in the source, not remembered),
what the **options** are, and what needs a **decision from Duc**. Nothing here is
started yet.

---

## 1. Run it on one machine (no tailscale)

**Today.** `lean-smoke.sh` already runs three lean nodes on loopback with a
stand-in CL — no tailscale, no docker. But the *full stack* single-machine path
(4 CLs + 4 EVM ELs + 4 lean nodes, wired together) lives only in a scratchpad
script that was never committed; and `lane-bench.sh` assumes four machines (its
census, pool wipe and corpus generation all go over `tailscale ssh`).

**Plan.**
- **1a. `local-testnet.sh`** — commit the single-machine full stack: boot the
  demo (4 validators, 12 containers) + 4 lean nodes on this box, wire each CL to
  its lean node through the docker gateway. One command, no tailscale.
- **1b. `lane-bench.sh --local`** — when `fleet.env` is absent (or `FLEET=local`),
  run every step locally: census over local containers, pool wipe over local
  ports, corpus generation in-process. *Same protocol, same gates* — the fleet
  path becomes the special case rather than the default.
- **1c. README section** — "run this project on your laptop in three commands".

Tailscale stays for *your* 4-machine fleet; it stops being required to use the
project. Effort ~1 day; verification is running a full `lane-bench lean` arm on
one box and getting a plausible number with fullness reported.

---

## 2. Is the lean EL "all we need"? — the honest gap list

### 2a. Header / commitment: no state root today

`commitment = keccak(parent ‖ number ‖ timestamp_ms ‖ txs_hash)`. There is **no
state root anywhere**. Consequences, in order of how much they matter:

- A node cannot verify state without **re-executing from genesis** (or trusting a
  snapshot). Fast sync is "trust the peer".
- **No proofs for users** — a wallet cannot be shown "your payment is in, here is
  the proof"; it must trust an RPC.
- **Nothing outside the lane can verify lane state** — which blocks any bridge to
  the EVM lane (see §5a) without running a lean node.

Options: (a) keep none — the BFT certificate is the authority, simplest, status
quo; (b) **checkpoint root every K blocks** — a flat-state digest, near-zero
steady-state cost, gives sync verification and a bridge anchor; (c) per-block
state root — we measured ~0.8 ms for a 16 k-account trie, so affordable, but it
is a permanent per-block cost for a property only (b) needs.

**Recommendation: (b).** **Decision needed** — this is a wire-format change and
should be made before anything else is built on the current header.

### 2b. Minimal header fields

Present: parent (32 B), number (8), timestamp\_ms (8), txs\_hash (32). Missing
candidates, with my read:

| field | verdict |
|---|---|
| `version: u8` | **add** — there is no way to evolve the block format today |
| checkpoint root | add if 2a lands as (b) |
| proposer | **add** — needed to make fees real (§2d) and to attribute blocks |
| gas/tx count | skip — derivable |
| receipts root | skip for now — logs are derived, not committed |

### 2c. Mempool: one real architectural gap

The reth pool itself is fine (its admission, nonce and eviction logic is exactly
what we wanted to reuse). But **transactions do not propagate between lean
nodes** — only *blocks* do (announce/pull). A user submitting to node A has their
payment included only when A is the proposer, from A's own pool. Every benchmark
this campaign fed all four nodes directly, which hid this completely.

That is fine for a benchmark and **not viable for a product**. Options: (a) a
tx-gossip mesh like the block mesh; (b) a **forwarder** — the node forwards
submissions to the current/next proposer (the op-reth `--rollup.sequencer-http`
pattern, and the shape the isolated-ingress work already validated); (c) accept
it and make submission proposer-aware in the client.

Secondary, smaller: `getTransactionCount` is pool-inclusive (it bit us twice);
no priority ordering exists because every tx pays the same protocol price;
per-sender slot caps need a documented setting.

### 2d. Fees: two correctness problems, not just design

- The **beneficiary is a hardcoded placeholder** (`Address::with_last_byte(0xbe)`)
  taken from *node config*. It is consensus-critical state input that is **not in
  the block**: two nodes configured differently compute different balances and
  therefore different commitments — a silent fork. It works today only because
  every node runs the default. **This should be fixed regardless of anything else
  on this list**: take the beneficiary from the block (needs the proposer field
  in 2b) or fix it in protocol.
- An **invalid transaction pays nothing**. `validate_and_debit` returns before any
  debit, and total-STF makes the block still valid. So a byzantine proposer can
  fill a block with invalid transactions for free: they cost every validator
  bytes and an ecrecover, and cost the attacker nothing. Bounded (a block is
  budget-capped, and it wastes the attacker's own turn) but it is a real
  griefing vector. Options: charge the proposer for included-invalid txs; or
  reject blocks containing invalid txs — but that trades away exactly the
  halting-freedom that total STF was chosen for. **Decision needed.**
- Beyond correctness: there is no fee *market* (fixed price), so under real
  congestion there is no ordering signal and no spam price. That is arguably
  correct for a payment lane with a fixed $0.000021 promise — but it should be a
  stated design decision, not an accident.

### 2e. Is the 1→N transaction "finished"?

Wire format, execution (serial + parallel, differentially tested), pool
integration, gas metric and fee arithmetic: **done**. What "finished" still
needs: tx propagation (2c), the two fee fixes (2d), a version/upgrade path (2b),
a `MAX_OUTPUTS = 10_000` DoS review, and a decision on whether users get any
proof of payment (2a). The u64-gwei amount precision is a documented trade and I
would leave it.

---

## 3. After the EL stabilises — what actually has headroom

**Parallel execution during the voting round is already done and is not a lever.**
Execution runs in the vote gap (v1.3/v1.3.1 staging) and costs ~1 µs/output; the
sender-partitioned parallel executor exists and is differentially tested. Even
making execution instant would move a height by a few percent. Parking it is the
honest call.

What has real headroom, in order:

1. **Compact proposals** — propose tx *hashes* against pre-distributed bodies.
   Attacks the 57 %-of-height proposal stream, the p90 tail and the value-sync
   storm at once; byte-law estimate ~10× fewer consensus bytes. (Honestly: this
   is Narwhal's dissemination/agreement separation applied to our stack.)
2. **Height pipelining** — overlap height *N+1*'s dissemination with *N*'s votes.
3. **Batch ecrecover** — the admission ceiling both lanes hit (~6 k tx/s at N=1)
   is ecrecover-bound; SIMD batching is a known 2-4×. Only matters if low-N
   traffic matters.
4. **Multi-lane generalisation** — N lanes under one certificate. This is the
   conceptual contribution and the thing worth formalising if publication ever
   comes back on the table.

---

## 4. Logging convention (so this is reviewable remotely)

`CLAUDE.md` now instructs every session to append to **`docs/worklog.md`**: what
changed (with commit hashes), what was measured (numbers *with fullness and
cadence*), what broke, what remains open, and any design decision taken. One
dated entry per session, append-only, reviewable by `git log`/diff from anywhere.
Brainstorms go in the same file marked as such, so speculation is never confused
with measurement.

---

## 5. Things not on the original list that I think matter

### 5a. The lane is an island — no cross-lane value flow
Nothing moves value between the EVM lane and the payment lane. Today the only
funds that exist are the ones written into the lean genesis. The paper describes
a v0 cross-sector flow; there is no code. Without it there is no product story:
users cannot get money in or out. This is probably the **largest missing feature**
on the list, and it depends on §2a (something outside the lane must be able to
verify lane state).

### 5b. Threat model, written down
Byzantine proposer (covered by total STF), cross-lane equivocation (covered by
the joint `value_id`), replay (covered by the lane domain in the signing hash) —
but free-invalid-tx griefing (§2d), `MAX_OUTPUTS` amplification, and mempool spam
under a fixed fee are open. One page, so we stop rediscovering these.

### 5c. Consensus-critical gates in CI
The differential gates (stage↔promote byte-identity, serial↔parallel state
equality, pinned wire vectors, lockstep) run only when someone remembers.
They are exactly the tests that must run on every commit. The lean crates are now
workspace members, so this is mostly wiring `make test-unit` in `ci.yml`.

### 5d. State growth
The flat state map grows with users forever. No pruning, no archival, no measured
memory ceiling for a realistic user count. Worth measuring before it is a
surprise.

### 5e. Observability
The lean node exposes a handful of atomics. Prometheus metrics (admission rate,
pool depth, build/exec/persist timings, announce/backfill counters) would have
saved several of this campaign's debugging sessions outright.

### 5f. Generated-file hygiene
`assets/localdev/genesis.json` is generated *and* checked in, so every demo run
with different parameters dirties the tree (it did during this campaign). Either
gitignore it or have the demos write to a scratch path.

---

## 6. Decisions taken (2026-08-25, Duc)

- **Checkpoint state root: YES.** Design it **lagged** so it stays off the
  consensus path (below).
- **Header gains `version` + `proposer`.** Wire-format break; do it once,
  together with the checkpoint field.
- **Invalid-tx fee: charge the proposer.** Do not reject the block, do not halt —
  halting-freedom under total STF is kept.
- **Payment-lane cadence may differ from the EVM lane** (product is fine with
  1 blk/s or 0.5 blk/s for payments) — see §7 before building it.

### Lagged checkpoint — why it is off the critical path

Block *N* carries the state root **as of the last checkpoint boundary at or
before N−K**, not its own post-state. Then:

- the **proposer** already has that state (it was executed long ago) — nothing to
  compute before proposing;
- **validators vote structurally**, as they already do — no execution before the
  vote;
- verification happens when they execute in the vote gap / at the anchor. A
  mismatch is an **attributable halt**, exactly the path that already exists for
  execution divergence.

So neither proposing nor voting waits on a root. Cost is once per K blocks; with
K≈1024 that is once per ~8 minutes at 2 blk/s. Note the flat state grows with
users, so an O(n) walk over millions of accounts is the wrong shape — use the
incremental accumulator from `experiments/utxo-state` (~11 µs/update, flat in
state size) rather than re-hashing the world at each boundary.

---

## 7. Cadence: what the data says before we build anything

**The payment lane is currently pacer-bound, not capacity-bound.** Measured on
v1.3.1 at the 500 ms target:

| budget | outs/blk | blk/s | payments/s | MB/blk | MB/s | ms/blk |
|---|---|---|---|---|---|---|
| 150M | 28,700 | 1.93 | 55,391 | 0.82 | 1.59 | 518 |
| 175M | 33,500 | 1.93 | 64,655 | 0.96 | 1.86 | 518 |
| 200M | 38,300 | 1.94 | 74,302 | 1.10 | 2.13 | 515 |
| 225M | 43,100 | 1.93 | 83,183 | 1.24 | 2.39 | 518 |
| 250M | 47,900 | 1.86 | 89,094 | 1.37 | 2.56 | 538 |

Heights sit at ~518 ms from 150M to 225M — that is the **pacer holding them**, not
the chain straining. Only at 250M does the height start to slip. So there is
free headroom at 2 blk/s we have not taken (+7 % by moving to 250M).

**What slowing down might buy.** A local fit over the two capacity-bound points
gives height ≈ 343 ms + 0.14 µs/byte, which extrapolates to 4.6 MB blocks at
1 blk/s → ~162 k payments/s. **Do not trust that number**: the same law predicts
710 ms for the 525M drain's 2.59 MB blocks, and we measured **1,155 ms** — it
underpredicts by 1.6× at that size. Real superlinearity above ~1.5 MB (stream
p90, slowest-peer gating, bigger sync frames). Correcting for it puts 1 blk/s
nearer **~100–120 k payments/s**; if bytes/s is simply capped around 2.5 MB/s it
is **neutral** (~87 k). Honest range: **87 k–160 k, most likely ~110 k.**

**Effect on Malachite agreement — three real risks, one of them a footgun.**

1. **Timeouts do not scale with cadence.** `propose` is 3,000 ms today. At 1 blk/s
   with 3–4.6 MB payloads, stream+assemble can exceed it, the round fails, and
   cadence collapses — the exact failure mode that cost this campaign several
   runs. Timeouts are on-chain consensus params (same mechanism as
   `targetBlockTimeMs`), so they are settable — **but they must be raised with the
   cadence.** This is the first thing to get wrong.
2. **Tail risk grows faster than size.** Vote rounds are gated by the slowest
   peer; a bigger payload widens that tail (the wifi validator was the canary all
   campaign).
3. **Sync frames get bigger too.** A validator that misses a large height fetches
   a large frame — the value-sync storm gets *worse* with block size, not better.

**Per-lane cadence is already structurally supported**: `lean_payload` is an
`Option`, and `commit_lanes(evm, None)` is the EVM-only case, so "lean block every
K heights" is a deterministic proposer rule (`height % K`), not new consensus
machinery. But it adds height-duration variance (cheap/expensive alternating), and
timeouts must then be sized for the expensive height.

**Recommended order — measure before designing:**

1. Take the free headroom: **250M at 2 blk/s** (already measured: 89 k payments/s).
2. **Measure global 1 blk/s** at 300/450/600M, N=100, 10-min windows, with
   `propose` timeout raised proportionally. ~1 hour with `lane-bench.sh`. This
   decides the question with data instead of extrapolation.
3. Only if 1 blk/s wins clearly *and* the EVM lane must stay at 2 blk/s, implement
   the skip-heights rule for per-lane cadence.
4. Note that **compact proposals (§3.1) change this calculus entirely** — 10×
   fewer consensus bytes puts the pacer back in charge at any cadence. If that
   lands, slowing the chain may be solving a problem that disappears.

---

## Suggested order

0. **Cadence experiment (§7.1-7.2)** — one hour, decides a design question.
1. **§2d beneficiary fix** — consensus-critical, small, no reason to wait.
2. **§1 single-machine** — unblocks anyone else touching the project.
3. **§2a/2b header decision** — wire-format break; everything else builds on it.
4. **§2c tx propagation** — the gap between benchmark and product.
5. **§5c CI gates**, **§5b threat model** — cheap, prevent regressions.
6. **§5a cross-lane flow** — the big product feature.
7. **§3.1 compact proposals** — the remaining performance lever.

Endurance testing (12 h+ at the operating point, with `--fanout-amount` so
senders do not bankrupt) should run in the background of whichever item is
active; it is the cheapest way to keep the numbers trustworthy.

## 8. Mixed-N workload + parallel recovery (planned 2026-08-25, decisions: Duc)

**Decisions.** Stay at 2 blk/s. Default mix = **equal-by-tx** over N ∈ {1,5,10,50,100}
(each N is 20 % of *transactions*; avg N ≈ 33). Keep one **equal-by-payments** stress
mix (each N carries 20 % of *payments*; tx counts skew to N=1). Parallelize
**recovery only** — the bench shows ecrecover gains 7–8× from rayon while parallel
*execution* is neutral-to-slower (0.33→0.57 µs/out at N=1), so execution stays serial.

**Why they belong together.** Pure N=100 blocks carry ~290 sigs (serial recovery
~9 ms — irrelevant). The by-payments mix at 225M carries ~5,700 sigs → ~170 ms serial
recovery, which blows the ~200 ms vote gap and loses the staging race; parallel is
~22 ms. Mixed workloads are where parallel recovery stops being optional.

### Work items

- **P1 spammer**: `--fanout-outputs` accepts `1:20,5:20,10:20,50:20,100:20`
  (weights = tx share). Sampling is **deterministic by tx index** (cycle a weighted
  pattern, no RNG) so a corpus is reproducible byte-for-byte and realized shares are
  exact. Single-value spec unchanged.
- **P2 node**: `decode_block_txs` gains a parallel-recovery path
  (`recover_all_parallel`), env-gated `LEAN_PARALLEL_RECOVERY=1`, default off.
  Skip-invalid semantics and item order must be byte-identical to serial.
- **P3 harness**: `lane-bench.sh` accepts a mix spec; the measure step computes
  **fullness by gas** (Σ 21000+5000·Nᵢ from per-tx output counts, which the parser
  already reads) and adds `avg_n` and `sigs_blk` to every JSON row.
- **P4 smoke**: `lean-smoke.sh --mixed` arm (see V4/V5 below).

### Verification ladder — all local until the last step

Every claim gets a test at the cheapest level that can falsify it. A step runs only
after the one above it passes.

- **V1 — corpus determinism (unit, seconds).** Same spec → byte-identical corpus
  (sha256), exact 20 % tx share per N, per-tx fee = lean_fee(Nᵢ). Falsifies: P1.
- **V2 — admission (smoke node, seconds).** Submit a mixed corpus sample; require
  `pending == submitted` (no underpriced/malformed rejections across N values).
  Falsifies: fee/pool arithmetic under mixed N.
- **V3 — recovery differential (unit, seconds).** For mixed blocks *with invalid
  txs interleaved*: parallel decode+recover returns the identical item vector
  (order, skips, senders) as serial. This is the consensus-critical gate for P2.
- **V4 — replay invariance (local, ~1 min).** Drive the *same block bytes* into two
  fresh nodes — flag off vs flag on — and require the **same head commitment**. A
  live restatement of V3 through the real node path.
- **V5 — convergence under mixed load (local, ~2 min).** `lean-smoke.sh --mixed`:
  3 loopback nodes, mixed corpus, N heights → 3/3 nodes one commitment, blocks
  non-empty, measured avg N ≈ 33 ± 2, gas-fullness ≈ 100 %. Falsifies: P3
  accounting and any mixed-load wire/pool issue.
- **V6 — the mechanism, live (single-machine testnet, ~1 evening).** Full stack on
  one box (4 CLs + 4 EVM ELs + 4 lean nodes — the demo topology). Six 10-min arms
  at 2 blk/s / 225M: {pure-100, mixed-by-tx, mixed-by-payments} × {serial,
  parallel}. **Pass** = the serial/parallel gap appears *only* where predicted:
  - pure-100 and mixed-by-tx: serial ≈ parallel (recovery ≤ 40 ms either way);
  - mixed-by-payments: serial arm loses staging races (anchor p50 rises, possibly
    cadence dips); parallel arm restores the pure-100 profile.
  Caveat recorded with the numbers: one box shares CPU among 4 validators, so
  *absolute* throughput is not fleet-comparable — but serial-vs-parallel on the
  same box is a controlled comparison, which is what V6 claims.
- **V7 — no-regression + fleet numbers (fleet, optional last).** Pure N=100 must
  reproduce the campaign row within the known ±15 % band, then one mixed-by-tx
  fleet arm for the §5-grade number. Only this step needs tailscale.

**Adoption rule.** `LEAN_PARALLEL_RECOVERY` flips to default-on only after V3–V6
pass and one soak (≥2 h mixed load, local) shows zero divergence; the flag stays
for one release as the rollback.
