# reth 2.3 with JMT state root — payment lane

The payment EL computes its `stateRoot` as a **16-shard Jellyfish-Merkle-Tree root** (Diem JMT via
Penumbra `jmt`, redb-persisted) instead of reth's MPT, when launched with the flags below.
Implemented by vendoring reth's `reth-trie-db` (via `[patch]`) and injecting the JMT branch into
`overlay_root_with_updates` (vendor/reth-trie-db/src/jmt_root.rs); MPT path unchanged when off.

## Enable
    ARC_PAYMENT_ROOT=jmt ARC_JMT_STORE_PATH=<dir> arc-node-execution node ... \
        --engine.state-root-fallback --engine.disable-parallel-sparse-trie
The two engine flags force reth onto the synchronous provider root path (which we patched) instead
of the parallel sparse-trie task (which we did not) — required so build AND validate both use JMT.

## Verified (experiments/dual-el/jmt/smoke.sh)
Single node, Engine API (fcU+attrs -> getPayloadV4 -> newPayloadV4), empty block on localdev genesis:
- MPT run:  emptyBlockRoot 0x9e891651...  validate=VALID
- JMT run:  emptyBlockRoot 0xee793ec0...  validate=VALID   (root differs from MPT => JMT path fired)
- build == validate (newPayloadV4 accepted the JMT-rooted block: proposer and validator agree)
- DETERMINISTIC: identical JMT root across independent fresh runs (the consensus property)
=> reth 2.3 PRODUCES and SELF-VALIDATES blocks with JMT state roots.

## Scope / follow-ups (honest)
- First-cut store: process-global redb JMT at ARC_JMT_STORE_PATH, version++ per root call. Fine for
  a single proposer; multi-validator + re-org idempotency (same block -> same version) needs the
  version keyed by block number/parent, and stale-node GC (the 134GB-at-100M finding).
- Only the sync provider root path is patched; the parallel sparse-trie task is bypassed via flags.
  A production build would also patch/replace that path (or make JMT the parallel task).
- Storage tries unsupported (payment accounts have none) — accounts-only commitment.

## Live 2h head-to-head attempt (2026-07-18) — MPT ran, JMT stalled at block 2 (correctness, not perf)
Bare-metal harness (headtohead.sh): two host ELs, identical mock-CL + transfer load, only ARC_PAYMENT_ROOT differs.
- MPT lane: 334 blocks in ~90s, avg state_root 1.55 ms (live reth, small empty-chain state, sync overlay path).
- JMT lane: stalled at block 1. Root cause (measured): reth's payload builder calls overlay_root_with_updates
  SPECULATIVELY ~11x per block (1734 candidate builds in ~150s on the MPT lane). Our first-cut JMT store MUTATES
  a shared global tree + advances a version on EVERY call, so by validation time the tree is polluted by discarded
  candidate blocks -> committed block's root != re-executed root -> "Re-executed state root does not match" -> invalid.
- This is a CORRECTNESS bug, not a perf result: overlay_root_with_updates MUST be a PURE function (reth's MPT is —
  it reads the immutable DB trie + applies the overlay in-memory, persisting only on block commit). The fix:
  (1) overlay computes read-only from a persisted base (no speculative mutation); (2) advance/persist the base
  exactly once per committed block via reth's write_trie_updates hook (which carries the block number).
  = the "multi-validator idempotency" follow-up already flagged. Until then reth PRODUCES a valid JMT-rooted block
  (block 1 validated) but cannot sustain a live chain under speculative building.
