# Payment-lane commitment beyond RAM — an experimental search

**Branch:** `payment-commitment-beyond-ram`. **Crate:** `experiments/utxo-state/` (standalone).

## Question
Find a data structure to replace the general-purpose Merkle-Patricia Trie for a **payment-only**
state such that it **retains an advantage even when the state exceeds RAM**. Compare the **account**
and **UTXO** models, and optimize for **both** update latency (disk I/O per block) **and** proofs
(witness size + verify cost).

## Hard constraint: post-quantum, hash domain only
Every commitment must be **hash-based** (collision/preimage resistance only → PQ-safe; Grover only
halves security, so 256-bit). This **rules out** all discrete-log / pairing / lattice-vector schemes:
Verkle, KZG, IPA, Pedersen vector commitments, and multiset hashes MuHash/ECMH are **out**. Two
consequences that shape the design:
1. **Binary Merkle is ~proof-optimal in the hash domain.** Widening a hash tree makes proofs *bigger*
   (each level needs k−1 sibling hashes), so we do NOT get the Verkle "wide → tiny proof" trade. The
   lever for small proofs is **batching + locality**, not the commitment.
2. **Locality pays a second time, on proofs.** A block's *batched* Merkle multiproof shares internal
   nodes across touched leaves; logically-clustered (dense-index) leaves overlap heavily → block
   witness collapses far below K independent branches, while hash-scattered (MPT) leaves stay ≈ K full
   branches. So the Phase-1 locality lever also shrinks the witness.
Succinct-proof path that STAYS hash-based: STARK/FRI over the block's Merkle transition (PQ-safe) —
noted as a direction, not built here.

## Why the MPT loses beyond RAM
Ethereum's MPT keys by `keccak(addr)` → uniformly random. An account update walks ~log(n) nodes on
random cold pages; a block's merklization is random-I/O-bound once the working set exceeds RAM
(measured: 27.8 ms cold state-root on the 169 GB Arc state, merkle/state-root dominant). This is
structural: random keys ⇒ random disk.

## The lever unique to a payment lane
We control the key space. Assigning accounts **dense sequential indices** (account numbers) instead
of hash-derived addresses makes the authenticated structure's nodes lay out **contiguously**, so a
block's updates touch **contiguous pages** → sequential SSD I/O + effective prefetch instead of
random. Sequential is ~100–1000× cheaper than random for cold pages. `locality.rs` measured the RAM
shadow of this (~1.4×); the disk regime is where it should become large — and is exactly what the
current in-RAM benchmark cannot reach.

## Honest "beyond RAM" methodology (the crux)
Root-free, reproducible cold-I/O measurement without a literal >62 GB build every run:
- Back every structure with an **mmap'd file** (`memmap2`).
- Between measured "blocks," `madvise(MADV_DONTNEED)` the region to **evict our own pages** →
  next access is a genuine cold disk read (models state whose working set exceeds RAM).
- Account cold I/O per block via `/proc/self/io` (`read_bytes`) and major page faults
  (`getrusage.ru_majflt`); dense should show few/sequential, hash-keyed many/random.
- **Validate** the proxy against at least one real **>62 GB** build (we have 2.3 TB disk) to confirm
  the madvise proxy matches true page-cache pressure.

## The matrix
Structures × metrics, all measured at sizes **crossing the RAM boundary** (e.g. 10M → 500M+ entries).

Account model (all hash-based):
- **A0 hash-keyed MPT** — real `alloy-trie`, baseline (the thing to beat).
- **A1 locality-keyed dense Merkle** — mmap dense binary Merkle, dense account index (the primary bet).
- **A2 batched block multiproof** — same binary Merkle; measure per-tx branch AND the *batched* block
  witness (shared internal nodes) for clustered (dense) vs scattered (hash-keyed) leaves. This is the
  hash-domain proof lever (locality-aided), replacing the dropped vector-commitment variant.

~~UTXO model~~ — **dropped.** Academically interesting (conflict-free parallelism, statelessness) but
wrong for a balance system in production: balances must be reconstructed by summing unspent outputs,
every payment needs coin selection + change outputs, and each spend carries a per-coin proof. Terrible
for "what is account X's balance." The payment lane is **account-based**; the search is now solely the
ideal account commitment.

Metrics per structure:
- **Latency/I/O beyond RAM:** cold + warm state-root/update latency, **reads-per-block**,
  random-vs-sequential bytes, RSS, throughput (updates/s).
- **Proofs:** witness size per tx, proof gen + verify cost, and whether the model is stateless
  (proof-carrying) vs stateful.

## Phases
0. **Design note** (this file) — commit early.
1. **Account latency** (`disk_bench` bin): A0 vs A1 mmap, madvise-cold, `/proc/self/io` accounting,
   curve crossing RAM. First real beyond-RAM number. ← start here.
2. **Hash-domain proofs** (`multiproof` bin) — DONE. Per-tx branch + batched block witness, clustered
   (dense) vs scattered. Completes the account-model verdict (latency + proofs, both PQ).
3. **Trust the number** — real `alloy-trie` MPT baseline (the actual production thing being replaced;
   "hashed" so far is only a lower bound), AND validate the fadvise cold proxy against a real >62 GB
   natural-pressure build. ← next.
4. **Realize locality in production** (the crux): dense indices need an assignment scheme + an
   address→index directory. Open questions: (a) does the addr→index directory reintroduce random I/O?
   (it's a point lookup, not merklized — likely a cheap separate index, but measure); (b) growing set →
   an APPEND-dense tree over the populated prefix [0,N) (grows with users, stays dense) vs a fixed-depth
   full tree; (c) deletion/dust → tombstone + index reuse without fragmenting locality; (d) deterministic
   index assignment across validators. This is the gap between "dense Merkle wins when indices are given"
   and "a working payment lane."
5. **Synthesis:** one table — structure × {reads/block beyond RAM, cold root latency, per-tx proof,
   batched block witness, resident RAM} → the "ideal" pick, honestly scoped.

## Phase 1 results — account layout, cold disk I/O per block (measured 2026-07-04)
`disk_bench` (i7-11700F, 62 GB RAM, NVMe): one file-backed mmap binary Merkle over N dense accounts,
block = 500 leaf updates + path recompute, cold via flush+`fadvise(DONTNEED)`+`madvise(DONTNEED)`,
cold bytes from `/proc/self/io read_bytes`. Dense (`physical=logical`) vs hashed
(`physical=logical·golden_odd mod 2^m`) — SAME tree, SAME root (verified), only node placement differs.

| depth | accounts | node file | dense cold/blk | hashed cold/blk | **ratio** | dense lat | hashed lat |
|------:|---------:|----------:|---------------:|----------------:|----------:|----------:|-----------:|
| 22 | 4.2M  | 0.2 GiB | 19.0 MiB | 51.3 MiB   | **2.7×**  | 99 ms  | 153 ms  |
| 24 | 16.8M | 1 GiB   | 18.9 MiB | 409.5 MiB  | **21.6×** | 139 ms | 854 ms  |
| 26 | 67.1M | 4 GiB   | 22.0 MiB | 1078.7 MiB | **49×**   | 199 ms | 1982 ms |
| 28 | 268M  | 16 GiB  | 25.6 MiB | (GiB/blk; too slow to finish 10 blocks) | — | 272 ms | — |

**Headline:** dense cold reads stay ~**flat (~20 MiB/block)** as state grows 64× — a locality-keyed
block touches a *bounded, contiguous* working set independent of total state size. Hashed cold reads
grow **superlinearly** (random placement scatters each block's accesses across the whole, growing file;
readahead then wastes bandwidth on unwanted neighbours). The locality advantage doesn't just survive
beyond RAM — it **widens with scale** (2.7× → 21.6× → 49× and climbing). This is the empirical core of
the answer: the MPT's random `keccak(addr)` keying is exactly what makes it degrade beyond RAM; a
dense-index (locality-keyed) authenticated structure removes that.

**Honest caveats:** (1) `fadvise` evicts the *whole* file including hot upper levels, so absolute dense
numbers (~20 MiB) are pessimistic — a real >RAM run keeps upper levels cached, lowering dense further,
so the ratio is if anything understated. (2) This isolates node *layout* at fixed Merkle shape; a real
MPT adds KV-index I/O on top of random node placement, so "hashed" here is a **lower bound** on the
MPT's penalty. (3) Uniform-random account access; real payments have locality (`--zipf`) which helps
dense more. (4) Proxy still to be validated against a real >62 GB natural-pressure build (Phase 4).
Merkle-branch proof = depth·32 B (704–896 B here), layout-independent; shrinking it is Phase 3.

## Phase 2 results — hash-domain block witness, locality vs scattered (measured 2026-07-04)
`multiproof` (exact Merkle multiproof frontier count; PQ-safe, hashes only). Single-leaf branch =
depth hashes (960 B at depth 30), position-independent. The batched BLOCK witness, depth 30 (1.07B
accounts):

| block | leaf placement | witness | amortized/tx | vs K independent |
|------:|----------------|--------:|-------------:|-----------------:|
| 500   | contiguous (locality) | **0.9 KiB** (30 hashes) | 0.06 | 497× |
| 500   | window 4k             | 37.7 KiB | 2.42 | 12.4× |
| 500   | window 1.02M          | 158.6 KiB | 10.1 | 3.0× |
| 500   | scattered (MPT)       | 314.8 KiB | 20.1 | 1.5× |
| 5000  | contiguous            | **0.9 KiB** (30 hashes) | 0.01 | 5007× |
| 5000  | scattered (MPT)       | 2628 KiB | 16.8 | 1.8× |

**Headline:** a block touching a **contiguous** account range proves in **~depth hashes total (≈0.9 KiB),
INDEPENDENT of block size** — K contiguous leaves form one subtree, so the witness is just its path to
the root. Scattered (hash-keyed MPT) leaves need ~K·depth hashes (315 KiB → 2.6 MiB). So the *same*
locality lever from Phase 1 shrinks the block witness ~**350× (block 500) → ~2900× (block 5000)**, all
in the hash domain (post-quantum). Real payments aren't perfectly contiguous, but even a locality
*window* (accounts that transact together numbered nearby) gives 8–12×, and a payment lane can *assign*
indices to encourage clustering (registration cohort / activity). Both the latency win (Phase 1) and
the proof win (Phase 2) come from one thing: **dense-index (locality) keying of a hash binary Merkle.**

## Phase 4 — validation on a real >62 GB build (RAM = 62 GB, measured 2026-07-05)
The Phase-1 numbers used the fadvise cold proxy. Validation with a **real 128 GiB structure**
(depth 31, **2.1 billion accounts**, genuinely > RAM), block = 500:

| structure | mode | cold read/block | note |
|-----------|------|----------------:|------|
| dense | fadvise proxy | 28.45 MiB | whole file evicted each block (pessimistic) |
| dense | **natural pressure** | **3.75 MiB** | hot upper tree stays cached; only deep/leaf levels cold |
| hashed | (confirmatory, >RAM) | — | explosion already established: 409 MiB @16M, 1079 MiB @67M |

**Findings:** (1) dense stays **flat and tiny even at 2.1 B accounts** — cold reads ~22→26→28 MiB
(fadvise) across 67 M→268 M→2.1 B, and only **3.75 MiB/block under true natural memory pressure**.
(2) The fadvise proxy **overstates dense cost** (it evicts hot upper levels a real >RAM node keeps
cached), so the proxy's dense-vs-hashed ratios (2.7×→49×) are a **lower bound** on the true advantage.
The headline holds and strengthens: a dense-keyed block costs **< 4 MiB of cold I/O at 2.1 billion
accounts / 128 GiB**, while the hash-keyed layout is already reading GiB/block at 67 M. Note: building
the 128 GiB tree took ~15–21 min (one-time); a real client uses incremental writes, not a full rebuild.
### Real Ethereum MPT baseline (`mpt_baseline`, measured 2026-07-05) — corrects the "proxy" framing
Built the **actual** Ethereum MPT (alloy-trie, keccak-keyed accounts) and extracted each key's real
path nodes:

| accounts | MPT path nodes/key | MPT proof bytes/key | binary path nodes | binary proof (depth·32) |
|---------:|-------------------:|--------------------:|------------------:|------------------------:|
| 1 M   | 6.7 | 2644 B | 20 | 640 B |
| 4 M   | 7.2 | 2928 B | 22 | 704 B |
| 16 M  | 7.7 | 3172 B | 24 | 768 B |
| 64 M  | 8.2 | 3467 B | 26 | 832 B |

Two corrections to the earlier hand-wave: (1) **the real MPT single-key proof is ~4x LARGER than the
dense binary Merkle** (3467 B vs 832 B @64 M) — each 16-ary branch node carries 16 child hashes
(~512 B). So dense binary wins on proof size even single-key, over the real MPT, *before* the batched-
block locality win. (2) The same-shape "hashed" proxy is **not a strict lower bound** on the MPT: the
MPT touches *fewer, larger* nodes (~8 vs ~26), but each is a KV (MDBX B-tree) lookup — ~8 nodes x
~3-4 index pages ~= ~30 random pages/update, **comparable** to the proxy's ~26 direct random reads. So
the proxy is a fair *approximation* of the real MPT's random-access page count, not a bound either way.
What is **robust** regardless: dense = sequential/contiguous (flat ~4-28 MiB/block); *any* random-keyed
structure (real MPT or proxy) = ~30 random pages/update -> hundreds of MiB to GiB/block beyond RAM,
superlinear. Still deferred (needs a persistent node store): the MPT's exact on-disk cold-**I/O** per
block — but its two drivers (path-node count, node bytes) are now measured on the real structure.

## The cut — where the MPT actually loses (the RAM line, measured 2026-07-05)
The advantage is **not universal**; it exists only beyond RAM. Natural pressure, no forced eviction:

| state | vs 62 GB RAM | dense cold/blk | hashed cold/blk | winner |
|------:|--------------|---------------:|----------------:|--------|
| 1 GiB · 16.8M | fits easily | 0.00 MiB | 0.00 MiB | tie — no win |
| 4 GiB · 67M   | fits        | 0.00 MiB | 0.00 MiB | tie — no win |
| 128 GiB · 2.1B | 2× over    | 3.75 MiB | GiB/block (too slow to finish 3 blocks) | dense ~1000× |

Three regimes: **state ≪ RAM** → both fully cached, identical, *don't switch*; **state ≈ RAM** → the MPT
crosses first (bulkier: 16-ary/RLP/address keys ≈ 2–4× bytes/account, so it thrashes at a few hundred M
accounts here while the lean dense tree holds to ~1B); **state ≫ RAM** → MPT random-disk-bound & climbing,
dense flat. A real payment ledger (Ethereum alone ≈ 250M accounts / ~1 TB today) lives permanently in the
third regime — the cut is the operating point, not a corner case. Why balances specifically: payment work
is O(1) (add/subtract two numbers), so ~100% of per-tx cost is the state-root update, and balance state
only grows with users → guaranteed to cross RAM. For contracts the MPT tax hides behind execution
(Arc ERC20: exec ~4%, state-root ~58%, persistence ~38%); for balances execution is ~0%, nothing to hide.

## Interim verdict (account model, both dimensions, PQ)
The "ideal" hash-domain structure for a payment lane is a **locality-keyed dense binary Merkle** (≈ NOMT):
- Latency beyond RAM: cold reads flat ~20 MiB/block as state grows (vs MPT superlinear, 49× @67M).
- Proofs: single-key branch already ~4× smaller than the real MPT (832 B vs 3467 B @64M, measured);
  contiguous block witness ~depth hashes (0.9 KiB) vs MPT ~K·depth, ~350–2900×.
- Binary is ~proof-optimal in the hash domain; no PQ compromise (no vector commitments).
DONE: real MPT baseline (measured) + >62 GB validation + production locality analysis. Robust core:
random-keyed (MPT or proxy) → superlinear beyond RAM; dense → flat. Still deferred (needs a persistent
node store): the MPT's exact on-disk cold-I/O/block (its drivers are now measured). UTXO dropped (wrong
for balances); vector commitments dropped (not PQ).

## Realizing locality in production (the crux — analysis + measured operating points)
The benchmark win assumes state is keyed by a **dense account index** and that a block's touched
indices **cluster**. Two production questions decide whether that holds.

**(1) The address→index directory — does it reintroduce random I/O? No, if txs reference accounts by
index.** The clean design: a tx names accounts by their dense **index** (an account *number*), not their
address. The index is assigned deterministically at account creation (e.g. `(block_height, position)` →
next free index) and is known to the owner, like a bank account number or a Solana account key passed in
the tx. The node then verifies the Merkle proof **at that index directly** — it never does an
address→index lookup during block processing. The address→index map becomes a **client-side / locally
rebuildable convenience index**, not consensus state and not on the node's hot path.
- If a directory *is* needed on-node (pay-by-address UX): it is a **point lookup** (~1 random read/
  lookup, 2/tx), O(1) not O(log n), a plain KV separable from the state tree. Additive cost ~2 reads/tx
  ≪ the merklization saving (dense ~20 MiB vs MPT ~1 GiB per 500-tx block). Net win survives. Note the
  MPT avoids a directory only by keying directly on `keccak(addr)` — which is *exactly* what forces its
  random-I/O merklization. So the real trade is: MPT skips a directory but pays random merklization;
  dense pays a cheap (or zero, via reference-by-index) directory and gets sequential merklization.

**(2) Earning activity-locality — index assignment is a first-class lever, not free.** Real payments
cluster by *activity*, not creation time, so the win is only as good as the index assignment. Measured
proof witness vs the activity-clustering window (depth 30, 1.07 B accounts, block 500; the realistic
operating range between best-case contiguous and MPT-scattered):

| activity window | block witness | amortized/tx | vs MPT-scattered (315 KiB) |
|----------------:|--------------:|-------------:|---------------------------:|
| ~2k (tight cohort)   | 24 KiB  | 1.6  | ~13× smaller |
| ~8k                  | 52 KiB  | 3.3  | ~6× |
| ~32k                 | 81 KiB  | 5.2  | ~4× |
| ~128k                | 112 KiB | 7.2  | ~2.8× |
| ~2M (loose)          | 174 KiB | 11.1 | ~1.8× |
| random (= MPT)       | 315 KiB | 20.2 | 1× (no win) |

So the honest production claim is **not** the 350×/49× best case — it is: *if index assignment keeps
transacting accounts within a window of a few thousand, you get ~10–20× on proofs and a proportional
I/O win; a loose window still gives ~3×; fully random assignment gives nothing (you are back to the
MPT).* Assignment strategies that earn tight windows: **temporal cohorts** (users onboarded together —
a merchant's customers, an exchange's users — get contiguous indices and transact intra-cohort);
**hierarchical/prefix** indices (`domain ‖ local`, e.g. per merchant / rollup / region) so intra-domain
payments are windowed. Adaptive re-indexing by the observed activity graph is possible but rehashes
paths — likely not worth it.

**(3) Growth & deletion.** Append-dense: new accounts take the next free index → the tree is dense over
the populated prefix `[0,N)`, grows with users, no wasted empty subtrees; depth = ⌈log₂ N⌉; balances
update in place. Dust/close: tombstone + free-list reuse (reusing a low freed index keeps density;
minor cohort-contiguity churn, acceptable).

## Non-negotiables (repo methodology)
Measured, reproducible, **no fabricated numbers**; state ranges/variance; every claim reruns from the
crate README. Reference SOTA so we don't reinvent: **NOMT** (page-optimized binary Merkle trie, 2024),
**QMDB** (append + in-RAM twig Merkle, 2025), **JMT** (Aptos, versioned SMT) — all hash-based, in
scope. **Verkle** (vector commitments) is a conceptual reference only — **excluded on PQ grounds**.
Hash choice: keccak256 here; a payment lane could pick a faster PQ hash (BLAKE3) — orthogonal to the
structure. Signature verification (k256, ~2× slower than libsecp256k1) is a separate PQ question
(Falcon/Dilithium/SPHINCS+) — out of scope for the commitment search; note it, don't hide it.
