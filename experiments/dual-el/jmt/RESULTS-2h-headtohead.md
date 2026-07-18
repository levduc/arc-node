# JMT vs MPT — 2-hour head-to-head (payment lane commitment)

Two identical `arc-node-execution` ELs (reth 2.3), same Prague-capped localdev genesis, same
mock-CL block loop (250ms), same transfer load (800 tx/s, `--fresh-recipients` so state grows).
**Only difference:** `ARC_PAYMENT_ROOT=jmt` on one lane → its state root is a 16-shard incremental
Jellyfish Merkle Tree (redb node store) instead of reth's MPT (MDBX). Bare-metal (host binary).
Root latency = reth's `reth_sync_block_validation_state_root_histogram`, sampled every 30s.

## Result (7255s, ~26k blocks/lane, ~4–5M fresh accounts/lane)

| metric (windowed root latency) | MPT (MDBX) | JMT (redb, incremental) |
|---|---|---|
| median | **17.81 ms** | **12.75 ms** |
| mean | 17.30 ms | 21.46 ms |
| p90 | 20.66 ms | 53.60 ms |
| p99 | 22.43 ms | 85.38 ms |
| max | 22.84 ms | 101.60 ms |
| growth (first→last 10min) | +7.86 ms | +8.68 ms |
| **state-root mismatches** | **0** | **0** |

### JMT bimodal split (threshold 2× median = 26 ms)
- **Steady-state windows (79% of samples): mean 12.20 ms** — ~30% *faster* than MPT's 17.81 ms.
- **redb-flush windows (21%): mean 56.59 ms** — these dominate the tail and pull JMT's mean above MPT.

## Reading

1. **The incremental JMT fusion is correct.** 0 "Re-executed state root does not match" over ~26k
   blocks × ~3 root calls each on BOTH lanes. Build/validate/witness all agree — the O(k·log n)
   read-only-root + per-block commit design holds under 2h of growing state.

2. **The JMT *algorithm* is competitive-to-faster than MPT.** Its steady-state root compute (12.2 ms)
   beats MPT (17.8 ms) by ~30%, and it scales the same way (both grew ~+8 ms as state grew ~4–5M
   accounts). The tree is not the problem.

3. **The redb node store is the JMT tax.** 21% of blocks hit a ~57 ms redb-flush window (p99 85 ms),
   which is what makes JMT's *mean* (21.5 ms) exceed MPT's (17.3 ms). MPT rides reth's tuned MDBX with
   its write buffering; the naive redb store flushes synchronously and its flush cost grows with the
   tree (52 ms at 60min → 80–100 ms at 2h). **Optimization target = the node-store persistence layer,
   not the commitment algorithm.**

4. **Neither hit the disk-bound cliff.** At ~4–5M accounts both state sets still fit in RAM, so the
   large-state regime where MPT's shared disk-bound trie collapses (27.8 ms cold on the real 169 GB
   Arc state) was not reached — this run isolates *commitment* cost at in-RAM sizes, not the I/O cliff.

## Follow-ups (to make JMT win outright)
- Replace/tune the redb node store: batch flushes off the hot path, or use MDBX with reth's write
  buffering, or an in-RAM node cache with async persistence (mirrors reth's persistence-threshold).
- Run at RAM-exceeding state (the App C regime) where MPT pays disk I/O and JMT's flat O(k·log n)
  compute should pull decisively ahead.
