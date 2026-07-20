# Fair comparison of authenticated key-value primitives

Same key set, same change set, same scale, same machine, back to back. `commit-bench --primitives`.
Keys are splitmix64-avalanched (uniform in the high bytes) standing in for `keccak(address)`.

| primitive | commitment | node store | value store |
|---|---|---|---|
| MPT | keccak Merkle-Patricia | MDBX (disk) | MDBX (disk) |
| JMT | keccak Jellyfish MT | redb (disk) | redb (disk) |
| dense | keccak fixed-depth | mmap array | **heap** BTreeMap |
| SALT | IPA/Pedersen (EC) | **heap** MemStore | **heap** MemStore |

## In RAM

**N = 2M, 200 changed/round**

| primitive | build | RSS | median | p99 |
|---|---|---|---|---|
| JMT | 30 s | 1.07 GB | 3.712 ms | **4.629 ms** |
| dense | 18 s | 1.70 GB | **2.371 ms** | 22.239 ms |
| SALT | 10 s | 0.96 GB | 3.055 ms | 28.451 ms |

**N = 10M, 200 changed/round**

| primitive | build | RSS | median | p99 | p99/median |
|---|---|---|---|---|---|
| JMT | 212 s | 6.40 GB | 5.448 ms | **13.417 ms** | **2.5x** |
| dense | 119 s | 6.00 GB | **2.725 ms** | 127.080 ms | 47x |
| SALT | 64 s | **2.77 GB** | 3.195 ms | 120.472 ms | 38x |

Medians sit within ~2x of each other. **The real separation is tail latency**: JMT stays at 2.5x its
median while dense and SALT blow out to 38-47x. For a chain where worst-case block time bounds the
cadence, that matters more than the median.

## Under memory pressure — the decisive axis

N = 10M, `MemoryMax=8G`:

| primitive | outcome |
|---|---|
| JMT | **survived** (built in 259 s; redb keeps everything on disk) |
| dense | **OOM-killed** |
| SALT | never reached |

Earlier notes called dense "evictable" — that was wrong. Only its *nodes* are mmap'd; its **values
live in a heap BTreeMap**, so it dies under pressure just like SALT. **JMT is the only one of the
three that degrades instead of dying**, because redb puts both nodes and values on disk.

## Against the MPT on real Arc state (44.4M accounts, 168 GB)

| | median root |
|---|---|
| MPT via reth `overlay_root` (synchronous) | 222.9 ms |
| MPT via reth's tuned pipeline (measured previously, comparable state) | ~27.8 ms |
| SALT, all 44.4M accounts resident (14.1 GB) | 11.0 ms |

## So: is SALT better?

**Not clearly.** The honest reading across everything measured:

1. **In RAM, no primitive dominates.** Medians are within ~2x; SALT is the most memory-efficient
   (0.28-0.32 KB/key vs JMT/dense at ~0.6), JMT has by far the best tail.
2. **SALT's advantage over the MPT is real but much smaller than the headline.** 20.2x against
   reth's *synchronous* path, but only **~2.5x** against its tuned pipeline. Quoting 20x compares
   an optimised SALT against an unoptimised MPT.
3. **SALT commits strictly less.** Arc's state has 568,885,040 storage slots it does not represent
   at all. A production payment lane needs storage committed, or contracts prohibited AND enforced.
4. **SALT and dense OOM rather than degrade.** The MPT runs 168 GB of state on a 78 GB box because
   mmap evicts; SALT must be fully resident.
5. **JMT is the sleeper.** Same keccak-only assumptions as the MPT (no discrete-log), tightest
   tail, the only alternative that survives memory pressure, and it commits the same
   `(nonce, balance)`. It "lost" the first head-to-head only because of its redb write path, which
   is a tuning problem, not a structural one.

**If a payment lane had to ship on one of these today: JMT**, on robustness. SALT is the more
interesting research direction — its flat-ish cost and EC batching are genuinely different — but as
shipped it trades away graceful degradation and storage coverage for a modest constant-factor win.

## Caveats
- The MPT commits `RLP(nonce, balance, storageRoot, codeHash)`; the other three commit
  `(nonce, balance)`. The MPT does strictly more work per leaf, and no flag equalises that.
- RSS deltas for JMT/dense/SALT are measured sequentially in one process, so later primitives
  inherit earlier residue. SALT's figure is corroborated independently (14.1 GB at 44.4M on the
  real snapshot = 0.32 KB/key), the other two are not.
- Durability differs: MPT and JMT persist nodes, dense msyncs pages, SALT persists nothing.
