# reth fork — parallel execution + cheap state root for the payment lane

Goal: make a modified reth that handles the payment-lane workload (full 1-Ggas blocks of native
transfers) far faster, by attacking where the time actually goes.

## Why (the measurement that set the direction)

`crates/evm/examples/transfer_bench.rs` ran a full 1-Ggas block (47,618 × 21k-gas transfers) through
the **real `ArcBlockExecutor`**: **2.15 µs/tx** (465k tx/s serial). The live node measures **~11 µs/tx**
(379 ms / 34k-tx block). So **~80 % of live execution cost is NOT the EVM/executor** — it's reth's
engine-tree machinery *around* it:

- the async **state-root task**: multiproof + sparse-trie prewarm + per-tx receipt streaming to a
  background root task (`payload_processor`), and
- per-tx state-provider/cache lookups.

Two levers, and they're synergistic:

1. **Kill the async state-root machinery.** The lane's state is small/RAM-resident, so a *synchronous*
   root over the bundle at block end is ~0.8 ms (measured). reth already has this: `StateRootStrategy`
   has a `Synchronous` variant that uses `spawn_cache_exclusive` (NO multiproof task, NO prewarm),
   selected by `config.state_root_fallback()`. **This is a stock CLI flag — no fork needed:**
   `--engine.state-root-fallback`. Try it on the payment EL first; it should remove most of the 80 %.
2. **Parallel execution** of the remaining ~20 %. The per-tx loop is
   `reth-fork/crates/engine/tree/src/tree/payload_validator.rs` (`execute_transactions`, ~line 1265,
   `executor.execute_transaction(tx)`). Native transfers among a closed set parallelize with:
   - **deferred coinbase**: Arc credits the beneficiary the FULL fee (base+tip) on *every* tx
     (`handler.rs::reward_beneficiary`, base fee not burned) → a naive parallel scheme sees an all-tx
     write conflict on the beneficiary; accumulate per-thread and apply once.
   - **blocklist read cache**: every transfer does 2 SLOADs on `NATIVE_COIN_CONTROL_ADDRESS`
     (sender + recipient, `handler.rs::pre_execution`); read-only per block → cache the bitmap once.
   - conflict handling on hot recipients (same account paid by many) — Block-STM-style abort/retry or
     deterministic partitioning.

## Fork wiring (dev box only — never committed)

The fork lives at `~/reth-fork` (`cp -a ~/reth-2.3-ref ~/reth-fork`, v2.3.0). Arc's 41 reth deps are
re-pointed at it via a `[patch]` section that `apply-fork.sh` appends to `Cargo.toml`. It uses
**absolute local paths**, so it must NOT be committed (breaks Docker/CI/other checkouts).

```bash
RETH_FORK=~/reth-fork ./apply-fork.sh        # wire Arc -> local reth fork
cargo build --release -p arc-node-execution  # first build ~10 min, then incremental
./apply-fork.sh revert                        # remove before any Docker/prod build (verified: Cargo.toml == HEAD)
```

Verified: full `arc-node-execution` binary builds against the fork (10m30s), and apply/revert is a
clean round-trip.

## To run the forked binary in the demo

The payment EL runs as the `arc_execution:latest` Docker image. To exercise fork changes on the
fleet/single-machine demo, rebuild that image from the fork-built binary (see `make build-docker`) —
the Cargo `[patch]` must be applied at image-build time, which means building the binary on the host
and COPYing it in, or a Dockerfile that mounts the fork. (Docker can't reach absolute host paths, so
host-build + copy is the path.) TODO: add a `build-docker-fork` target.

## Status / next

- [x] fork builds against local reth (enabler)
- [x] both levers located precisely; blocklist + coinbase semantics confirmed
- [ ] measure `--engine.state-root-fallback` on the payment EL (no fork — do this first)
- [ ] parallel `execute_transactions` path in the fork (deferred coinbase, blocklist cache, conflicts)
- [ ] differential replay vs stock binary (consensus-correctness) BEFORE any fleet run
- [ ] docker image from the fork + re-measure fleet tps at 1 Ggas
