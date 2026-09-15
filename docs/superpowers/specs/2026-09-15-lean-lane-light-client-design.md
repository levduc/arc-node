# Lean lane — light-client proofs (inclusion, balance, receipt)

Date: 2026-09-15. Status: **BRAINSTORM** — for the repo owner to review, nothing
implemented, nothing measured. Every number below marked *(est)* is arithmetic
from measured inputs, not a measurement. Measured inputs are cited to
`docs/lean-lane-integration.md` (fleet runs of 2026-09-15) and to the lean-lane
sources.

Companion designs: `2026-09-14-lean-lane-header-binding-design.md` (v0.2,
`prev_randao` binding — assumed shipped throughout), `docs/lean-lane-integration.md`.

## 0. Problem

The lane has **no state trie, no receipts trie, no transaction trie, no logs**
(`state.rs`: "flat account state … no trie, no root"; `chain.rs`: "Execution
results (balances) are NOT committed per block"). That is exactly why it runs at
107–136 k payments/s. The bill is that a wallet, an exchange or an auditor has
no cryptographic way to answer three questions without running a full node:
(a) **inclusion** — "payment (tx `T`, output `j`) is in certified block `h`";
(b) **state** — "account `A` had balance `B`, nonce `n` at height `h`";
(c) **receipt** — "that payment was *executed and credited*, not no-op'd".
This document plans all three without putting Merkle work on the validators'
per-height critical path.

### Operating point used for all estimates

From the 2026-09-15 fleet runs (4 validators, 500 ms pacer, 100 % fullness):

| point | budget | txs/blk | blk bytes | blk/s | payments/s | period |
|---|---|---|---|---|---|---|
| N=100 "fan-out" | 300 M | 575 | 1.65 MB | 1.86–1.92 | 107–110 k | 527–532 ms, **~170 ms slack** |
| N=100 "large" | 450 M | 863 | 2.48 MB | 1.52–1.58 | 131–136 k | — |
| N=1 "worst leaf count" | 450 M | 17,307 | 1.73 MB | 1.81–1.84 | 31–32 k | — |

Tx wire size `72 + 28N` B. Execution ≈ 1 µs/output; ecrecover ≈ 33 µs/tx
(rayon-parallel, default on). keccak256 throughput assumed **300 MB/s/core** and
a 64-byte keccak at **0.3 µs** *(est — tiny-keccak scalar x86; measure before
committing to any Phase 2/3 number)*.

---

## 1. What is already free, and what a light client must download today

### 1.1 The free chain of bindings

```
BFT certificate (2f+1 validator sigs over value_id = EVM block hash)
  └─ EVM header RLP  (~600 B)  — keccak = the certified block hash
       └─ header.prev_randao = lean commitment C_h          [v0.2 binding]
            └─ C_h = keccak( parent32 ‖ number u64LE ‖ timestamp_ms u64LE ‖ txs_hash32 )
                 └─ txs_hash = keccak( wire[48..] )  — the whole framed tx section
                      └─ framed bytes: [n u32]([len u32][tx])*
```

Three consequences that change what Phase 1 can do with no protocol change:

1. **`txs_hash` costs 80 bytes to verify, not 1.65 MB.** The commitment preimage
   is `parent ‖ number ‖ timestamp ‖ txs_hash` = 80 B. A client holding the
   certified EVM header gets a *trustworthy `txs_hash`* from an 80-byte object.
   Only descending from `txs_hash` to an individual tx needs the bulk.
2. **The lean chain is a hash chain.** `parent` is in the preimage, so one
   certified header at `h` transitively authenticates every lean block below it
   given the 80-byte preimages in between — ~152 B/s, ~13 MB/day *(est)*, cheap
   to mirror in a client.
3. **Framing is bound.** `txs_hash` covers the count and every length
   (`chain.rs` v2 commitment), so tx boundaries cannot be re-cut.

### 1.2 What is NOT free

- **No inner structure.** `txs_hash` is a *linear* keccak over the tx section:
  no path to tx `i`, membership requires every byte.
- **Nothing about state.** No root, anywhere, ever.
- **Nothing about execution.** Total-STF means an invalid tx inside a certified
  block is a silent no-op (`exec.rs`). Inclusion does **not** imply credit.

### 1.3 Download per proof type, today

| proof | what must be fetched | bytes | client CPU | verdict |
|---|---|---|---|---|
| (a) tx/payment inclusion at `h` | certificate + EVM header + **whole lean block** | ~1.65–2.48 MB | 1 keccak pass ≈ 5.5–8.3 ms *(est)* | works, but 400–1300× more bytes than it should be |
| (b) balance/nonce at `h` | every block from genesis, replayed | ~271 GB/day of block bytes at the 300 M point *(est: 1.65 MB × 1.9 × 86,400)* | full node | **impossible in practice** |
| (c) "executed and credited" | block `h` **plus the full state at `h−1`** | as (b) | full node | **impossible** |

So (a) is expensive-but-sound; (b) and (c) do not exist.

---

## 2. Option A — an attested indexer ("harness"), no protocol change

A service that subscribes to certified lean blocks, replays them, and serves
proofs. It is *not* trusted for what the certificate already covers.

### 2.1 What it can and cannot lie about

**Cannot lie** (client re-derives locally): block bytes for a height — the
client recomputes `keccak(wire[48..])` and `C_h` and checks `C_h` against a
certified header; tx inclusion and index (same check); block ordering and the
lean parent chain; anything derivable from certified bytes.

**Can lie** (nothing on-chain contradicts it): a balance or nonce at a height;
whether a tx was applied or no-op'd; its own signed state roots; and **by
omission** — refusing to serve, or serving a stale height as current.

### 2.2 Containment

- **M-of-N across independent operators.** The attestation is 32 bytes of root;
  a client requires signatures from M distinct operators (exchange, wallet
  vendor, foundation, auditor). Disagreement is loud and immediate.
- **Anyone can audit by replay.** The inputs are certified and public; replay is
  deterministic (`exec.rs` total STF, documented same-block-credit rule). A full
  audit at the 300 M point costs ~271 GB/day of I/O, 0.1 core of execution and
  0.04 core of ecrecover *(est)* — a laptop-day per chain-day.
- **Divergence evidence is small and objective.** A dispute reduces to one
  triple `(h, A, balance)` plus each harness's 1 KB Merkle proof against its own
  signed root; replay decides. This is *fraud evidence*, not a fraud proof —
  no on-chain adjudication and no bond in Phase 1. Say so out loud to users.

### 2.3 What it signs

```
Attestation = sign_harness(
    domain          32 B   keccak("ARC_LEAN_HARNESS_V1" ‖ chain_id)
    height          8 B
    lean_commitment 32 B   -- ties the attestation to certified bytes
    state_root      32 B   -- SMT root over the account map after h
    applied_root    32 B   -- root over the per-block applied/no-op bitmaps
    tree_version    4 B    -- SMT parameterisation, for upgrades
)                          -- 65 B secp256k1 sig
```

~205 B per checkpoint. At K=64 blocks (≈34 s) that is ~0.5 GB/year from all
operators combined *(est)* — permanently archivable.

### 2.4 Bootstrap (the honest hard part)

The client must know the Arc **validator set**. Same weak-subjectivity problem
every PoS light client has: ship a hardcoded `(height, valset)` checkpoint,
follow governance valset updates through certified EVM headers from there,
re-checkpoint on client updates. Nothing about the lean lane makes this harder
or easier — it rides entirely on the EVM lane's certificate chain. **Open:**
Arc's certificate encoding and valset-update path must be pinned down before an
API is frozen; not examined here.

### 2.5 API sketch

```
lean_getTxProof(txHash | {height,index})
  -> {evmHeaderRlp, certificate, leanHeaderPreimage(80B), merklePath[]?, index, txBytes}
                                                 # merklePath only after Phase 2
lean_getAccountProof(address, height)
  -> {account{nonce,balance}, smtPath[], smtBitmap,
      checkpoint{height, stateRoot, leanCommitment}, attestations[]}
lean_getPaymentReceipt(txHash)
  -> {height, index, applied, outputs[{to,amount}], appliedProof, attestations[]}
lean_getCheckpoint(height|latest) -> Attestation
lean_getBlockBytes(commitment)    -> raw bytes  # so a client can self-verify
lean_getLeanHeaders(from, to)     -> 80 B × n   # the cheap hash chain of §1.1
```

### 2.6 Can one machine keep up at 100 k payments/s?

| component | rate | cost *(est)* |
|---|---|---|
| ingest | 3.1 MB/s | trivial network, 271 GB/day storage |
| ecrecover | 1,092 tx/s (N=100) / 31 k tx/s (N=1) | 0.04 / 1.0 core |
| execution | 107 k outputs/s | 0.11 core |
| tx-hash index | 1,092 entries/s × 48 B | 4.5 GB/day |
| SMT maintenance | see below | 0.6–1.5 cores |
| state, 10^8 accounts | flat map + SMT node store | ~9 GB RSS + ~11 GB NVMe |

**SMT arithmetic** *(est)*. Sparse Merkle tree over `keccak(address)`,
empty-subtree compressed, so effective depth ≈ `log2(n_accounts)` ≈ **27** at
10^8 accounts. Per block ~57.5 k accounts are touched (one credit per payment
plus 575 sender debits). A batch shares its upper levels, so distinct dirty
nodes ≈ `57.5k × (27 − log2 57.5k) + 2 × 57.5k ≈ 765 k`/block — 0.23 s of
hashing per 0.53 s block, and random node loads from an 11 GB store (~1 µs each)
push it to ~0.8 s. **Per-block roots do not fit on one machine.**

**So batch.** At K=64 (~34 s) ~3.5 M updates collapse to ~2.5 M distinct
accounts and `2.5M × (27 − 21.3) + 5M ≈ 19 M` dirty nodes per checkpoint =
560 k nodes/s ≈ **0.6–1.5 cores** including I/O. Comfortable.

**Alternative for small state.** A sorted-keccak accumulator (sort all accounts
by `keccak(addr)`, build a plain binary tree) is a full rebuild at
**~0.6 s per 10^6 accounts** *(est)*: 6 s at 10^7 (fine at K=64), 60 s at 10^8
(> 34 s, SMT wins). **Crossover ≈ 3–5 × 10^7 accounts.** Build the accumulator
first — a day of work, no persistent node store, trivially auditable — and swap
to the SMT when state actually crosses ~10^7.

**Verdict:** one 16-core / 64–128 GB / NVMe machine keeps up. The binding
constraint is **block-byte retention (95 TB/year raw, and fan-out payloads are
addresses — incompressible)**, not CPU. Plan tiering: hot window of raw blocks
for inclusion proofs, cold archive, and the 80 B/height header chain kept
forever.

---

## 3. Option B — put a cheap commitment in the lean block

The point of B is to make the third party *verifiable* rather than trusted.

### 3.1 B1 — a Merkle root over the framed txs

Two shapes:

- **B1a, additive:** keep `txs_hash`, add `txs_root` to the commitment preimage.
  Wire format unchanged — but validators recompute the commitment at vote time
  (structural check), so this adds a **second** full pass: +5.5 ms at 1.65 MB,
  +8.3 ms at 2.48 MB *(est)*, on the critical path. Reject.
- **B1b, replacement (recommended):** make `txs_hash` *be* the Merkle root.
  `leaf_i = keccak(0x00 ‖ len_i u32LE ‖ tx_i)`, internal `keccak(0x01 ‖ L ‖ R)`,
  odd level duplicates the last node, leaf count bound by prefixing the domain
  with `n_txs` (so a padded tree cannot be re-cut). The commitment preimage stays
  80 B; the wire format and the CL's `scan_wire` are **untouched**.

**B1b cost on the vote path** *(est)*:

| point | today (linear keccak) | B1b serial | B1b, rayon ×8 |
|---|---|---|---|
| 575 leaves, 1.65 MB | 5.5 ms | 5.5 ms leaves + 0.17 ms nodes = **5.7 ms** (+3 %) | ~1.0 ms |
| 863 leaves, 2.48 MB | 8.3 ms | **8.6 ms** (+3 %) | ~1.4 ms |
| 17,307 leaves, 1.73 MB | 5.8 ms | 5.2 ms leaves + 5.2 ms nodes = **10.4 ms** (+80 %) | ~1.5 ms |

The worst case (N=1) is +4.6 ms against ~170 ms of slack ≈ 2.7 % of the period,
and B1b is **parallelisable where the linear keccak is not**, so on a multicore
validator it is likely a net *win*. Both arms want a fleet A/B before anyone
claims that.

**What it unlocks:** inclusion proofs drop from a whole block to a path.

| point | today | with B1b |
|---|---|---|
| N=100, 300 M | 1.65 MB | 10 × 32 B path + 2,872 B tx + 80 B header + ~600 B EVM header ≈ **3.9 KB** (+cert) — **420×** |
| N=1, 450 M | 1.73 MB | 15 × 32 B + 100 B + 80 B + 600 B ≈ **1.3 KB** (+cert) — **1,300×** |

Certificate size is valset-dependent and not estimated here (4 validators today;
at 100 validators × 65 B ≈ 8 KB would dominate a 1.3 KB proof — **open**, and a
reason to care about aggregate signatures eventually).

**Consensus implications.** The commitment formula changes ⇒ **coordinated
rollout from height 1 of a fresh chain**, the rule already in force for the lane
flag and for v0.2. Total-STF is untouched: a header-hash change, not an
execution-rule change. And a *wrong* root cannot happen — the root is recomputed
from bytes, never trusted from the wire, exactly like `txs_hash` today.

### 3.2 B2 — a delayed state root, `state_root(h−K)` carried in block `h`

The proposer of `h` has K blocks (≈34 s at K=64) of wall clock to compute the
root of `h−K`, entirely off the critical path.

**Is it consensus-enforced or merely evidence?** The crux, and the answer is
nicer than it first looks:

- If validators do **not** check it at vote time (today's structural-only
  voting), a wrong root gets certified. It is then *attributable evidence*
  against a known proposer — detectable by every honest node within one block —
  but **not** a consensus violation, and clients still fall back to M-of-N.
  Barely better than Option A.
- If validators **do** check it, the check is **free at vote time**: every
  validator anchored and executed `h−K` K blocks ago and can have cached
  `root(h−K)`, so vote time is a 32-byte comparison. The *cost* is maintaining
  the tree, in the K-block gap, off the critical path. A mismatch is a
  structural rejection in the same class as a bad parent linkage, so the STF
  stays total.

Take the second. Two conditions make it safe:

1. **A lagging validator abstains, never votes Invalid.** The rule already
   exists (`lean-lane-integration.md`: "catch-up budget → per-peer slices, lag
   abstains instead of voting Invalid"). A validator that has not executed
   `h−K` has no opinion on the root.
2. **K generous.** Execution is 7–15 % of a height; K=64 (~34 s) leaves a
   ~7× margin over the worst observed anchor. K is a consensus parameter and
   Arc changes those **live via governance with no restarts** (CLAUDE.md §5).

**Cost to validators** *(est, §2.6 arithmetic)*: 0.6–1.5 cores on a background
thread, +9 GB RSS for the flat map at 10^8 accounts, +11 GB of NVMe node store.
**This is the one proposal that adds a standing resource requirement to
validators — the reason B2 is Phase 3 and not Phase 2.** New liveness failure
mode: a validator whose tree falls behind abstains and stops contributing votes
(a quarter of voting power per straggler at four validators). Needs a
health-gate metric — tree lag in heights — beside the parked-CL checks
(CLAUDE.md §6).

### 3.3 B3 — a per-block delta root (considered, parked)

Merkle root over the sorted `(address, new_nonce, new_balance)` touched in the
block: 57.5 k leaves ⇒ ~34 ms/block *(est)*, off the critical path but not free,
and it proves balance *changes* only — a client still needs an index to find the
last height that touched `A`, i.e. it still needs the harness. Strictly weaker
than B2 for the same order of cost.

---

## 4. Option C — receipts under total-STF

### 4.1 What a receipt even is here

`exec.rs` already builds `LeanReceipt { tx_hash, block_number, index, sender,
applied, logs }`, kept in a 200 k in-memory ring served for a recent window
only. But `logs` is a pure function of the tx bytes and `applied` — one
`TransferLog` per **non-zero** output, nothing when `applied == false`. So:

> **In this lane the complete receipt is one bit.** `included at (h,i)` (Option
> A or B1b) ∧ `applied` ⇒ the full log set reconstructs from tx bytes the client
> already holds.

That is the whole of Option C.

### 4.2 The cheapest commitment to that bit

No-ops are rare — after the `max_account_slots` 256→4096 fix, pool rejections
run at ~1 in 2.1 M (CLAUDE.md §5). So a bitmap is the wrong default encoding:

```
applied_section =
   [u8 mode]
   mode 0x00: nothing follows                 -- all applied (the common case)
   mode 0x01: [u32 n] ([u32 index])*          -- exception list
   mode 0x02: [ceil(n_txs/8) bytes]           -- bitmap; used when n_noops > n_txs/2
```

| case | bytes |
|---|---|
| all applied (expected, ~every block) | **1 B** |
| a handful of no-ops | 5 + 4×k B |
| adversarial all-invalid, 575 txs | 72 B (mode 2) |
| adversarial all-invalid, 17,307 txs | 2,164 B (mode 2) |

CPU: zero — the executor already produces `applied` per tx.

### 4.3 Where it lives

Same delayed trick as B2: block `h` carries `applied_section(h−K)`. Delayed
because the *proposer* has executed `h` (it builds by executing) but a
*validator* has not — `applied(h)` in block `h` would be either unverifiable at
vote time or execution on the vote path. With `h−K` the check compares against
something every validator already computed.

### 4.4 Do the CL and EL need to know?

**No, with one small exception.** The CL treats the lean payload as opaque
except for framing — it walks the length table and rejects trailing bytes
(`chain.rs::scan_wire`). Appending a section trips that check. So Option C costs
lean-lane the format + validation + delayed-check plumbing (the real work), the
CL ~10 lines in `scan_wire` plus a wire-version bump, and reth **nothing** (the
EVM lane never sees lean bytes).

If C ships with B1b, the commitment preimage must cover the new section or a
proposer could vary it freely under a fixed commitment:
`commitment = keccak(parent ‖ number ‖ ts ‖ txs_root ‖ applied_digest)` — still
fixed-size, still recomputed from bytes, never trusted from the wire.

---

## 5. Recommendation and phased plan

**Phase 1 now, Phase 2 with the next coordinated chain restart, Phase 3 only on
demonstrated demand.** Phases 1 and 2 leave the per-height critical path
untouched (Phase 2 arguably makes it faster); Phase 3 is the only one that
charges validators a standing resource rent, and should be paid only if
exchanges/auditors actually refuse M-of-N attestation.

### Phase 1 — harness, no protocol change · ~2–3 weeks *(est)*

**Ships:** ingest-by-commitment from any node; replay (reuse `lean-lane-node` as
a library); tx-hash index; sorted-keccak accumulator, checkpoint every K=64;
attestation signing; the §2.5 API; a `lean-audit` CLI that replays certified
bytes and diffs against any harness's signed roots. (ingest+replay 3 d,
accumulator 3 d, API 3 d, attestation + audit CLI 5 d, ops 3 d.)
**Unlocks:** (a) sound today at 1.65 MB/proof; (b) and (c) at M-of-N trust.
**Risk:** low — nothing consensus-facing. The honest risk is *presentational*:
users must be told (b) and (c) are attested, not proven.

### Phase 2 — `txs_hash` becomes a Merkle root (B1b) + applied section (C) · ~1 week lean-lane + ~2 d CL + a fleet gate *(est)*

**Ships:** B1b leaf/node domains; the mode-tagged applied section at `h−K`;
`commitment = keccak(parent ‖ number ‖ ts ‖ txs_root ‖ applied_digest)`; CL
`scan_wire` tolerance; harness serves real paths.
**Unlocks:** inclusion proofs at **1.3–3.9 KB** (420–1300× smaller) and the
receipt bit consensus-checked instead of attested — wallets get a
trust-minimised "your payment landed".
**Risk:** medium — commitment + wire bump ⇒ **fresh chain from height 1, all
validators together** (CLAUDE.md §5). Gate on a fleet A/B showing the vote-path
delta is inside run-to-run noise at the N=1 / 450 M worst case (est +4.6 ms).

### Phase 3 — delayed state root (B2), validator-checked · ~4–6 weeks *(est)*

**Ships:** SMT in the lean node on a background thread; `state_root(h−K)` in the
block; vote-time comparison against the cached local root; abstain-on-lag;
tree-lag metric in the health gate.
**Unlocks:** (b) with **no trusted party at all** — a 1 KB account proof against
a root inside a BFT-certified block. What an exchange or a regulator wants.
**Risk:** high — +0.6–1.5 cores and +20 GB per validator, a new abstain-driven
liveness mode, a second coordinated rollout. Do not start before Phase 1 has run
the SMT in production for a month at real state sizes.

### Kept true in every recommended phase

The per-height critical path gains **at most** the B1b delta (+3 % at the
fan-out points, +4.6 ms worst case, −80 % if parallelised) plus one 32-byte
comparison. Every tree — Merkle over accounts, per-block deltas, checkpoint
roots — is computed off-box (Phase 1) or in the K-block gap (Phase 3). Nothing
proposed here runs between receiving a proposal and voting.

## 6. Open questions

1. Certificate encoding and size, and the valset-update path — unexamined, and
   it caps how small a proof can usefully get (§3.1).
2. Is keccak really ~300 MB/s and ~0.3 µs/64 B on the fleet hardware? Every
   Phase 2 number scales off it; one `cargo bench` settles it.
3. Production account cardinality and the recipient-repeat rate — both move the
   SMT batching arithmetic and the accumulator/SMT crossover (§2.6).
4. Should the harness attest a *flat-state digest* (`FlatState::digest`, already
   in `state.rs`) in Phase 1 before the accumulator exists? One line, proves
   nothing per-account, but makes cross-operator divergence detectable on day 1.
5. Block-byte retention policy — 95 TB/year raw is the real cost driver of
   Option A, not CPU.
