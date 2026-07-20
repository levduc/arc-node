# MEASURED: MPT vs SALT on real 168 GB Arc state, both holding all 44.4M accounts

The definitive run. Previous versions of this comparison were extrapolated; challenged on that, we
seeded SALT from the actual snapshot and measured both.

**State**: Arc testnet pruned snapshot, block 52,661,589. `mdbx.dat` 168 GB; 44,429,999 accounts;
568,885,040 storage slots; 3,736,057 MPT intermediate nodes. Machine: 78 GB RAM / 63 GB available.
**Method**: `commit-bench --existing --with-salt`, 200 accounts changed per block sampled by
seeking to pseudo-random keys (varied `--seed`), both structures given the identical change set.

| | median root |
|---|---|
| MPT (reth `overlay_root`, disk-backed) | **222.9 ms** |
| SALT (all 44.4M accounts resident, 14.1 GB) | **11.0 ms** |
| | **SALT 20.2x faster** |

SALT seeding: 44,429,999 accounts in 444 s, **14.1 GB RSS = ~0.32 KB/account**.

## Two extrapolations this run corrected

| claim | extrapolated | MEASURED |
|---|---|---|
| SALT memory at 44.4M accounts | 81 GB -> "cannot fit on this box" | **14.1 GB — fits easily** |
| SALT speed advantage | ~90-100x | **20.2x** |

1. **Memory was 5.7x overestimated.** The 1.83 KB/account figure came from *total process RSS* in
   the synthetic harness, which included MDBX page cache for the test DB and reth's own structures.
   The true marginal cost is ~0.32 KB/account. The earlier conclusion "SALT cannot enter the regime
   where it wins" was therefore **wrong** — it enters it comfortably.
2. **Advantage was ~4.5x overestimated**, because SALT is *not* perfectly flat in state size:
   3.1 ms at 1M accounts -> 11.0 ms at 44.4M. That is sublinear (3.5x cost for 44x state, i.e.
   tree-depth growth) but not free. The fleet's "+4.3% over 20x state" held only across a much
   smaller range.

## What stands

- **SALT wins decisively in the past-RAM regime: 20.2x on real Arc state.** This is the crossover,
  measured rather than argued, with both structures holding identical real state.
- The MPT's cost here is dominated by disk seeks (~48 KB read per account updated, measured via
  `/proc/diskstats`), and swings ~15x with cache residency (19.8 ms for already-cached accounts vs
  ~290 ms for fresh ones).
- In the cached regime the ordering **reverses**: MPT 0.246 ms vs SALT 3.1 ms synthetic (12.8x),
  MPT 3.23 ms vs SALT 7.76 ms on the live fleet (2.4x).

So the honest summary across all measurements: **whichever structure avoids touching disk wins, by
one to two orders of magnitude, and the crossover is cache residency — not state size, trie depth,
or arithmetic.**

## Caveats
1. `overlay_root` is reth's SYNCHRONOUS path — no parallel state root, no prefetch, no sparse-trie
   cache. reth's tuned pipeline measured 27.8 ms on a comparable snapshot and ~2 ms with Storage V2.
   So 222.9 ms is an **upper bound** on MPT cost. It remains the right comparison here only because
   the SALT lane is wired through that same seam.
2. SALT commits `(nonce, balance)` only. The MPT commits full account leaves and this state has 569M
   storage slots that SALT does not represent at all. **A production payment lane would need storage
   committed, or contracts prohibited and that prohibition enforced.**
3. SALT's 14.1 GB is unevictable anonymous heap: it OOMs rather than degrading under pressure
   (demonstrated at `MemoryMax=2G`), whereas the MPT's mmap survives on far less RAM than its state.
