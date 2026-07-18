# Jellyfish Merkle Tree as the payment-lane state commitment — feasibility study

*2026-07-16, branch `gravity-payment-lane`. This is the paper's central research question
("the commitment for the lean EVM state") made concrete against real candidates.*

## First: disambiguate the name (it matters)

- **EspressoSystems/jellyfish** (`~/jellyfish`) is a PLONK/SNARK crypto suite. Its
  `jf-merkle-tree::UniversalMerkleTree` is a sparse tree for CIRCUIT-friendly use: in-memory
  only (`Arc<MerkleNode>` pointers, no KV node-store abstraction), single-key updates only
  (no batching; `from_kv_set` loops), arity-3 with ~160-level paths for address-sized keys,
  algebraic hashes first-class (Rescue/Poseidon; keccak exists but its leaf hash does not bind
  the key — a soundness footgun), arkworks-heavy deps. **Not a state-commitment engine.**
  Verdict: unsuitable for an EL; relevant only if we ever want SNARK-provable state.
- **The actual "Jellyfish Merkle Tree" (Diem/Aptos paper) is the Penumbra `jmt` crate**
  (`~/jmt`, v0.12): versioned sparse merkle over hashed keys, **batched per-block updates**
  (`put_value_set(version, writes) -> (RootHash, TreeUpdateBatch)`), pluggable node storage
  (`TreeReader`/`TreeWriter` traits — MDBX/RocksDB adapters are straightforward), pluggable
  `SimpleHasher` (keccak = a 10-line impl), ics23 proofs, no arkworks. This is the real candidate.

## Why replacing MPT is even legal here

The payment lane's `stateRoot` is OURS: the CL only requires all payment ELs to compute the
same header; nothing checks Ethereum MPT semantics cross-chain. So the lane can adopt any
deterministic commitment without touching the EVM lane or consensus code — the change is
confined to the payment EL's root computation + node persistence + genesis init
(the exact three sites we mapped during the gravity-reth post-mortem:
provider `state_root*`, the engine-tree root path, trie-updates persistence).

## Measured reality check (this machine, i7-11700F, in-RAM node store)

New harness: `experiments/utxo-state/src/bin/jmt_bench.rs` (Penumbra jmt 0.12, sha256,
account records ~72B, preseed-style addresses, pool-walk update pattern — mirrors the live lane's
workload).

| structure | scale | 4,761 updates/block | per-update |
|---|---|---|---|
| jmt out-of-the-box (single-threaded) | 1M accts | 65 ms/block | 13.7 µs |
| jmt out-of-the-box (single-threaded) | 10M accts | **80.9 ms/block** | 17.0 µs |
| **jmt SHARDED x16 forest (rayon, root=keccak(16 shard roots))** | 10M accts | **10.84 ms/block (p95 12.9)** | **2.3 µs** |
| reth MPT, live payment lane (parallel sparse-trie task, keccak) | 10M accts | 2.6–12 ms/block | ~0.5–2.5 µs |
| our dense in-RAM Merkle (paper App. E `merkle_acc`) | 25M leaves | — | ~11 µs |

**Honest headline, two parts:** (1) naive JMT is ~7× SLOWER than reth's tuned MPT machinery —
the "JMT beats MPT" intuition only holds against naive MPTs. (2) **A 16-way sharded JMT forest
(each shard an independent JMT by first key-nibble; lane root = keccak of the 16 shard roots)
reaches 10.84 ms/block — PARITY with reth's parallel sparse-trie on identical hardware and
workload** — implemented in ~80 lines (`jmt_bench --sharded`). Sharding is embarrassingly
deterministic and proof-friendly (shard proof + 16-root preimage).
(Context: Aptos gets its production throughput from sharding/pipelining above the tree, and
gravity-reth from a 16-way parallel rewrite — the structure alone buys nothing; parallelism does.)

## The lean-state engine — IMPLEMENTED AND TESTED (`lean_state`)

`experiments/utxo-state/src/bin/lean_state.rs` (~350 loc): the paper's unified store+commitment,
disk-backed. 16 shards, one redb file each, holding BOTH the JMT nodes and the versioned account
records (the JMT value store IS the account store — no PlainState/trie split). Pipeline per block:
parallel secp256k1 verify -> deterministic execution -> 16-way parallel versioned JMT update with
DURABLE commits -> lane root = keccak(16 shard roots).

Self-tests (`--selftest`, passing): cross-instance root determinism (the consensus property),
root progression, balance conservation, historical-version queries.

Full-pipeline benchmark @10M accounts on disk, 4,761 signed transfers/block, 30 blocks
(shared box, alongside other load — p95 reflects disk contention):

| phase | ms/block | notes |
|---|---|---|
| sig-verify (parallel k256) | 44.5 | 9.3 µs/tx; libsecp256k1 would halve this |
| execute (balance/nonce) | 25.6 | redb reads; RAM-cache would cut most of it |
| commit+persist (JMT x16 + fsync) | 191-311 | run-to-run disk variance; reth's persist alone = 211.8 measured |
| **TOTAL (fully durable)** | **~380 (p50 231)** | **80 µs/tx end-to-end ≈ 12.5k TPS single node** |

Context: the live lane's equivalent phases (reth, same blocks) ≈ exec 64.4 + root 12 + persist
211.8 ≈ 288 ms — the ~350-line prototype lands in the SAME class as tuned reth on its first
outing, while adding native versioning and one-table storage. Build: 10M accounts to disk in 66 s.
Known gaps: 17 GB on-disk (versioned-node accumulation — needs stale-node GC / larger build
batches vs reth's 0.7 GB), no RAM cache on the read path, fsync-per-shard could batch.

## What JMT DOES buy (and what it costs)

Wins (structural, real):
- **Versioned roots natively** — query/prove state at any height without archive gymnastics;
  stale-node GC by version. (reth needs changesets + unwinds for this.)
- **Radical simplicity** — one node type, no RLP branch/extension zoo, KV-native storage:
  a from-scratch lane EL (the paper's endgame) is far easier to build correctly around JMT.
- **Cheap succinct proofs incl. non-membership** (ics23 ecosystem) — relevant for the
  agent-registry/channel direction (docs/agent-payment-lane.md) and light clients.
Costs:
- Raw single-threaded update speed loses to reth's parallel MPT today (numbers above).
- Hash-scattered keys (sha/keccak of address) — same cache-locality profile as MPT; our
  `locality` experiment's dense-keying win (~1.4x in RAM, more on disk) applies to NEITHER
  unless we key by account index, which JMT would actually permit (KeyHash is opaque 32B —
  we could use index-derived keys for the preseed range and hash-keys for organic accounts...
  research knob).
- Integration into reth-as-is means bypassing the entire trie stack while keeping MDBX
  (TreeReader/Writer over a new table) — a focused but consensus-critical patch to our fork,
  same blast-radius class as the gravity nested-trie experience.

## Recommendation — FOR THE PAYMENT LANE (per project direction)

1. Stock jmt swap on the live lane: NO (measured regression). **Sharded-x16 JMT forest: YES,
   viable** — parity speed today (10.84 vs 2.6-12 ms) with three structural wins reth MPT
   lacks: native versioned roots (historical state without changesets/unwinds), KV-native node
   storage (trivial MDBX table; no trie-table zoo — recall the gravity failure mode), and
   ics23 proofs incl. non-membership (feeds the agent-registry/channel direction).
2. Integration path into the payment EL (our fork): new node flag `--arc.payment-root=jmt16`;
   swap the three root sites (provider state_root*, engine-tree root path, trie-updates persist)
   for the sharded forest; JMT node batches into one new MDBX table; genesis init writes the
   10M preseed via 16 parallel put_value_sets (measured build: minutes). Consensus-legal because
   the lane's root only needs cross-validator determinism. Differential harness exists.
3. **Next experiments** (in order):
   a. [DONE — 10.84 ms] sharded parallel forest prototype.
   b. MDBX-backed TreeReader/Writer + cold-start benchmark — the beyond-RAM regime where reth
      MPT collapsed to 27.8 ms/blk on real disk state; JMT's sequential-version node layout
      should shine exactly there. THIS is the decisive experiment.
   c. index-keyed KeyHash for preseeded accounts (locality win, JMT edition).
   d. combine with grevm (block-stm-for-arc.md): parallel exec + sharded root =
      the full parallel payment-lane pipeline.
