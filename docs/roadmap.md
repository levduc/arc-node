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

## Suggested order

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
