# Past-RAM harness: MPT vs SALT over an identical account set

`crates/commit-bench` — both structures hold all N accounts and update k per block. Uses reth's
REAL MDBX-backed MPT (`overlay_root` over a live tx, with `AccountsTrie` intermediate nodes built
first) and SALT's `readonly_root`, so neither side is a toy.

## Why the live fleet couldn't answer this
The fleet tops out at RAM-resident state, where MPT always wins (2.79x at 250k, 2.40x at 5M) and
extrapolation says trie size alone needs ~10^6x more state to close the gap. The crossover requires
the *step change* when lookups miss cache. This harness reaches it directly.

## Result 1 — uncapped, 1M accounts, 200 changed/block (both hold 1M)

| | median root |
|---|---|
| MPT (reth, MDBX) | **0.246 ms** |
| SALT | 3.146 ms |
| | MPT faster **12.8x** |

Setup costs: MPT intermediate trie nodes 0.99 s; SALT seed of 1M accounts 4.5 s (~4.5 us/acct).

Note this MPT number is a **lower bound on real cost**: accounts are bulk-inserted in sorted order
(`append` requires ascending keys), the friendliest possible layout. Measured elsewhere: the same
~5M accounts cost the MPT 8.62 ms grown organically vs 3.23 ms bulk-loaded.

## Result 2 — THE STRUCTURAL FINDING: under a memory cap, SALT dies where MPT degrades

`systemd-run -p MemoryMax=2G -p MemorySwapMax=0`, 1M accounts, 4.30 GB on disk:

| | outcome under 2 GB cap |
|---|---|
| MPT | **completed** — built its trie in 966 ms; mmap pages evict to page cache |
| SALT | **exit 137, OOM-killed** during seeding (`Failed with result 'oom-kill'`) |

This is a property of the design, not a tuning problem. The MPT's state is mmap-backed, so under
pressure the kernel evicts pages and the process merely slows. SALT's `MemStore` is **anonymous
heap**, which cannot be evicted, so memory pressure kills it outright. A node that degrades is
operable; a node that OOMs is not.

## Result 3 — measured footprint of the SHIPPED MemStore

| accounts | max RSS |
|---|---|
| 1M | 2.16 GB |
| 2M | 3.99 GB |
| **marginal** | **~1.83 KB / account** |

At that rate: 10M -> ~18 GB, 100M (Arc scale) -> ~183 GB.

**IMPORTANT, do not misread this as contradicting SALT's headline.** Their README claims *"for up to
3 billion key-value pairs, SALT's **authentication layer** requires only a 1 GB memory footprint"* --
that is the *authentication/trie layer*, NOT the key-value data. Our 1.83 KB/account is **total
process RSS**, which includes every account's data held in `MemStore`'s `BTreeMap`s plus reth's own
structures. The two numbers measure different things and must not be divided against each other.

What can be said fairly:
- `MemStore` is documented as "a simple in-memory storage backend ... using BTreeMap collections".
  It is a straightforward reference backend, almost certainly not the compact production
  representation the 1 GB / 3B figure refers to.
- For *our* integration, total footprint is what binds operationally, and with this backend it is
  ~1.83 KB/account.
- Testing SALT's actual memory claim would require isolating its authentication layer from value
  storage, or using whatever compact store MegaETH runs in production. Neither is in this crate.

## Standing conclusions
1. SALT's **flat-cost property is real** (fleet: +4.3% vs MPT's +21% over a 20x state increase).
2. SALT's cost is **~97% elliptic-curve arithmetic**, so it is CPU-bound; RAM residency buys it
   nothing until the MPT starts paying disk seeks.
3. **Inside the cached regime the MPT wins**, by 12.8x here and 2.40-2.79x on the fleet.
4. Under memory pressure **SALT OOMs while the MPT degrades** — with this backend, the RAM-resident
   design is a liability exactly where it was supposed to be an advantage.
5. The regime where SALT should win (state >> RAM, MPT paying random seeks) remains **unmeasured**,
   because with this backend SALT cannot be resident at that scale on these machines.
