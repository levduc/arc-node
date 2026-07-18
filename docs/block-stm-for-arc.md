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

## Expected gains — PAYMENT LANE FIRST (revised for the 10k-TPS regime)

The original analysis (below, kept for honesty) said "payment lane moot" — that was anchored to
the 1,500 tx/s era. **At the current target regime (200M gas, ~9,500 transfers/block, 10k+ TPS)
the math flips:**

- Measured (fleet 2h run): payment exec = **64.4 ms/block at 4,761 txs** (13.5 µs/tx, one core).
- At 9,500-tx blocks: **~130–300 ms/block of serial execution — the entire 250 ms cadence budget.**
- And it is paid TWICE per height (proposer build + validator re-execute), on every validator.

Payment-lane transfers are grevm's best case: disjoint sender/recipient pairs → near-empty DAG →
near-linear scaling. The ONE hot account in a pure-transfer block is the fee recipient — and
grevm's miner-reward deferral (NoRewardHandler + lazy commit) exists precisely for that.
Expected: 64 ms → ~8–12 ms at 4,761 txs; ~130–300 ms → ~20–40 ms at 9,500 txs on 8–16 cores.
**That converts execution from cadence-killer back to rounding error at 10k TPS — grevm's
primary target here is the payment lane**, with the EVM lane's contract traffic as the bonus.

| lane / workload | today (measured) | Block-STM effect |
|---|---|---|
| payment lane @1.5k TPS | exec 13.5-33 µs/tx, minor share | modest (~15% block time) |
| **payment lane @10k TPS (200M blocks)** | **exec ≈ cadence budget (130-300 ms/blk serial)** | **the unlock: ~8-16x on disjoint transfers + fee-recipient deferral** |
| EVM lane, guzzler (hot contract) | strictly serial chain (measured) | zero — cannot parallelize a dependency chain |
| EVM lane, diverse contracts | unmeasured on our stack | 2-5x plausible (gravity's claim) |
| proposer build path | serial pass per block | same speedup — directly raises max cadence |

## Effort estimate

- Wrapper + flag + receipts glue: 2-3 days.
- alloy-evm 0.30/0.36 alignment: hours-to-1-day (or bump to 0.36).
- Arc precompiles through `custom_precompiles`: 1 day.
- Arc handler semantics parity (if needed beyond precompiles): the risky unknown — audit
  `crates/evm/src/handler.rs` customizations against grevm's `NoRewardHandler`; budget 2-4 days.
- Differential validation: 1-2 days with existing harness.
Total: **~1.5-2 weeks to a validated, flag-gated parallel executor** — vs the 6+ weeks the
reth-2.x port took, because the revm boundary is already aligned.

## MEASURED — grevm-style Block-STM prototype (implemented, `blockstm` bin)

Built the deterministic core of grevm's design (hint-derived dependency DAG → level-synchronous
parallel apply, conflict-free by construction; + deferred-fee-recipient) and measured it on the
payment lane's actual workloads. 10M accounts, 9,500 tx/block, 16 threads:

**When per-tx work is real (state read modeled at ~1µs/account — matches lean_state's 25ms serial
execute phase):**
| workload | serial | Block-STM | speedup | note |
|---|---|---|---|---|
| disjoint transfers | 20.6 ms | 2.32 ms | **8.9×** | best case, near-linear |
| **pool(10k) — our real spam pattern** | 21.4 ms | 2.51 ms | **8.5×** | the demo workload |
| hot-recipient (all → 1 acct) | 19.7 ms | 20.4 ms | **1.0×** | DAG depth = 9500 (serial chain); STM cannot help — matches our `contention` finding |

**When transfers are arithmetic-only (state already in RAM, ~free):** serial 0.02 ms vs parallel
1.2 ms — **Block-STM is net overhead**. Parallelism only pays when the contended work is expensive.

Correctness: deferred-parallel reproduces serial balances AND fee total on every workload (the
determinism consensus needs).

### Honest notes
- The 8.5× on `pool(10k)` is the load-bearing result: that IS the payment lane's real pattern, and
  at 10k TPS the execute phase (lean_state: 25 ms serial) is big enough for it to matter — Block-STM
  takes it to ~3 ms. Combined with sharded-JMT root (~11 ms), execute stops being the bottleneck;
  persistence (191–311 ms) becomes the sole remaining lever.
- Deferred vs naive fee-recipient measured near-identical HERE because the prototype uses atomic
  balances — a single hot atomic isn't a conflict. In grevm's *true* MVCC-STM it is (every tx would
  read/write the recipient version → cascade re-execution), which is exactly why grevm's
  NoRewardHandler defers it. Our prototype validates the DAG+parallel win; the deferral trick's
  necessity is inherited from grevm's design, not re-measured here.
- This is a standalone prototype (like lean_state / jmt_bench), NOT wired into the payment EL. It
  proves the technique + quantifies the win on our workload before the consensus-critical graft.

### Bottom line for the payment lane
Block-STM is a real ~8.5× win on the lane's execute phase at 10k-TPS scale — worth doing — but the
payment lane's dominant cost is **persistence**, then **root**, then execute. Priority order for the
lean lane: (1) persistence (batch fsync / faster disk — measured 55× swing home-disk vs NVMe),
(2) sharded-JMT or reth sparse-trie root (~11 ms), (3) Block-STM execute (~3 ms). grevm matters most
on the EVM lane (diverse contracts) and as the builder accelerator; on the payment lane it's the
third lever, not the first.

## INTEGRATION STATUS (overnight 2026-07-16, branch gravity-payment-lane)

Stages 1-2 DONE, committed, tree green. Stage 3 (live wiring) blocked on a documented
architectural wall — NOT a grevm defect.

### Done + tested (commits: grevm-add, block-stm-green 5678e7a, differential d3de0b4)
- grevm added; **dependency layer resolves + compiles clean** — revm 40.0.3 / alloy-evm 0.36
  already match grevm's exact pins. The feared multi-week dep wall DOES NOT EXIST.
- `crates/evm/src/parallel.rs`: `parallel_execute_block()` + `sequential_execute_block()`
  wrapping grevm's `Scheduler` over Arc's revm-40 types. `parallel_enabled()` reads
  `ARC_PARALLEL_EVM`.
- `crates/evm/tests/blockstm.rs` (integration test — compiles the lib normally, sidesteps the
  pre-existing bit-rotted `evm.rs` #[cfg(test)] module): **3/3 GREEN** —
  executes 200 Arc transfers (all succeed), deterministic across runs, and DIFFERENTIAL
  parallel==sequential on a mixed disjoint+hot-recipient block (identical per-tx gas AND final
  balances/nonces every account = the consensus-correctness property).

### Stage 3 blocker (the live payment EL actually using grevm) — architectural, precise
grevm needs an **owned `DB: DatabaseRef + Send + Sync`** (ParallelState<DB> takes it by value).
Arc's `ArcBlockExecutor` runs inside alloy-evm's tx-by-tx `BlockExecutor`, which holds the state
as `&mut State<DB>` *inside* the constructed `Evm`. Three concrete gaps:
1. **DB extraction**: can't hand grevm an owned DatabaseRef from behind `self.evm.db_mut()`
   (`&mut State<DB>`). `State<DB>: DatabaseRef` only when `DB: DatabaseRef`, and it's borrowed
   by the Evm anyway. Overriding `BlockExecutor::execute_block` (which DOES receive the whole
   tx iterator) still can't produce the owned DatabaseRef grevm requires.
2. **Receipts**: Arc's `receipt_builder.build_receipt` wants a per-tx `&EvmState` delta; grevm
   returns `Vec<ExecutionResult>` + one combined `BundleState`, not per-tx state deltas — so
   receipts must be rebuilt from `ExecutionResult` alone.
3. **Arc fee accounting**: per-tx `tx_gas_used`→EMA/base-fee accounting and pre-tx gas checks
   are interleaved with serial execution; must be reconstructed post-hoc from grevm results.

### The correct path (gravity's architecture) — multi-day, consensus-critical
gravity solved exactly this with a SEPARATE executor path, not an override:
`fn parallel_executor(db: DB) -> Box<dyn ParallelExecutor>` constructed from the **raw db before
Evm wrapping** (`GrevmExecutor::new(chain_spec, cfg, db)` → `ParallelState<DB>`), selected at the
node's block-execution wiring. For Arc that means: (a) a new `ArcParallelExecutor` built from the
raw `DatabaseRef`; (b) rebuild receipts + Arc fee accounting from grevm results; (c) route the
payment-lane's block execution (payload build + validation) to it behind `--arc.parallel-evm`;
(d) differential-replay on a live testnet. `parallel_execute_block()` (done, tested) is the engine
this path calls. Estimate: the receipts/fee reconstruction + wiring + validation is the bulk of
the earlier 1.5-2 week estimate; the engine + dep layer (the parts feared hardest) are DONE.

## WIRING PROGRESS (2026-07-18, live-EL integration attempt)
- execute_block override SEAM committed (serial-only, behavior-identical, flag-gated). 3/3 tests green.
- PROBE: adding `+ DatabaseRef` to the executor's DB bound does NOT break external callers
  (the concrete payment-EL DB IS a DatabaseRef — the crux is satisfiable!). It breaks only
  INTERNAL method resolution: compute_gas_values / compute_gas_values_legacy /
  validate_extra_data_base_fee live in the sibling impl block (executor.rs:206) with the looser
  `DB: StateDB` bound. FIX = propagate `DB: StateDB + DatabaseRef` to BOTH impl blocks that carry
  `E`: line 206 (`impl ArcBlockExecutor`) and line 351 (`impl BlockExecutor`). Block at 120 is
  Spec-only, untouched. This is bounded (2 impl headers), not a wall.
- REMAINING after bound propagation: fill execute_block parallel branch — build Vec<TxEnv> from
  into_parts(), call parallel_execute_block(self.evm.cfg_env(), self.evm.block(), txs, <dbref of
  self.evm.db()>), then commit grevm bundle + build receipts (the receipt build from
  Vec<ExecutionResult> vs Arc's per-tx-state ReceiptBuilderCtx is the last real unknown).
