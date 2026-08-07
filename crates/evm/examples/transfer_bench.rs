//! Profiling bench for the payment-lane workload: a full 1-Ggas block of plain native
//! transfers (21k gas each) through the REAL `ArcBlockExecutor` + Arc EVM on in-memory
//! state — measures the serial per-tx execution cost that bounds payment-lane cadence.
//!
//!   cargo run --release -p arc-evm --example transfer_bench
//!
//! Context (branch blockstm-native-transfers): fleet-measured exec is ~379ms for a ~34k-tx
//! payment block, replayed ~5x/height — THE cadence bottleneck (state root 0.8ms, persist
//! 169ms). This bench isolates the executor's share of that cost on cache-hot state, which
//! decides whether the fix is a cheaper serial path, parallel execution, or both.

use std::time::Instant;

use alloy_consensus::{transaction::Recovered, TxEip1559};
use alloy_primitives::{address, Address, Signature, TxKind, U256};
use arc_evm::executor::ArcBlockExecutor;
use arc_evm::{ArcEvmConfig, ArcEvmFactory};
use arc_execution_config::chainspec::LOCAL_DEV;
use reth_chainspec::EthChainSpec;
use reth_evm::{ConfigureEvm, EvmEnv};
use reth_evm::block::BlockExecutor;
use revm::{
    context::{BlockEnv, CfgEnv},
    database::InMemoryDB,
    state::AccountInfo,
};
use revm_primitives::{hardfork::SpecId, StorageKey, StorageValue};

const N_TX: usize = 47_618; // 1 Ggas / 21k = a full payment block
const N_ACCOUNTS: usize = 16_000; // closed spammer-like account set
const GAS_1G: u64 = 1_000_000_000;
const BASEFEE: u64 = 20_000_000_000; // 20 gwei (demo economics)
const MAX_FEE: u128 = 40_000_000_000_000; // spammer signs 40k gwei
const TIP: u128 = 1_000_000_000;

fn addr(i: usize) -> Address {
    let mut b = [0u8; 20];
    b[0] = 0xAA;
    b[12..20].copy_from_slice(&(i as u64).to_be_bytes());
    Address::from(b)
}

fn main() {
    let chain_spec = LOCAL_DEV.clone();
    let mut db = InMemoryDB::default();

    // genesis alloc (accounts + code + storage), same as the node's init
    for (a, acct) in &chain_spec.genesis().alloc {
        let code = acct.code.clone().map(revm::state::Bytecode::new_raw);
        let info = AccountInfo {
            balance: acct.balance,
            nonce: acct.nonce.unwrap_or_default(),
            code_hash: code
                .as_ref()
                .map(|c| c.hash_slow())
                .unwrap_or(revm_primitives::KECCAK_EMPTY),
            code,
            ..Default::default()
        };
        db.insert_account_info(*a, info);
        if let Some(storage) = &acct.storage {
            for (k, v) in storage {
                db.insert_account_storage(
                    *a,
                    StorageKey::from_be_bytes(k.0),
                    StorageValue::from_be_bytes(v.0),
                )
                .expect("genesis storage");
            }
        }
    }

    // Raise the on-chain ProtocolConfig blockGasLimit to 1 Ggas — pre-execution validates
    // the block header gas limit against it (same slot the demo genesis patcher writes).
    db.insert_account_storage(
        address!("3600000000000000000000000000000000000001"),
        StorageKey::from_str_radix(
            "668f09ce856848ead6cb1ddee963f15ef833cea8958030868f867aec84385203",
            16,
        )
        .unwrap(),
        StorageValue::from(GAS_1G),
    )
    .unwrap();

    // closed set of funded EOAs paying each other (the measured fleet workload)
    for i in 0..N_ACCOUNTS {
        db.insert_account_info(
            addr(i),
            AccountInfo {
                balance: U256::from(10u128.pow(21)),
                ..Default::default()
            },
        );
    }

    let sig = Signature::new(U256::from(1), U256::from(1), false);
    let t = Instant::now();
    let txs: Vec<_> = (0..N_TX)
        .map(|i| {
            let s = i % N_ACCOUNTS;
            let tx = TxEip1559 {
                chain_id: chain_spec.chain_id(),
                nonce: (i / N_ACCOUNTS) as u64,
                gas_limit: 21_000,
                max_fee_per_gas: MAX_FEE,
                max_priority_fee_per_gas: TIP,
                to: TxKind::Call(addr((s + 1) % N_ACCOUNTS)),
                value: U256::from(1u64),
                access_list: Default::default(),
                input: Default::default(),
            };
            let signed =
                reth_ethereum_primitives::TransactionSigned::new_unhashed(tx.into(), sig);
            Recovered::new_unchecked(signed, addr(s))
        })
        .collect();
    println!("built {N_TX} txs in {:?}", t.elapsed());

    let cfg_env = CfgEnv::new()
        .with_chain_id(chain_spec.chain_id())
        .with_spec_and_mainnet_gas_params(SpecId::PRAGUE);
    let evm_env = EvmEnv {
        cfg_env,
        block_env: BlockEnv {
            basefee: BASEFEE,
            gas_limit: GAS_1G,
            ..Default::default()
        },
    };
    let evm_config = ArcEvmConfig::new(reth_ethereum::evm::EthEvmConfig::new_with_evm_factory(
        chain_spec.clone(),
        ArcEvmFactory::new(chain_spec.clone()),
    ));
    let mut state = revm::database::State::builder()
        .with_database(&mut db)
        .with_bundle_update() // the live node tracks bundle state for persistence
        .build();
    let evm = evm_config.evm_with_env(&mut state, evm_env);
    let ctx = reth_evm::eth::EthBlockExecutionCtx {
        parent_hash: Default::default(),
        parent_beacon_block_root: None,
        ommers: &[],
        withdrawals: None,
        extra_data: Default::default(),
        tx_count_hint: Some(N_TX),
        slot_number: None,
    };
    let receipt_builder = reth_ethereum::evm::RethReceiptBuilder::default();
    let mut executor = ArcBlockExecutor::new(evm, ctx, chain_spec.clone(), &receipt_builder);

    let t = Instant::now();
    executor.apply_pre_execution_changes().expect("pre");
    let t_pre = t.elapsed();

    let t = Instant::now();
    for tx in &txs {
        executor.execute_transaction(tx).expect("tx");
    }
    let t_exec = t.elapsed();

    let t = Instant::now();
    let (_evm, res) = executor.finish().expect("finish");
    let t_fin = t.elapsed();

    assert_eq!(
        res.gas_used,
        N_TX as u64 * 21_000,
        "every tx must land as a plain 21k transfer"
    );
    assert_eq!(res.receipts.len(), N_TX);
    println!(
        "\ntransfer_bench: {N_TX} transfers, {N_ACCOUNTS} accounts, 1 Ggas block\n  pre-exec  {t_pre:?}\n  execute   {t_exec:?}   ({:.2} us/tx, {:.0} tx/s serial)\n  finish    {t_fin:?}",
        t_exec.as_secs_f64() * 1e6 / N_TX as f64,
        N_TX as f64 / t_exec.as_secs_f64(),
    );
}
