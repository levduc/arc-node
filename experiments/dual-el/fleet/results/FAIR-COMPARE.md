# Fair MPT-vs-SALT comparison — 4-machine fleet, fresh chain

`fair-compare.sh 1800 300`, 55/55 samples symmetric, agreement held on every sample.
val3 on NVMe (FASTSSD=1). Both lanes: identical native transfers, identical rate, same genesis
(100M gas, chainId 1337), same machines.

|                     | EVM (MPT) | PAY (SALT) |            |
|---------------------|-----------|------------|------------|
| landed tps          | 321.0     | 321.4      |            |
| txs / block         | 82.3      | 82.3       | validity   |
| exec ms / block     | 3.190     | 3.230      | CONTROL    |
| persist ms / block  | 46.54     | 47.38      | CONTROL    |
| **root ms / block** | **2.670** | **7.440**  | COMMITMENT |
| root us / tx        | 32.4      | 90.4       |            |

**Controls agree** (exec within 1.3%, persist within 1.8%), so the root delta is attributable to the
commitment and not to workload, disk, or execution differences.

## Result: on a FRESH chain, MPT is 2.79x faster than SALT

This is the regime where MPT is expected to win, and it is not the whole story:

- The chain's trie is small enough to sit entirely in page cache, so the MPT pays only cheap keccak
  hashing and NONE of the disk I/O that SALT exists to avoid.
- SALT's cost is ~97% elliptic-curve arithmetic (measured: 0.083 ms bucket layer vs 2.634 ms
  IPA/Pedersen commitment). EC scalar mults are microseconds; keccak is nanoseconds. No amount of
  RAM changes that -- it is CPU-bound, not memory-bound.

## Why this is one point on a curve, not a verdict

MPT's cost grows with state; SALT's barely does. From the 2 h single-machine run:

| | first 10 min | last 10 min | growth |
|---|---|---|---|
| MPT  | 5.25 ms | 8.62 ms | **+3.37** |
| SALT | 5.45 ms | 5.78 ms | **+0.61** |

MPT started ahead and lost by the end of that run. Re-run this script as the fleet chain grows to
locate the crossover on the fleet.

## Uncontrolled, and all three flatter SALT
1. SALT commits `(nonce, balance)`; an MPT account leaf is `RLP(nonce, balance, storageRoot, codeHash)`.
2. SALT persists **no** trie nodes; the MPT writes its trie to MDBX every persistence batch.
3. SALT's commitment is RAM-resident, so this holds only while it fits.

So the true MPT-vs-SALT gap on a fresh chain is **at least** 2.79x in MPT's favour -- correcting any
of the three would widen it, not narrow it.

---

# Second point: 5M preseeded accounts (same fleet, same method)

`PRESEED=5000000` on the SHARED genesis (367.6 MB, 5,001,293 alloc), both lanes re-inited from it.
SALT seeded all 5,001,293 accounts at first root. 55/55 samples symmetric, agreement held.

|                     | EVM (MPT) | PAY (SALT) |            |
|---------------------|-----------|------------|------------|
| landed tps          | 321.7     | 321.4      |            |
| txs / block         | 82.4      | 82.3       | validity   |
| exec ms / block     | 3.340     | 3.000      | CONTROL    |
| persist ms / block  | 49.11     | 47.76      | CONTROL    |
| **root ms / block** | **3.230** | **7.760**  | COMMITMENT |

## The curve so far

| accounts | MPT root | SALT root | ratio |
|---|---|---|---|
| ~250k | 2.670 ms | 7.440 ms | MPT 2.79x |
| 5M    | 3.230 ms | 7.760 ms | MPT 2.40x |
| growth over 20x state | **+21%** | **+4.3%** | narrowing |

MPT's cost grows ~5x faster than SALT's, and the gap narrows -- SALT's flat-cost property is real
and measurable. But:

## Trie size ALONE will never produce a crossover

At ~21% MPT growth per 20x state, closing a 2.40x gap needs ~10^6 x more state (trillions of
accounts). Inside the page-cached regime the MPT simply wins. A crossover requires the STEP CHANGE
when lookups start missing cache and become disk seeks -- not a gradual curve. Anyone planning to
"just preseed more" should read this row first.

## METHODOLOGICAL FINDING: preseeded state != organically grown state

At a comparable ~5M accounts:

| how the state was reached | MPT root |
|---|---|
| organic growth (2 h single-machine run) | **8.62 ms** |
| genesis preseed (this run) | **3.23 ms** |

2.7x apart at the same account count. Preseed bulk-loads a contiguous, cache-friendly layout;
organic growth applies millions of individual trie updates that scatter and fragment the store. So
preseeding reaches a large state quickly but does NOT reproduce its cost profile, and it flatters
the MPT specifically.

Caveat: those two runs differ in hardware and config (single-machine bare metal vs 4-machine
fleet), so treat the cross-run number as suggestive, not conclusive. The within-run growth rates
are the trustworthy part.

## What would actually settle it
Organic growth to a genuinely RAM-exceeding state, or a cap tight enough to force eviction without
OOMing on the genesis-resident alloc (~250 B/account, so preseed itself consumes the cap budget --
another reason preseed and memory-capping fight each other).
