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
| **jmt crate, out-of-the-box (single-threaded)** | 1M accts | **65 ms/block** | 13.7 µs |
| **jmt crate, out-of-the-box** | 10M accts | *(run in progress — expect ~75-90 ms/block from depth growth)* | ~16-19 µs |
| reth MPT, live payment lane (parallel sparse-trie task, keccak) | 10M accts | **2.6–12 ms/block** | **~0.5–2.5 µs** |
| our dense in-RAM Merkle (paper App. E `merkle_acc`) | 25M leaves | — | ~11 µs |

**Honest headline: the naive JMT is ~6× SLOWER than reth's tuned MPT machinery.** The
"JMT beats MPT" intuition comes from comparing JMT against *naive* MPT implementations;
reth 2.x's parallel sparse-trie + prefix-set machinery is anything but naive. The jmt crate's
`put_value_set` is single-threaded; Aptos gets its throughput from sharding/pipelining above
the tree, and gravity-reth from a 16-way parallel rewrite — the structure alone buys nothing.

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

## Recommendation

1. **Do NOT swap the live lane's MPT for stock jmt** — it would be a measured regression at
   today's scale. reth's sparse-trie cache is the state of the art we already ship.
2. **Adopt JMT as the commitment for the FUTURE minimal payment EL** (the from-scratch lane the
   paper points to): there, simplicity + versioning + proofs outweigh raw speed, and a
   parallelized `put_value_set` (16-way by key-prefix, gravity-style, over independent subtrees)
   is a tractable optimization that should land it in the low-single-digit ms/block range.
3. **Next experiments** (cheap, high-info):
   a. parallel batch update prototype over jmt (rayon by first key nibble) — does 65 ms → <8 ms?
   b. MDBX-backed TreeReader/Writer + cold-start benchmark (the beyond-RAM regime the paper
      cares about — where reth MPT collapsed to 27.8 ms/blk on real disk state).
   c. index-keyed KeyHash for preseeded accounts (locality experiment, JMT edition).
