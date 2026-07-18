//! Integration test: grevm Block-STM executes Arc-shaped transactions in-tree.
//! Lives in tests/ (not a #[cfg(test)] unit module) so it compiles the arc-evm library
//! NORMALLY and does not drag in evm.rs's (separately bit-rotted) unit-test module.

use arc_evm::parallel::parallel_execute_block;
use revm::context::result::ExecutionResult as ER;
use revm::context::{BlockEnv, CfgEnv, TxEnv};
use revm::database::CacheDB;
use revm::database_interface::EmptyDB;
use revm::state::AccountInfo;
use revm::primitives::{Address, TxKind, U256};

fn addr(i: u64) -> Address {
    let mut a = [0u8; 20];
    a[12..].copy_from_slice(&i.to_be_bytes());
    Address::from(a)
}

fn seeded_db(n: u64) -> CacheDB<EmptyDB> {
    let mut db = CacheDB::new(EmptyDB::default());
    for i in 0..n {
        db.insert_account_info(
            addr(i),
            AccountInfo { balance: U256::from(1_000_000u64), nonce: 0, ..Default::default() },
        );
    }
    db
}

fn transfer_tx(from: u64, to: u64, amount: u64, chain_id: u64) -> TxEnv {
    TxEnv {
        caller: addr(from),
        kind: TxKind::Call(addr(to)),
        value: U256::from(amount),
        gas_limit: 100_000,
        gas_price: 0,
        chain_id: Some(chain_id),
        nonce: 0,
        ..Default::default()
    }
}

#[test]
fn block_stm_executes_transfers() {
    let n = 200u64;
    let db = seeded_db(n * 2);
    let txs: Vec<TxEnv> = (0..n).map(|i| transfer_tx(i, n + i, 100, 1)).collect();
    let cfg = { let mut c = CfgEnv::new().with_chain_id(1); c.disable_nonce_check = true; c.disable_base_fee = true; c };
    let block = BlockEnv { gas_limit: 30_000_000, basefee: 0, ..Default::default() };

    let (results, _state) =
        parallel_execute_block(cfg, block, txs, db).expect("parallel execute");

    assert_eq!(results.len(), n as usize, "one result per tx");
    assert!(results.iter().all(|r| matches!(r, ER::Success { .. })),
            "every disjoint transfer must succeed under Block-STM");
}

#[test]
fn block_stm_is_deterministic() {
    let n = 128u64;
    let mk = || {
        let db = seeded_db(n * 2);
        let txs: Vec<TxEnv> = (0..n).map(|i| transfer_tx(i, n + i, 50, 1)).collect();
        let cfg = { let mut c = CfgEnv::new().with_chain_id(1); c.disable_nonce_check = true; c.disable_base_fee = true; c };
        let block = BlockEnv { gas_limit: 30_000_000, basefee: 0, ..Default::default() };
        parallel_execute_block(cfg, block, txs, db).unwrap().0
    };
    let a = mk();
    let b = mk();
    let gas = |rs: &[ER]| rs.iter().map(|r| r.gas_used()).collect::<Vec<_>>();
    assert_eq!(gas(&a), gas(&b), "two parallel runs must agree per-tx");
}
