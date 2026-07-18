# Block-STM (grevm) for Arc's reth-2.3 fork — integration study

*2026-07-16, branch `gravity-payment-lane`. Sources: /home/papaduck/grevm (v2.2.2, HEAD f130ef4),
/home/papaduck/gravity-reth (reth-1.8.3 fork), our fork (reth v2.3.0 / revm 40.0.3 / alloy-evm 0.30).*

## Headline: the dependency wall we feared does not exist

grevm v2.2.2 is **already built on upstream revm 40.0.3** (`grevm/Cargo.toml:11-30` — revm 40.0.3,
revm-context 18.0.3, revm-primitives 24.0.0, alloy-evm 0.36, *no Galxe forks*; the old fork hacks
were replaced by a custom `NoRewardHandler`). Our fork pins **revm 40.0.3 exactly**. What remains
is alloy-evm 0.36 vs our 0.30 (minor-version drift on `PrecompilesMap`/`EthEvm`) and reth-side
glue. This is a **days-scale integration, not the multi-week revm port we did for reth 2.x**.
(Note: gravity-reth itself embeds an OLDER grevm via git rev on revm 29 — ignore its pins, reuse
only its wiring shape.)

## What grevm is (from source, not the README)

- Entry: `Scheduler::new(cfg: CfgEnv, block: BlockEnv, txs: Arc<Vec<TxEnv>>, state: ParallelState<DB>,
  with_hints: bool, custom_precompiles: Option<…>)` → `parallel_execute(concurrency)` →
  `take_result_and_state() -> (Vec<ExecutionResult>, ParallelState<DB>)` (`scheduler.rs:316-496`).
- Hybrid algorithm: a static **dependency DAG** from tx hints (caller/to/ERC20-selector parsing,
  `hint.rs`) + **Block-STM optimistic execution** with MV-memory (DashMap) and re-validation;
  ordered async commit; **miner-reward deferral** (the classic hot-coinbase fix); self-referential
  DAG edges to serialize genuinely hot accounts; sequential fallback for small blocks (<64 txs),
  `GREVM_FALLBACK_SEQUENTIAL`, or SELFDESTRUCT aborts.
- State interface: plain revm `DatabaseRef` (+ Send/Sync, clonable error). All parallel mutation is
  internal (DashMap caches); output is a standard revm `BundleState` → drops straight into reth's
  `HashedPostState::from_bundle_state` → state root, unchanged.

## Integration plan for our fork

Gravity's wiring shape (reusable): a `ParallelExecutor` trait + `GrevmExecutor` wrapper selected at
executor-construction time (`gravity-reth crates/ethereum/evm/src/{lib.rs:313,parallel_execute.rs}`).
Ours maps to:

1. Add `grevm = "2.2"` (crates.io) to the workspace. Version check: revm 40.0.3 ✓; align
   alloy-evm 0.30 → verify `DynPrecompile`/`PrecompilesMap` signatures (or bump our alloy-evm
   to 0.36 — small, same revm).
2. New `ArcParallelExecutor` in `crates/evm`: build `Vec<TxEnv>` via our existing
   `ConfigureEvm::tx_env`, destructure our `EvmEnv` into `(CfgEnv, BlockEnv)`, wrap the state
   provider in `ParallelState<DB>`, run `Scheduler`, zip `Vec<ExecutionResult>` into our receipts
   (same code shape as `ArcBlockExecutor`'s loop today), `take_bundle()` → `BlockExecutionOutput`.
3. Selection: env/CLI flag (`--arc.parallel-evm`) in the executor factory — mirrors
   `disable_grevm`. Sequential `ArcBlockExecutor` stays the default until differential-validated.
4. **Arc-specific care**: our custom precompiles must go through grevm's `custom_precompiles`
   arg; our EVM customizations (opcode hooks, known_bytecode fast-path) live in the ArcEvm
   handler — grevm builds its OWN evm per worker (`Context::mainnet()…`), so Arc handler
   customizations must be re-expressed in grevm's handler slot, or (simpler) we accept
   mainnet-EVM semantics on the payment lane only (it runs plain transfers by design).
5. Validation: differential replay (same fixture through serial + parallel executor, byte-identical
   receipts/state root) — the harness from the reth-2.0 port validation exists.

## Expected gains — grounded in OUR measurements (be honest)

| lane / workload | today (measured) | Block-STM effect |
|---|---|---|
| payment lane, native transfers | exec ≈ 13.5-33 µs/tx; exec is ~10-25% of block wall time (root+persist dominate) | independent transfers parallelize near-linearly, BUT Amdahl: shaving 13.5µs→~3µs/tx cuts block time ≤15%. **Modest.** |
| EVM lane, guzzler (hot contract+counter) | strictly serial dependency chain (measured in `contention`) | **zero** — Block-STM cannot parallelize a serial chain; grevm handles it via key_tx (graceful, not faster) |
| EVM lane, diverse contract mix (realistic dApp traffic) | not measured on our stack; gravity claims 4x at 100k accts | the real win: this is where grevm's DAG+STM shines. Expect 2-5x exec on multi-contract workloads |
| proposer build path | build = one serial execution pass | same speedup applies to building — improves max cadence under full blocks |

Conclusion: grevm is worth integrating **for the EVM lane** (general contract traffic) and as the
builder accelerator; for the payment lane's native transfers it is nearly moot — our lane is
signature/root/persistence-bound, exactly as the paper's App. E measured. Priorities stay:
commitment (see jmt-state-commitment.md) > persistence > parallel EVM, for the payment lane;
the reverse for the EVM lane.

## Effort estimate

- Wrapper + flag + receipts glue: 2-3 days.
- alloy-evm 0.30/0.36 alignment: hours-to-1-day (or bump to 0.36).
- Arc precompiles through `custom_precompiles`: 1 day.
- Arc handler semantics parity (if needed beyond precompiles): the risky unknown — audit
  `crates/evm/src/handler.rs` customizations against grevm's `NoRewardHandler`; budget 2-4 days.
- Differential validation: 1-2 days with existing harness.
Total: **~1.5-2 weeks to a validated, flag-gated parallel executor** — vs the 6+ weeks the
reth-2.x port took, because the revm boundary is already aligned.
