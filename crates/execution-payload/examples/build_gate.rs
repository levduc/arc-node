//! BUILD gate: drive the REAL payload builder offline and digest what it produces.
//!
//!   cargo run --release -p arc-execution-payload --example build_gate
//!   ARC_PARALLEL_TRANSFERS=1 cargo run --release -p arc-execution-payload --example build_gate
//!
//! Both invocations must print IDENTICAL digests (the outer diff is the gate, exactly like
//! `parallel_transfer_bench`).
//!
//! WHY THIS EXISTS: the existing offline gate only covers the VALIDATION path
//! (`ArcBlockExecutor` driven the way `newPayload` drives it). Four consensus bugs in this
//! project, three of them invisible to state-only comparison — and the next planned change is
//! SPECULATIVE PREBUILDING, which lives in the BUILDER. The builder is exactly where a wrong
//! speculative state becomes a wrong block, and nothing covered it offline until now.
//!
//! What this drives is the production path itself — `arc_ethereum_payload`, the same function
//! both `ArcEthereumPayloadBuilder::try_build` and the invalid-tx-filtering wrapper call — over a
//! REAL MDBX-backed provider (`create_test_provider_factory_with_chain_spec` + `insert_genesis`,
//! which writes hashed state AND the trie, so state roots are real), with transactions injected
//! through the `best_txs` closure seam (the `_pool` argument is unused in the build function).
//!
//! Checks, in order of what they catch:
//!   1. DETERMINISM — the same build run twice in-process must produce the same block hash
//!      (catches map-iteration-order nondeterminism in the builder).
//!   2. FLAG EQUIVALENCE (outer diff) — stock vs ARC_PARALLEL_TRANSFERS=1 must build the
//!      byte-identical block: the fast path routes through
//!      `execute_transaction_with_commit_condition` on the BUILDER path too.
//!   3. Arithmetic sanity — gas_used == 21k * txs, all offered txs included.
//!
//! When speculative prebuild lands, this gate grows a third leg: prebuilt-payload vs
//! fresh-build byte equality for the same (parent, fee recipient, tx set).

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use alloy_consensus::TxEip1559;
use alloy_primitives::{Address, Signature, TxKind, B256, U256};
use arc_evm::{ArcEvmConfig, ArcEvmFactory};
use arc_execution_config::chainspec::LOCAL_DEV;
use arc_execution_payload::payload::arc_ethereum_payload;
use reth_basic_payload_builder::{BuildArguments, BuildOutcome, PayloadConfig};
use reth_ethereum_payload_builder::EthereumBuilderConfig;
use alloy_consensus::transaction::Recovered;
use reth_db_common::init::init_genesis;
use reth_provider::providers::BlockchainProvider;
use reth_provider::test_utils::create_test_provider_factory_with_chain_spec;
use reth_transaction_pool::{
    error::InvalidPoolTransactionError,
    identifier::{SenderId, TransactionId},
    noop::NoopTransactionPool,
    BestTransactions, EthPooledTransaction, TransactionOrigin, ValidPoolTransaction,
};

const TIP: u128 = 1_000_000_000;
const MAX_FEE: u128 = 40_000_000_000_000;

/// Fixed-order iterator over pre-validated transactions — the gate's stand-in for the pool's
/// best-transactions stream. Order is the vector order, so the built block is deterministic.
struct StaticBest {
    txs: VecDeque<Arc<ValidPoolTransaction<EthPooledTransaction>>>,
    invalid: usize,
}

impl Iterator for StaticBest {
    type Item = Arc<ValidPoolTransaction<EthPooledTransaction>>;
    fn next(&mut self) -> Option<Self::Item> {
        self.txs.pop_front()
    }
}

impl BestTransactions for StaticBest {
    fn mark_invalid(&mut self, _tx: &Self::Item, _err: InvalidPoolTransactionError) {
        self.invalid += 1;
    }
    fn no_updates(&mut self) {}
    fn set_skip_blobs(&mut self, _skip: bool) {}
}

fn main() {
    let arc_spec = LOCAL_DEV.clone();
    let reth_spec = Arc::new(arc_spec.inner.clone());

    // Real MDBX-backed provider seeded with the exact localdev genesis (accounts, ProtocolConfig
    // storage incl. FeeParams + gas limit slots) — hashed tables AND trie written, roots are real.
    let factory = create_test_provider_factory_with_chain_spec(reth_spec.clone());
    let genesis_root = init_genesis(&factory).expect("init_genesis");
    let client = BlockchainProvider::new(factory).expect("blockchain provider");
    // Anchor on the header the DB actually stores, NOT the chainspec's sealed header: observed
    // (2026-08-09) that init_genesis computes a different state root than the chainspec header
    // declares for localdev (0xbc32... vs 0x0c6b...). For the gate only self-consistency matters
    // (parent header <-> DB state); the discrepancy itself is noted in MISSION-EL.md.
    let parent = reth_provider::HeaderProvider::sealed_header(&client, 0)
        .expect("read genesis header")
        .expect("genesis header missing");
    // Known localdev quirk (2026-08-09): the chainspec genesis header DECLARES state root
    // 0xbc32..., but the root computed over the inserted alloc is 0x0c6b... (both insert_genesis
    // and init_genesis agree on the computed one). The builder derives child roots from the DB,
    // so the build is self-consistent either way; we print both so a change in either shows up
    // in the gate diff.
    println!("  parent declaredRoot {:#x}", parent.state_root);
    println!("  parent computedRoot {genesis_root:#x}");

    // Senders = the prefunded localdev dev accounts (EOAs with real balances). Deterministic order.
    let mut senders: Vec<(Address, U256)> = reth_spec
        .genesis()
        .alloc
        .iter()
        .filter(|(_, a)| a.code.is_none() && a.balance > U256::from(10u128.pow(18)))
        .map(|(addr, a)| (*addr, a.balance))
        .collect();
    senders.sort_by_key(|(a, _)| *a);
    assert!(!senders.is_empty(), "no prefunded EOAs in localdev genesis alloc");

    // Enough transfers to be a real block, few enough to fit any sane gas limit.
    let per_sender = (1_000 / senders.len()).max(2);
    let sig = Signature::new(U256::from(1), U256::from(1), false);
    let mut pool_txs = Vec::new();
    for (s_idx, (sender, _)) in senders.iter().enumerate() {
        for nonce in 0..per_sender as u64 {
            let tx = TxEip1559 {
                chain_id: reth_spec.chain().id(),
                nonce,
                gas_limit: 21_000,
                max_fee_per_gas: MAX_FEE,
                max_priority_fee_per_gas: TIP,
                to: TxKind::Call(Address::left_padding_from(
                    &(0x1000u64 + s_idx as u64).to_be_bytes(),
                )),
                value: U256::from(1_000u64),
                access_list: Default::default(),
                input: Default::default(),
            };
            let signed =
                reth_ethereum_primitives::TransactionSigned::new_unhashed(tx.into(), sig);
            let encoded_len = alloy_eips::eip2718::Encodable2718::encoded_2718(&signed).len();
            let recovered = Recovered::new_unchecked(signed, *sender);
            let pooled = EthPooledTransaction::new(recovered, encoded_len);
            pool_txs.push(Arc::new(ValidPoolTransaction {
                transaction: pooled,
                transaction_id: TransactionId::new(SenderId::from(s_idx as u64), nonce),
                propagate: false,
                timestamp: Instant::now(),
                origin: TransactionOrigin::Local,
                authority_ids: None,
            }));
        }
    }
    let n_offered = pool_txs.len();

    let evm_config = ArcEvmConfig::new(reth_ethereum::evm::EthEvmConfig::new_with_evm_factory(
        arc_spec.clone(),
        ArcEvmFactory::new(arc_spec.clone()),
    ));

    let build = |label: &str| -> (B256, B256, B256, u64, usize) {
        let attributes = reth_ethereum_engine_primitives::EthPayloadAttributes {
            timestamp: parent.timestamp + 2,
            prev_randao: B256::ZERO,
            suggested_fee_recipient: Address::repeat_byte(0xff),
            withdrawals: Some(vec![]),
            parent_beacon_block_root: Some(parent.hash()), // Arc convention
            slot_number: None,
        };
        let config = PayloadConfig {
            parent_header: Arc::new(parent.clone()),
            attributes,
            payload_id: Default::default(),
            parent_block_info: None,
        };
        let args = BuildArguments::new(Default::default(), None, None, config, Default::default(), None);
        let best = StaticBest { txs: pool_txs.iter().cloned().collect(), invalid: 0 };
        let outcome = arc_ethereum_payload(
            evm_config.clone(),
            client.clone(),
            NoopTransactionPool::default(),
            EthereumBuilderConfig::new(),
            None,
            args,
            |_attrs| Box::new(best) as Box<dyn BestTransactions<Item = _>>,
        )
        .expect("build failed");
        let payload = match outcome {
            BuildOutcome::Better { payload, .. } => payload,
            BuildOutcome::Freeze(payload) => payload,
            other => panic!("unexpected outcome for {label}: {other:?}"),
        };
        let block = payload.block();
        (
            block.hash(),
            block.header().state_root,
            block.header().receipts_root,
            block.header().gas_used,
            block.body().transactions.len(),
        )
    };

    let (h1, sr1, rr1, gas1, n1) = build("first");
    let (h2, ..) = build("second");
    assert_eq!(h1, h2, "NONDETERMINISTIC BUILD: same inputs, different block hash");
    assert_eq!(gas1, 21_000 * n1 as u64, "gas != 21k per transfer");
    assert_eq!(n1, n_offered, "builder dropped {} of {} offered txs", n_offered - n1, n_offered);

    println!("build_gate — REAL arc_ethereum_payload over MDBX provider, {n1} transfers");
    println!("  built  blockHash    {h1:#x}");
    println!("  built  stateRoot    {sr1:#x}");
    println!("  built  receiptsRoot {rr1:#x}");
    println!("  built  gasUsed      {gas1}");
    println!("  determinism         OK (two in-process builds identical)");
}
