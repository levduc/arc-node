//! Speculative payload prebuilding (`ARC_SPECULATIVE_BUILD=1`, default OFF).
//!
//! MEASURED MOTIVATION (experiments/reth-fork/MISSION-EL.md): the proposer's payload build is
//! ~98-240 ms of real, pre-computable compute (57.6% tx execution + 40.2% state root at full 50M
//! blocks) sitting on the height's critical path — while the EL idles 350-900 ms in the vote gap
//! between `newPayload(N)` returning VALID and the decide-round forkchoice. Hiding the build in
//! that gap is worth ~20% of block cadence at every size; at 50M it takes the height from 551 ms
//! to ~454 ms, under Arc's 500 ms target.
//!
//! HOW IT WORKS — entirely EL-side, no consensus changes, and (perhaps surprisingly) no reth fork:
//!
//! * reth's engine tree already publishes a newPayload'd block as the PENDING block whenever its
//!   parent is the canonical head (`insert_block_or_payload` -> `set_pending_block`), which is
//!   Arc's situation at every height. `state_by_block_hash` resolves the pending state, so the
//!   production build function can run against block N before N is canonical.
//! * A task polls the pending header (25 ms; no public subscription exists for the pending slot).
//!   When a new pending block N appears it predicts the next height's payload attributes and runs
//!   the REAL `arc_ethereum_payload` on parent N, stashing the result.
//! * `ArcEthereumPayloadBuilder::try_build` first checks the stash: if EVERY attribute matches the
//!   real request, the prebuilt payload is served (`BuildOutcome::Freeze`); otherwise it falls
//!   through to the normal build. A miss costs nothing but the idle CPU already spent.
//!
//! ATTRIBUTE PREDICTION (from CL source, read-only — crates/eth-engine + malachite-app):
//! * `timestamp = max(parent.timestamp, now_secs)` — the CL's own formula
//!   (`generate_payload_with_retry`); the payment lane copies the EVM lane's timestamp, and the
//!   lanes advance in lockstep, so the payment parent's timestamp equals the EVM parent's. The
//!   prediction misses only when the wall-clock second rolls over inside the vote gap.
//! * `fee_recipient` and `prev_randao` are LEARNED from the last real payload-attributes this
//!   node received (recorded in `try_build`). The CL sends a fixed fee recipient per validator
//!   and always-zero randao, but we cache rather than hardcode CL behaviour. Until the first
//!   real request after boot, speculation is skipped.
//! * `parent_beacon_block_root = N.hash` — Arc's convention.
//! * `withdrawals = Some([])` — always, V3.
//!
//! WHY THE POOL MUST BE PRE-FILTERED (correctness of CONTENT, found at design time): during the
//! vote gap the pool still contains N's transactions (pruning happens on canonicalization). If
//! the build loop sees a stale tx it fails nonce-check and `mark_invalid` REMOVES ALL DEPENDENT
//! TRANSACTIONS from the iterator — so sender chains would be dropped wholesale and the
//! speculative block would come out near-empty. `FilteredBest` skips exactly N's tx hashes so the
//! iterator starts each sender at the right nonce, matching what the real post-prune build sees.
//!
//! Consensus safety: the stashed payload was built by the production build function on the real
//! parent state; serving it is indistinguishable from having built it at request time. A wrong
//! PREDICTION cannot corrupt anything — it just misses and the normal path runs. The offline
//! `build_gate` example asserts prebuilt-vs-fresh byte equality (and the miss path) for the same
//! parent/attributes/transactions.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy_primitives::{Address, B256};
use metrics::counter;
use reth_basic_payload_builder::{BuildArguments, BuildOutcome, PayloadConfig};
use reth_chainspec::EthereumHardforks;
use reth_ethereum_engine_primitives::{EthBuiltPayload, EthPayloadAttributes};
use reth_ethereum_payload_builder::EthereumBuilderConfig;
use reth_evm::{ConfigureEvm, NextBlockEnvAttributes};
use reth_ethereum_primitives::{EthPrimitives, TransactionSigned};
use reth_node_api::PayloadBuilderError;
use reth_primitives_traits::SealedHeader;
use reth_chainspec::ChainSpecProvider;
use reth_storage_api::{BlockReaderIdExt, StateProviderFactory};
use reth_transaction_pool::{
    error::InvalidPoolTransactionError, BestTransactions, PoolTransaction, TransactionPool,
    ValidPoolTransaction,
};
use tracing::{debug, info, warn};

/// Poll interval for the pending-block watcher. The vote gap is 350-900 ms; 25 ms granularity
/// costs at most ~5% of the window it is trying to hide the build in.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Master switch. Read once; default OFF, exactly like `ARC_PARALLEL_TRANSFERS`.
pub fn speculative_build_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("ARC_SPECULATIVE_BUILD").as_deref() == Ok("1"))
}

/// A prebuilt payload plus every attribute the prediction committed to. `try_build` serves it
/// only if ALL of them equal the real request's.
pub struct SpeculativePayload {
    pub parent_hash: B256,
    pub timestamp: u64,
    pub fee_recipient: Address,
    pub prev_randao: B256,
    pub parent_beacon_block_root: Option<B256>,
    pub payload: EthBuiltPayload,
}

fn slot() -> &'static Mutex<Option<SpeculativePayload>> {
    static SLOT: OnceLock<Mutex<Option<SpeculativePayload>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Attributes learned from the last REAL payload request this node served. Written by
/// `try_build` on every request; read by the predictor.
struct LearnedAttrs {
    fee_recipient: Address,
    prev_randao: B256,
}

fn learned() -> &'static Mutex<Option<LearnedAttrs>> {
    static LEARNED: OnceLock<Mutex<Option<LearnedAttrs>>> = OnceLock::new();
    LEARNED.get_or_init(|| Mutex::new(None))
}

/// Record the attributes of a real payload request (called from `try_build` on EVERY request,
/// hit or miss) so the next speculation predicts them.
pub fn record_real_attributes(attrs: &EthPayloadAttributes) {
    let mut g = learned().lock().expect("learned attrs lock");
    *g = Some(LearnedAttrs {
        fee_recipient: attrs.suggested_fee_recipient,
        prev_randao: attrs.prev_randao,
    });
}

/// `try_build`'s first stop: serve the stashed payload iff every predicted attribute matches the
/// real request. Consumes the stash on hit; leaves it for the next height's overwrite on miss.
pub fn take_matching(
    config: &PayloadConfig<EthPayloadAttributes>,
) -> Option<EthBuiltPayload> {
    if !speculative_build_enabled() {
        return None;
    }
    let mut g = slot().lock().expect("speculative slot lock");
    let Some(spec) = g.as_ref() else {
        counter!("arc_speculative_build_outcome_total", "outcome" => "miss_empty").increment(1);
        return None;
    };
    let attrs = &config.attributes;
    if spec.parent_hash != config.parent_header.hash() {
        counter!("arc_speculative_build_outcome_total", "outcome" => "miss_parent").increment(1);
        return None;
    }
    if spec.timestamp != attrs.timestamp {
        counter!("arc_speculative_build_outcome_total", "outcome" => "miss_timestamp").increment(1);
        return None;
    }
    if spec.fee_recipient != attrs.suggested_fee_recipient
        || spec.prev_randao != attrs.prev_randao
        || spec.parent_beacon_block_root != attrs.parent_beacon_block_root
        || attrs.withdrawals.as_ref().is_none_or(|w| !w.is_empty())
    {
        counter!("arc_speculative_build_outcome_total", "outcome" => "miss_other").increment(1);
        return None;
    }
    counter!("arc_speculative_build_outcome_total", "outcome" => "hit").increment(1);
    let spec = g.take().expect("checked above");
    info!(target: "payload_builder",
        parent = %spec.parent_hash, timestamp = spec.timestamp,
        "(arc) serving SPECULATIVE prebuilt payload");
    Some(spec.payload)
}

/// Skip exactly the pending block's transactions. See module docs: without this, stale txs
/// nonce-fail and `mark_invalid` drops each sender's whole remaining chain, so the speculative
/// block would be near-empty and never match what the real (post-prune) build produces.
struct FilteredBest<T: PoolTransaction> {
    inner: Box<dyn BestTransactions<Item = std::sync::Arc<ValidPoolTransaction<T>>>>,
    skip: HashSet<B256>,
}

impl<T: PoolTransaction> Iterator for FilteredBest<T> {
    type Item = std::sync::Arc<ValidPoolTransaction<T>>;
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let tx = self.inner.next()?;
            if self.skip.contains(tx.hash()) {
                continue;
            }
            return Some(tx);
        }
    }
}

impl<T: PoolTransaction> BestTransactions for FilteredBest<T> {
    fn mark_invalid(&mut self, tx: &Self::Item, err: InvalidPoolTransactionError) {
        self.inner.mark_invalid(tx, err)
    }
    fn no_updates(&mut self) {
        self.inner.no_updates()
    }
    fn set_skip_blobs(&mut self, skip: bool) {
        self.inner.set_skip_blobs(skip)
    }
}

/// CL timestamp formula, from `malachite-app/src/payload.rs`: non-decreasing wall clock.
fn predict_timestamp(parent_timestamp: u64) -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before unix epoch")
        .as_secs();
    parent_timestamp.max(now)
}

/// Run one speculative build on top of `parent` and stash the result. Public so the offline
/// `build_gate` can drive the exact production speculation path.
#[allow(clippy::too_many_arguments)]
pub fn speculate_once<EvmConfig, Client, Pool>(
    evm_config: EvmConfig,
    client: Client,
    pool: Pool,
    builder_config: EthereumBuilderConfig,
    parent: SealedHeader,
    skip_hashes: HashSet<B256>,
    fee_recipient: Address,
    prev_randao: B256,
    timestamp: u64,
) -> Result<(), PayloadBuilderError>
where
    EvmConfig: ConfigureEvm<Primitives = EthPrimitives, NextBlockEnvCtx = NextBlockEnvAttributes>,
    Client: StateProviderFactory + ChainSpecProvider<ChainSpec: EthereumHardforks> + Clone,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TransactionSigned>>,
{
    let attributes = EthPayloadAttributes {
        timestamp,
        prev_randao,
        suggested_fee_recipient: fee_recipient,
        withdrawals: Some(vec![]),
        // Arc convention: parent execution block hash.
        parent_beacon_block_root: Some(parent.hash()),
        slot_number: None,
    };
    let parent_hash = parent.hash();
    let config = PayloadConfig {
        parent_header: std::sync::Arc::new(parent),
        attributes: attributes.clone(),
        payload_id: Default::default(),
        parent_block_info: None,
    };
    let args = BuildArguments::new(Default::default(), None, None, config, Default::default(), None);

    let pool_for_iter = pool.clone();
    let outcome = crate::payload::arc_ethereum_payload(
        evm_config,
        client,
        pool,
        builder_config,
        None,
        args,
        move |best_attrs| {
            Box::new(FilteredBest {
                inner: pool_for_iter.best_transactions_with_attributes(best_attrs),
                skip: skip_hashes,
            })
        },
    )?;

    let payload = match outcome {
        BuildOutcome::Better { payload, .. } => payload,
        BuildOutcome::Freeze(payload) => payload,
        other => {
            debug!(target: "payload_builder", ?other, "(arc) speculative build produced no payload");
            counter!("arc_speculative_build_outcome_total", "outcome" => "build_no_payload")
                .increment(1);
            return Ok(());
        }
    };
    counter!("arc_speculative_build_outcome_total", "outcome" => "built").increment(1);
    debug!(target: "payload_builder",
        parent = %parent_hash, timestamp, txs = payload.block().body().transactions.len(),
        "(arc) speculative payload stashed");
    *slot().lock().expect("speculative slot lock") = Some(SpeculativePayload {
        parent_hash,
        timestamp,
        fee_recipient,
        prev_randao,
        parent_beacon_block_root: Some(parent_hash),
        payload,
    });
    Ok(())
}

/// Spawn the pending-block watcher. Called from `build_payload_builder` when the flag is on;
/// builds one speculation per new pending block, on a blocking thread (the build is CPU work).
pub fn spawn_speculative_watcher<EvmConfig, Client, Pool>(
    evm_config: EvmConfig,
    client: Client,
    pool: Pool,
    builder_config: EthereumBuilderConfig,
) where
    EvmConfig: ConfigureEvm<Primitives = EthPrimitives, NextBlockEnvCtx = NextBlockEnvAttributes>
        + Clone
        + 'static,
    Client: StateProviderFactory
        + ChainSpecProvider<ChainSpec: EthereumHardforks>
        + BlockReaderIdExt<Block = reth_ethereum_primitives::Block>
        + Clone
        + Send
        + Sync
        + 'static,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TransactionSigned>>
        + Clone
        + 'static,
{
    info!(target: "payload_builder", "(arc) speculative prebuild watcher starting (ARC_SPECULATIVE_BUILD=1)");
    std::thread::Builder::new()
        .name("arc-speculative-build".into())
        .spawn(move || {
            let mut last_parent: Option<B256> = None;
            loop {
                std::thread::sleep(POLL_INTERVAL);
                // The pending block is the newPayload'd-but-not-yet-canonical block N that reth
                // published because its parent was the canonical head.
                let pending = match client.pending_block() {
                    Ok(Some(b)) => b,
                    Ok(None) => continue,
                    Err(err) => {
                        warn!(target: "payload_builder", %err, "(arc) speculative: pending_block failed");
                        continue;
                    }
                };
                let hash = pending.hash();
                if last_parent == Some(hash) {
                    continue;
                }
                // Need learned attrs before we can predict; until the first real request, skip.
                let Some((fee_recipient, prev_randao)) = learned()
                    .lock()
                    .expect("learned attrs lock")
                    .as_ref()
                    .map(|l| (l.fee_recipient, l.prev_randao))
                else {
                    continue;
                };
                last_parent = Some(hash);
                let skip: HashSet<B256> =
                    pending.body().transactions.iter().map(|tx| *tx.hash()).collect();
                let parent = SealedHeader::new(pending.header().clone(), hash);
                let timestamp = predict_timestamp(parent.timestamp);
                let t = std::time::Instant::now();
                if let Err(err) = speculate_once(
                    evm_config.clone(),
                    client.clone(),
                    pool.clone(),
                    builder_config.clone(),
                    parent,
                    skip,
                    fee_recipient,
                    prev_randao,
                    timestamp,
                ) {
                    warn!(target: "payload_builder", %err, "(arc) speculative build failed");
                } else {
                    debug!(target: "payload_builder", parent = %hash,
                        elapsed_ms = t.elapsed().as_millis() as u64,
                        "(arc) speculative build finished");
                }
            }
        })
        .expect("spawn arc-speculative-build thread");
}
