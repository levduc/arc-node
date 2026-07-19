# SALT (MegaETH) vs reth's MPT — 2-hour head-to-head

Two identical `arc-node-execution` ELs (reth 2.3), same genesis, same mock-CL (250 ms blocks), same
load (800 tx/s, `--fresh-recipients`). Only difference: `ARC_PAYMENT_ROOT=salt`. Bare-metal, one box.
Latency = reth's `reth_sync_block_validation_state_root_histogram`, 30 s windows.

## Result (7261 s, ~28k blocks/lane, ~4-5M accounts/lane)

| windowed state-root latency | MPT (MDBX) | SALT |
|---|---|---|
| median | 7.79 ms | **4.12 ms** (-47%) |
| mean | 7.96 ms | **4.61 ms** (-42%) |
| p90 | 8.68 ms | **4.34 ms** (-50%) |
| p99 | **21.07 ms** | 27.08 ms |
| max | **36.54 ms** | 48.94 ms |
| growth (first-to-last 10 min) | **+3.28 ms** | **+0.61 ms** |
| blocks | 27,582 | 27,958 |
| mismatches / panics | 0 / 0 | 0 / 0 |

## Reading

1. **Correct and stable**: 0 mismatches, 0 panics over ~28k blocks. SALT computes consensus-valid
   state roots inside reth.
2. **Same shape as the dense lane**: SALT starts SLOWER on small state (3.61 vs 5.19 ms is
   misleading here -- at head 65 it was 1.78 vs 1.48 in MPT's favour) and wins as state grows,
   because the MPT degrades (+3.28 ms) while SALT stays nearly flat (+0.61 ms).
3. **Wins the body, loses the tail.** Median/mean/p90 are decisively SALT's -- p90 is 2x better.
   But p99 and max are worse (27.08 / 48.94 vs 21.07 / 36.54). Both lanes have heavy tails in this
   run; a system tuned for worst-case block time should not read the median win as the whole story.

## THE COMPARISON IS NOT LIKE-FOR-LIKE — SALT does strictly less work

Quantified from the run, not assumed:

| | MPT | SALT |
|---|---|---|
| MDBX on disk | 73 MB | 56 MB (17 MB of trie nodes never written) |
| trie updates persisted | yes | none (`TrieUpdates::default()`) |
| account leaf | `RLP(nonce, balance, storageRoot, codeHash)` | `(nonce, balance)` only |
| commitment storage | MDBX, disk-backed | RAM-only `MemStore` |

Part of the gap is structural (the measured window times root COMPUTATION, where MPT pays B-tree
probes into MDBX and SALT walks memory -- the real effect). But the narrower leaf and absent trie
persistence inflate it by an unquantified amount. **Do not cite the 47% as SALT-vs-MPT until the
leaf encodings match and both persist on the same terms.**

## Also: every number in this file was produced by an UNSOUND root

Discovered while reviewing this run. The commitment contained only accounts that flowed through
`write_hashed_state` since process start; the 1293 genesis-prefunded accounts were written at
`init` and never appeared. So the root authenticated only *touched* accounts. Fixed in `9f45efc`
(seed from `HashedAccounts` at startup) -- but this run predates the fix.

The relative cost comparison survives (both lanes did internally consistent work), but these are
not measurements of a sound commitment. Re-run after the fix before publishing.

The same defect applied to the JMT and dense results.

## Still open before this is a payment-lane client
- storage slots (unauthenticated if a contract is ever deployed on the lane) -- must be enforced
- unwind/reorg rollback
- proofs: `eth_getProof` assumes MPT; SALT has its own Witness API
- the seeding fix must be applied to the dense and JMT lanes
- **the memory-capped run**: SALT's advantage comes from holding state in RAM. Untested past RAM.
