# Payment-lane commitment beyond RAM — an experimental search

**Branch:** `payment-commitment-beyond-ram`. **Crate:** `experiments/utxo-state/` (standalone).

## Question
Find a data structure to replace the general-purpose Merkle-Patricia Trie for a **payment-only**
state such that it **retains an advantage even when the state exceeds RAM**. Compare the **account**
and **UTXO** models, and optimize for **both** update latency (disk I/O per block) **and** proofs
(witness size + verify cost).

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

Account model:
- **A0 hash-keyed MPT** — real `alloy-trie`, baseline (the thing to beat).
- **A1 locality-keyed dense Merkle** — mmap dense binary Merkle, dense account index (the primary bet).
- **A2 locality-keyed + vector commitment** — wide node, curve25519 (Pedersen/IPA-style) for the
  *proof* dimension (small witness), same dense layout.

UTXO model:
- **U0 UTXO set + hash accumulator** — Utreexo-style forest (proof-carrying, ~O(log n) resident).
- **U1 UTXO set + multiset hash** — MuHash/ECMH-style O(1) commitment (curve25519), for comparison.

Metrics per structure:
- **Latency/I/O beyond RAM:** cold + warm state-root/update latency, **reads-per-block**,
  random-vs-sequential bytes, RSS, throughput (updates/s).
- **Proofs:** witness size per tx, proof gen + verify cost, and whether the model is stateless
  (proof-carrying) vs stateful.

## Phases
0. **Design note** (this file) — commit early.
1. **Account latency** (`disk_bench` bin): A0 vs A1 mmap, madvise-cold, `/proc/self/io` accounting,
   curve crossing RAM. First real beyond-RAM number. ← start here.
2. **UTXO accumulator** (`utxo_accum` bin): U0/U1, proof size + accumulator update beyond RAM.
3. **Proof dimension** (`vector_commit` bin): A2 vs A1 witness size / verify; U1 O(1) commitment.
4. **Synthesis:** one table — structure × {reads/block beyond RAM, cold root latency, proof size,
   verify cost, resident RAM} → the "ideal" pick, honestly scoped.

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

## Non-negotiables (repo methodology)
Measured, reproducible, **no fabricated numbers**; state ranges/variance; every claim reruns from the
crate README. Reference SOTA so we don't reinvent: **NOMT** (page-optimized binary Merkle trie, 2024),
**QMDB** (append + in-RAM twig Merkle, 2025), **JMT** (Aptos, versioned SMT), **Verkle** (vector
commitments). k256/curve25519 pure-Rust is ~2× slower than libsecp256k1 — note it, don't hide it.
