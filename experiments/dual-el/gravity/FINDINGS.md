# gravity-reth as Arc payment-lane EL — experiment findings (2026-07-14/15, NEGATIVE RESULT)

Goal: swap the payment lane's arc-reth for Galxe's gravity-reth (reth-1.8.3 fork: grevm parallel EVM,
16-way parallel merklization, gravity cache, RocksDB nested-trie storage) and measure its state-root
speed on 10M preseeded accounts vs arc-reth's 12.04 ms/blk root / 2.5 µs/tx (execcmp2 phase B).

## Verdict
**Unusable as an Engine-API-driven EL at scale.** All of gravity's performance machinery is wired to
"pipe execution", which is driven by the Gravity SDK's own consensus event bus. In standard Engine API
mode (`--gravity.disable-pipe-execution`, required — default mode HANGS fcU waiting on that bus):

- getPayload (build) = **~47-59 s per block at 10M accounts, even EMPTY blocks** (measured 5x)
- newPayloadV4 (import) = **~44 s** ("State root task finished elapsed=44.25")
- Cost is O(total state), not O(changes): ~5.5 µs/account-in-state per block.

## Mechanism
Their `init`/persistence populate ONLY the fork's own structures (RocksDB `db/state`,
`account_trie`, `storage_trie` CFs + nested trie, TrieUpdatesV2). The stock engine paths (sparse-trie
state-root task at validation; `StateRoot::overlay_root*` at build) walk reth's V1 MDBX trie tables —
which are EMPTY at genesis and, on a genesis-preseeded chain, can never fill: trie updates only carry
changed paths, and non-pipe mode sets `triev2: Default::default()` everywhere, so the nested trie
isn't maintained either. Every root = full-state walk, forever. Bootstrap experiment (build block 1,
import+finalize on all 4 nodes, persistence-threshold 0, wait, rebuild) confirmed: second build still
44.4 s. Their fast incremental primitive (`NestedStateRoot::calculate`, used by their MerkleStage/
pipeline) exists but nothing in the Engine-API flow persists its updates per block — fixing this means
rewiring their persistence, i.e. upstream fork surgery.

## What DID work (reusable)
- Full Engine API proposer cycle vs gravity-reth on small state: fcU+attrs -> getPayloadV4 ->
  newPayloadV4 -> fcU advance, all VALID (smoke, ~1k accounts, 0.6 s builds).
- CL improvement (committed): `ARC_PAYMENT_GENESIS_FILE_PATH` env — per-lane genesis for Engine API
  V4/V5 selection; payment lane can now run a DIFFERENT fork schedule than the EVM lane (needed:
  gravity/reth-1.8.3 predates Osaka + doesn't know arc's zero7; payment genesis is Prague-capped by
  demo-gravity.sh). CL auto-selects getPayloadV4 for the lane. -38005 "Unsupported fork" otherwise.
- Ops: gravity 2M-entry cache default OOMs an 11G cap on 10M state under load (500k in launcher);
  boot uncapped then clamp (same as arc ELs); `--gravity.disable-pipe-execution` mandatory.
- CL skip-round fix (509f404) survived ~70 consecutive proposer build failures with zero crashes.

## Files
- `Dockerfile` — wraps a host-built gravity-reth binary (build context must contain the binary;
  reth's .dockerignore excludes target/).
- `launch-payment-els-gravity.sh` — same contract as the arc launcher; stock reth flags +
  `--gravity.disable-pipe-execution --gravity.cache.capacity 500000`.
- `demo-gravity.sh` — full 10M-preseed dual-EL run (payment genesis stripped to Prague).
- Binary: /home/papaduck/gravity-reth/target/release/reth (b49b486, image gravity_reth:latest).

## If ever revisited
Options: (a) run at ~100k accounts where O(state) ≈ 0.5 s/block fits the deadline (comparison would
measure their UNOPTIMIZED path — misleading, not recommended); (b) patch their fork so Engine-API mode
maintains the nested trie (persistence surgery + route provider state_root through NestedStateRoot +
--engine.state-root-fallback); (c) adopt Gravity SDK consensus (out of scope by definition).
