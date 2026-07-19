# SALT (MegaETH) as a payment-lane commitment — feasibility probe

Detached workspace (same pattern as `experiments/utxo-state`). **The detachment is load-bearing**:
SALT needs `[patch.crates-io]` repointing `ark-ff`/`ark-ec`/`ark-serialize`/
`ark-ed-on-bls12-381-bandersnatch` at MegaETH's algebra fork, and patches apply only from a
workspace ROOT. Putting them in arc-node's root would repoint all 11 `ark-*` crates the main tree
already resolves. Detaching keeps the blast radius here.

## Walls, both cleared
- **Toolchain**: SALT pins `nightly-2026-03-20`, but it builds fine on our stable **1.93.0** — the
  pin is their dev toolchain, not a hard requirement.
- **ark conflict**: resolved entirely inside this workspace; main tree untouched.

## Run
```bash
cargo build --release
BLOCKS=150 PER_BLOCK=800 ./target/release/salt-bench
```

## Result (150 blocks x 800 fresh accounts, 120k accounts, in-memory MemStore)

| stage (median) | cost |
|---|---|
| state / bucket layer | 0.62 ms |
| **trie / commitment (IPA-Pedersen)** | **4.97 ms** |
| **total root update** | **5.60 ms** (7.0 us per changed account) |

mean 7.68 ms, p99 15.34 ms. First block costs ~281 ms (one-time trie/precompute setup).

## Reading it

**The commitment is 89% of the cost.** SALT's two stages are separable and only the second yields a
root: `state.update_fin(kvs)` produces bucket updates, then `StateRoot::update_fin(&state_updates)`
does the elliptic-curve work. Timing only the first stage gives ~0.46 ms and is NOT a state root --
an easy and completely wrong number to report (we made exactly that error first).

**7 us/changed account is not a compute win over a hash tree.** It is consistent with ~2.3 ECMuls
per key at ~3 us each. Keccak-based structures do their per-node work in nanoseconds. So SALT's
case does NOT rest on beating an MPT at root arithmetic -- at this scale it does not.

**Its actual claims are about a different axis**: ~1 GB for 3B key-value pairs, and *no random disk
I/O* for root updates. Those target the cost our own measurements kept landing on (redb flushes for
JMT; scattered dirty mmap pages for dense) -- i.e. the STORE, not the arithmetic. That is the
hypothesis worth testing in reth, and it is a memory/IO claim, not a latency-per-block claim.

## Caveats (do not compare naively to the live lanes)
- **Standalone microbenchmark, not inside reth**: no DB reads, no overlay machinery, no reth
  overhead. The MPT/JMT/dense numbers come from a live node's own state-root histogram.
- **Compute only, in-memory store.** SALT is explicitly an in-RAM design whose durability would come
  from snapshotting, so it pays no per-block write here. Crediting it against lanes that DO persist
  would repeat the fairness error we already made once.
- **120k accounts**, vs millions in the 2h live runs. SALT's claims are about the billions regime.
