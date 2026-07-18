//! Block-STM execution for the payment lane, via grevm (Galxe's DAG + Block-STM parallel EVM).
//!
//! grevm and Arc's fork are on the SAME revm/alloy-evm versions (revm 40.0.3, alloy-evm 0.36),
//! so grevm's `Scheduler` binds directly to our EVM types. This module exposes a **block-level**
//! parallel execution entry point: give it the block env + the ordered tx list + a
//! `DatabaseRef` over the pre-state, and it returns per-tx results plus the mutated parallel
//! state (from which a `BundleState` is taken and applied to reth's `State<DB>` for the state
//! root, exactly as the serial path does).
//!
//! This is the execution engine; wiring it into the block-production/validation path (so the
//! live payment EL uses it) is done by the executor factory behind a flag — see
//! `ArcEvmConfig` and `docs/block-stm-for-arc.md`. Kept behind `--arc.parallel-evm` /
//! `ARC_PARALLEL_EVM`, serial `ArcBlockExecutor` remains the default until differentially
//! validated on a live testnet.

use grevm::{ParallelState, Scheduler};
use revm::context::result::ExecutionResult;
use revm::DatabaseRef;
use revm::context::{BlockEnv, CfgEnv, TxEnv};
use std::sync::Arc;

/// Whether Block-STM is enabled for execution (env flag; the node CLI can also set this).
pub fn parallel_enabled() -> bool {
    std::env::var("ARC_PARALLEL_EVM").is_ok()
}

/// Execute an ordered block of transactions in parallel (Block-STM) over `db`.
///
/// Returns the per-transaction [`ExecutionResult`]s (in block order) and the mutated
/// [`ParallelState`], whose `BundleState` the caller applies to reth state for the state root.
/// Falls back to grevm's own sequential path for tiny blocks internally.
pub fn parallel_execute_block<DB>(
    cfg: CfgEnv,
    block: BlockEnv,
    txs: Vec<TxEnv>,
    db: DB,
) -> Result<(Vec<ExecutionResult>, ParallelState<DB>), grevm::GrevmError<DB::Error>>
where
    DB: DatabaseRef + Send + Sync,
    DB::Error: Clone + Send + Sync + 'static,
{
    // with_bundle_update = true so we can take a BundleState afterwards; metrics off.
    let state = ParallelState::new(db, true, false);
    // with_hints = false: grevm discovers dependencies dynamically (matches gravity-reth's call).
    // custom_precompiles = None here; Arc precompiles are threaded in by the executor-factory
    // integration (follow-up), not this raw entry point.
    let scheduler = Scheduler::new(cfg, block, Arc::new(txs), state, false, None);
    scheduler.parallel_execute(None)?;
    Ok(scheduler.take_result_and_state())
}

/// Sequential execution of the same block via grevm's own fallback path. Used to
/// differentially validate the parallel result (parallel MUST equal sequential — the
/// Block-STM correctness property, and our consensus-correctness proof).
pub fn sequential_execute_block<DB>(
    cfg: CfgEnv,
    block: BlockEnv,
    txs: Vec<TxEnv>,
    db: DB,
) -> Result<(Vec<ExecutionResult>, ParallelState<DB>), grevm::GrevmError<DB::Error>>
where
    DB: DatabaseRef + Send + Sync,
    DB::Error: Clone + Send + Sync + 'static,
{
    let state = ParallelState::new(db, true, false);
    let scheduler = Scheduler::new(cfg, block, Arc::new(txs), state, false, None);
    scheduler.fallback_sequential()?;
    Ok(scheduler.take_result_and_state())
}
