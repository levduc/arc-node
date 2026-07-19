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
