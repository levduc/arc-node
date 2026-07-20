# MPT on REAL Arc state (168 GB), past RAM — the crossover, and the catch

Arc testnet pruned snapshot, block 52,661,589, downloaded to papaduck's NVMe.
`mdbx.dat = 168 GB` on a machine with **78 GB RAM / 64 GB available** — 2.6x oversubscribed, so the
trie CANNOT be page-cached and lookups are real disk seeks. This is the regime neither the fleet
(RAM-resident) nor the synthetic harness (bulk-loaded, cache-friendly) could produce.

Measured with `commit-bench --existing`, read-only, 200 accounts changed per block, sampled by
seeking to pseudo-random keys so access is scattered like real load.

## Result — and it depends ENTIRELY on cache residency

The first run used a fixed PRNG seed, so a repeat run re-sampled the SAME 200 accounts and measured
page-cache hits (19.8 ms, ZERO disk reads). That is a methodology trap, not a result. Re-measured
with a varied `--seed` so each run touches FRESH accounts, capturing disk counters:

| accounts touched | median root | disk read (5 blocks x 200 accts) |
|---|---|---|
| fresh (seed 11111) | 308.9 ms | 52 MB |
| fresh (seed 22222) | 287.8 ms | 49 MB |
| fresh (seed 33333) | 263.7 ms | 46 MB |
| **already cached** | **19.8 ms** | **0 MB** |

**~48 KB of disk read per account updated** (~12 random 4 KB pages) — that is the direct evidence
the cost is disk seeks, not computation. Updating one account walks ~7 trie levels whose nodes are
scattered across 168 GB that cannot fit in 63 GB of RAM.

**The 15x spread between cached and uncached is the real finding.** Same state, same code; the only
variable is whether the touched accounts are resident. A production chain sits between the two:
active accounts repeat (cached), but fresh recipients do not.

### In context

| state | MPT root | |
|---|---|---|
| synthetic 1M, cached, bulk-loaded | 0.246 ms | best case |
| live fleet 5M, preseeded, cached | 3.23 ms | |
| **real 168 GB Arc state, UNCACHED accounts** | **~290 ms** | **~90-1200x worse** |
| real 168 GB Arc state, cached accounts | 19.8 ms | ~6-80x worse |

## The crossover

SALT costs ~3.1 ms for 200 changed accounts, ~independent of total state size (fleet: +4.3% over a
20x state increase). Through the SAME `overlay_root` seam:

| MPT case | SALT advantage |
|---|---|
| uncached accounts (~290 ms) | **~90x** |
| cached accounts (19.8 ms) | **~6x** |

So SALT's advantage is real but its SIZE depends on the workload's cache behaviour, not just on
state size. A payment lane paying fresh recipients -- exactly our `--fresh-recipients` load -- sits
toward the uncached end.

That is SALT's entire design thesis, and on this evidence it holds: once the MPT pays random disk
seeks, elliptic-curve arithmetic in RAM is vastly cheaper than hashing against a cold disk.

## The catch that makes it moot today

At the shipped `MemStore`'s measured ~1.83 KB/account, holding this state requires:

| accounts | SALT memory | machine |
|---|---|---|
| 50M | 91.5 GB | 78 GB total |
| 100M | 183 GB | 78 GB total |
| 200M | 366 GB | 78 GB total |

**SALT cannot be resident at Arc scale on this hardware**, and being anonymous heap it OOMs rather
than degrading (demonstrated: exit 137 at `MemoryMax=2G` where the MPT completed). The MPT handles
the same 168 GB state on 78 GB precisely because mmap pages evict.

So the honest position: **SALT wins the regime it was designed for by ~100x, but with the shipped
backend it cannot enter that regime.** Whether MegaETH's production store closes that gap is
untested — their 1 GB/3B claim covers the *authentication layer*, not key-value data, and is not
comparable to the total-RSS figure measured here.

## Caveats on the 357 ms
1. This is the SYNCHRONOUS `overlay_root` path — single-threaded, no prefetch, no sparse-trie cache.
   reth's live pipeline does far better (27.8 ms measured previously on a comparable snapshot,
   ~2 ms with Storage V2). **357 ms is an upper bound on MPT cost, not what a tuned node achieves.**
2. It is nonetheless the RIGHT comparison for our purposes, because the SALT lane is wired through
   that same `overlay_root` seam (`--engine.state-root-fallback`). Both commitments measured through
   one path.
3. Sampled accounts are uniformly random; a real block's accounts skew toward recently-active (and
   therefore warmer) state, so real per-block cost would sit below this.
