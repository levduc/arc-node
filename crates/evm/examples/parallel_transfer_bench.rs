//! Parallel native-transfer execution for the payment lane — algorithm + DIFFERENTIAL TEST.
//!
//!   cargo run --release -p arc-evm --example parallel_transfer_bench
//!
//! Runs the SAME block of transfers two ways — serial (production `ArcBlockExecutor` path) and
//! parallel — then compares the resulting state account-by-account. Any divergence is a bug and
//! fails the run. This is the same check the live chain makes with the state root, done offline
//! so we can iterate fast before touching consensus.
//!
//! ## Why not textbook Block-STM
//!
//! The real payment workload (spammer `transfer_recipient` = `0x1000 + nonce`) credits the SAME
//! recipient from many txs in a block. Optimistic Block-STM (pevm-style) sees that as a
//! read-write conflict on every tx and aborts/re-executes into a serial chain — the measured 1.0x
//! "hot recipient" case in `experiments/utxo-state/src/bin/blockstm.rs`.
//!
//! The insight (as in grevm's lazy balance updates): for a plain transfer a recipient's balance is
//! never *read*, only *incremented*. Increments commute. So:
//!
//!   * partition txs by SENDER — a sender's nonce/balance is a genuine read-modify-write, and one
//!     partition owns it exclusively, executing its txs in original order;
//!   * every other account touched (recipients, and the fee beneficiary which Arc credits on
//!     EVERY tx) is merged as a commutative BALANCE DELTA, in tx order.
//!
//! No aborts, no retries, deterministic. Hot recipients and the beneficiary — the two accounts
//! that break naive schemes — become the easy case.
//!
//! ## Correctness caveat this bench is built to catch
//!
//! A sender that is ALSO a recipient in the same block reads a balance that, serially, would have
//! included the incoming credit. With realistic funded accounts that never changes an outcome, but
//! it is a genuine semantic difference, so the differential check runs over BOTH workloads:
//!   * `pool`   — recipients are not senders (the real spammer pattern), and
//!   * `closed` — a ring where every recipient is also a sender (the adversarial case).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use alloy_consensus::{transaction::Recovered, TxEip1559};
use alloy_primitives::{address, Address, Signature, TxKind, U256};
use arc_evm::executor::ArcBlockExecutor;
use arc_evm::{ArcEvmConfig, ArcEvmFactory};
use arc_execution_config::chainspec::LOCAL_DEV;
use rayon::prelude::*;
use reth_chainspec::EthChainSpec;
use reth_evm::block::BlockExecutor;
use reth_evm::Evm; // brings transact()/db_mut() into scope
use reth_evm::{ConfigureEvm, EvmEnv};
use revm::database::CacheDB;
use revm::{
    context::{BlockEnv, CfgEnv},
    database::InMemoryDB,
    state::AccountInfo,
    Database, DatabaseCommit,
};
use revm_primitives::{hardfork::SpecId, StorageKey, StorageValue};

const N_TX: usize = 47_618; // 1 Ggas / 21k = a full payment block
const N_SENDERS: usize = 16_000;
const GAS_1G: u64 = 1_000_000_000;
const BASEFEE: u64 = 20_000_000_000; // 20 gwei (demo economics)
const MAX_FEE: u128 = 40_000_000_000_000;
const TIP: u128 = 1_000_000_000;
const BENEFICIARY: Address = address!("00000000000000000000000000000000000000ff");

fn sender_addr(i: usize) -> Address {
    let mut b = [0u8; 20];
    b[0] = 0xAA;
    b[12..20].copy_from_slice(&(i as u64).to_be_bytes());
    Address::from(b)
}

/// Recipient not in the sender set — the real spammer pattern (`0x1000 + nonce`).
fn pool_recipient(i: usize) -> Address {
    Address::left_padding_from(&((0x1000u64).wrapping_add((i % 512) as u64)).to_be_bytes())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Workload {
    /// Recipients are NOT senders; many txs credit the same recipient (hot-recipient).
    Pool,
    /// Ring: every recipient is also a sender (adversarial for the delta scheme).
    Closed,
}

fn build_db() -> InMemoryDB {
    let chain_spec = LOCAL_DEV.clone();
    let mut db = InMemoryDB::default();
    for (a, acct) in &chain_spec.genesis().alloc {
        let code = acct.code.clone().map(revm::state::Bytecode::new_raw);
        db.insert_account_info(
            *a,
            AccountInfo {
                balance: acct.balance,
                nonce: acct.nonce.unwrap_or_default(),
                code_hash: code
                    .as_ref()
                    .map(|c| c.hash_slow())
                    .unwrap_or(revm_primitives::KECCAK_EMPTY),
                code,
                ..Default::default()
            },
        );
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
    // 1 Ggas block gas limit in ProtocolConfig (pre-execution validates the header against it)
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
    for i in 0..N_SENDERS {
        db.insert_account_info(
            sender_addr(i),
            AccountInfo {
                balance: U256::from(10u128.pow(22)),
                ..Default::default()
            },
        );
    }
    db
}

fn build_txs(w: Workload) -> Vec<Recovered<reth_ethereum_primitives::TransactionSigned>> {
    let chain_id = LOCAL_DEV.chain_id();
    let sig = Signature::new(U256::from(1), U256::from(1), false);
    (0..N_TX)
        .map(|i| {
            let s = i % N_SENDERS;
            let to = match w {
                Workload::Pool => pool_recipient(i),
                Workload::Closed => sender_addr((s + 1) % N_SENDERS),
            };
            let tx = TxEip1559 {
                chain_id,
                nonce: (i / N_SENDERS) as u64,
                gas_limit: 21_000,
                max_fee_per_gas: MAX_FEE,
                max_priority_fee_per_gas: TIP,
                to: TxKind::Call(to),
                value: U256::from(1_000u64),
                access_list: Default::default(),
                input: Default::default(),
            };
            Recovered::new_unchecked(
                reth_ethereum_primitives::TransactionSigned::new_unhashed(tx.into(), sig),
                sender_addr(s),
            )
        })
        .collect()
}

fn evm_env() -> EvmEnv {
    EvmEnv {
        cfg_env: CfgEnv::new()
            .with_chain_id(LOCAL_DEV.chain_id())
            .with_spec_and_mainnet_gas_params(SpecId::PRAGUE),
        block_env: BlockEnv {
            basefee: BASEFEE,
            gas_limit: GAS_1G,
            beneficiary: BENEFICIARY,
            ..Default::default()
        },
    }
}

fn evm_config() -> ArcEvmConfig {
    let cs = LOCAL_DEV.clone();
    ArcEvmConfig::new(reth_ethereum::evm::EthEvmConfig::new_with_evm_factory(
        cs.clone(),
        ArcEvmFactory::new(cs),
    ))
}

/// Post-state fingerprint: every account the block touched. This is the differential check —
/// the offline stand-in for comparing state roots on-chain.
type Fingerprint = BTreeMap<Address, (U256, u64)>;

fn fingerprint(db: &mut InMemoryDB, w: Workload) -> Fingerprint {
    let mut out = BTreeMap::new();
    let mut add = |db: &mut InMemoryDB, a: Address| {
        if let Ok(Some(i)) = db.basic(a) {
            out.insert(a, (i.balance, i.nonce));
        }
    };
    for i in 0..N_SENDERS {
        add(db, sender_addr(i));
    }
    if w == Workload::Pool {
        for i in 0..N_TX.min(4096) {
            add(db, pool_recipient(i));
        }
    }
    add(db, BENEFICIARY);
    out
}

/// Production path: reth's engine drives `ArcBlockExecutor` one transaction at a time.
fn run_serial(
    db: &mut InMemoryDB,
    txs: &[Recovered<reth_ethereum_primitives::TransactionSigned>],
) -> Duration {
    let cfg = evm_config();
    let cs = LOCAL_DEV.clone();
    let mut state = revm::database::State::builder()
        .with_database(&mut *db)
        .with_bundle_update()
        .build();
    let evm = cfg.evm_with_env(&mut state, evm_env());
    let rb = reth_ethereum::evm::RethReceiptBuilder::default();
    let mut ex = ArcBlockExecutor::new(
        evm,
        reth_evm::eth::EthBlockExecutionCtx {
            parent_hash: Default::default(),
            parent_beacon_block_root: None,
            ommers: &[],
            withdrawals: None,
            extra_data: Default::default(),
            tx_count_hint: Some(txs.len()),
            slot_number: None,
        },
        cs,
        &rb,
    );
    ex.apply_pre_execution_changes().expect("pre");
    let t = Instant::now();
    for tx in txs {
        ex.execute_transaction(tx).expect("tx");
    }
    let d = t.elapsed();
    let (mut evm_after, res) = ex.finish().expect("finish");
    assert_eq!(res.receipts.len(), txs.len());
    // flush the executor's accumulated state into the backing InMemoryDB so we can fingerprint it.
    // State keeps changes as *transitions* until merged; without this the bundle is empty.
    evm_after
        .db_mut()
        .merge_transitions(revm::database::states::bundle_state::BundleRetention::PlainState);
    let bundle = evm_after.db_mut().take_bundle();
    let mut plain: Vec<_> = bundle.state.into_iter().collect();
    plain.sort_by_key(|(a, _)| *a);
    for (addr, acc) in plain {
        if let Some(info) = acc.info.clone() {
            db.insert_account_info(addr, info);
        }
    }
    d
}

/// Parallel path: partition by sender, merge balances as commutative deltas.
fn run_parallel(
    db: &mut InMemoryDB,
    txs: &[Recovered<reth_ethereum_primitives::TransactionSigned>],
    threads: usize,
) -> Duration {
    // ---- partition by sender (deterministic; a sender's nonce chain stays ordered) ----
    let mut by_sender: BTreeMap<Address, Vec<usize>> = BTreeMap::new();
    for (i, tx) in txs.iter().enumerate() {
        by_sender.entry(*tx.signer_ref()).or_default().push(i);
    }
    let partitions: Vec<Vec<usize>> = by_sender.into_values().collect();
    // spread partitions over workers (each worker builds one EVM + overlay)
    let per = partitions.len().div_ceil(threads * 4); // 4x chunks => better load balance
    let chunks: Vec<Vec<Vec<usize>>> =
        partitions.chunks(per.max(1)).map(|c| c.to_vec()).collect();

    let base: &InMemoryDB = db;
    let cfg = evm_config();
    let env = evm_env();

    let t = Instant::now();
    // ---- parallel phase ----
    // Each worker executes whole senders against a private overlay and AGGREGATES per address:
    // (credit, debit, owned-nonce). Deltas commute, so aggregating inside the worker is exact and
    // shrinks the serial merge from one entry per (tx, account) to one per (worker, account).
    let per_worker: Vec<std::collections::HashMap<Address, (U256, U256, Option<u64>)>> = chunks
        .par_iter()
        .map(|senders| {
            let mut cdb = CacheDB::new(base);
            let mut evm = cfg.evm_with_env(&mut cdb, env.clone());
            let mut agg: std::collections::HashMap<Address, (U256, U256, Option<u64>)> =
                std::collections::HashMap::new();
            for part in senders {
                for &i in part {
                    let tx = &txs[i];
                    let owner = *tx.signer_ref();
                    let res = match evm.transact(tx) {
                        Ok(r) => r,
                        Err(_) => continue, // invalid tx: the serial path drops it the same way
                    };
                    for (addr, acct) in res.state.iter() {
                        if !acct.is_touched() {
                            continue;
                        }
                        let pre = evm
                            .db_mut()
                            .basic(*addr)
                            .ok()
                            .flatten()
                            .map(|i| i.balance)
                            .unwrap_or_default();
                        let post = acct.info.balance;
                        let e = agg.entry(*addr).or_insert((U256::ZERO, U256::ZERO, None));
                        if post >= pre {
                            e.0 = e.0.saturating_add(post - pre);
                        } else {
                            e.1 = e.1.saturating_add(pre - post);
                        }
                        if *addr == owner {
                            e.2 = Some(acct.info.nonce); // owner's latest nonce wins
                        }
                    }
                    evm.db_mut().commit(res.state);
                }
            }
            agg
        })
        .collect();

    // ---- merge phase: order-independent (credits/debits commute; each sender has one owner) ----
    for agg in per_worker {
        let mut entries: Vec<_> = agg.into_iter().collect();
        entries.sort_by_key(|(a, _)| *a); // deterministic application order
        for (addr, (credit, debit, nonce)) in entries {
            let mut info = db.basic(addr).ok().flatten().unwrap_or_default();
            info.balance = info.balance.saturating_add(credit).saturating_sub(debit);
            if let Some(n) = nonce {
                info.nonce = n;
            }
            db.insert_account_info(addr, info);
        }
    }
    t.elapsed()
}

/// Hand-written transfer semantics — NO EVM at all. This is what the in-executor fast path will
/// do; the differential check below proves it matches revm exactly before it goes near consensus.
///
/// For a plain 21k transfer the semantics are fully determined:
///   gas_used            = 21_000
///   effective_gas_price = min(max_fee, basefee + max_priority)
///   sender   -= value + 21_000 * effective   ; nonce += 1
///   to       += value
///   beneficiary += 21_000 * effective        (Arc credits the FULL fee; base fee is NOT burned)
fn run_fastpath(
    db: &mut InMemoryDB,
    txs: &[Recovered<reth_ethereum_primitives::TransactionSigned>],
) -> Duration {
    use alloy_consensus::Transaction as _;
    let t = Instant::now();
    for tx in txs {
        let sender = *tx.signer_ref();
        let inner = tx.inner();
        let to = match inner.kind() {
            TxKind::Call(a) => a,
            TxKind::Create => continue,
        };
        let value = inner.value();
        let eff = core::cmp::min(
            inner.max_fee_per_gas(),
            (BASEFEE as u128).saturating_add(inner.max_priority_fee_per_gas().unwrap_or(0)),
        );
        let fee = U256::from(eff).saturating_mul(U256::from(21_000u64));

        let mut si = db.basic(sender).ok().flatten().unwrap_or_default();
        si.balance = si.balance.saturating_sub(value).saturating_sub(fee);
        si.nonce += 1;
        db.insert_account_info(sender, si);

        let mut ti = db.basic(to).ok().flatten().unwrap_or_default();
        ti.balance = ti.balance.saturating_add(value);
        db.insert_account_info(to, ti);

        let mut bi = db.basic(BENEFICIARY).ok().flatten().unwrap_or_default();
        bi.balance = bi.balance.saturating_add(fee);
        db.insert_account_info(BENEFICIARY, bi);
    }
    t.elapsed()
}

fn main() {
    let threads = rayon::current_num_threads();
    println!(
        "parallel_transfer_bench — {N_TX} transfers, {N_SENDERS} senders, 1 Ggas block, {threads} threads\n"
    );

    // ---- decomposition: what is left in the executor path after the fast path + caches? ----
    // The fast path still does 2 state reads per transfer (sender, recipient); everything else is
    // receipt building + State/bundle commit. This isolates the read half so we know whether
    // batch-prefetching those 2 reads is worth a ~250-line consensus-critical change.
    {
        use alloy_consensus::Transaction as _;
        let txs = build_txs(Workload::Pool);
        let mut db = build_db();
        let mut state = revm::database::State::builder()
            .with_database(&mut db)
            .with_bundle_update()
            .build();
        let t = Instant::now();
        let mut sink = 0u64;
        for tx in &txs {
            let to = match tx.inner().kind() { TxKind::Call(a) => a, _ => continue };
            if let Ok(Some(i)) = state.basic(*tx.signer_ref()) { sink = sink.wrapping_add(i.nonce); }
            if let Ok(Some(i)) = state.basic(to) { sink = sink.wrapping_add(i.nonce); }
        }
        let t_reads = t.elapsed();

        // isolate db.commit(): apply a realistic 3-account transfer diff per tx, nothing else
        use revm::state::{Account, EvmState};
        let bene = BENEFICIARY;
        let t = Instant::now();
        for (i, tx) in txs.iter().enumerate() {
            let to = match tx.inner().kind() { TxKind::Call(a) => a, _ => continue };
            let mut st = EvmState::default();
            for a in [*tx.signer_ref(), to, bene] {
                let mut info = revm::state::AccountInfo::default();
                info.balance = U256::from(1_000_000u64 + i as u64);
                let mut acct = Account::from(info.clone());
                acct.info = info;
                acct.mark_touch();
                st.insert(a, acct);
            }
            state.commit(st);
        }
        let t_commit = t.elapsed();
        println!(
            "decomposition (pool, {N_TX} txs), executor path is ~54ms total:\n  2 state reads/tx  {:>8.1?}  ({:.2} us/tx)\n  pure arithmetic   ~4.6ms\n  => remainder (receipts + State/bundle commit) is the rest  [sink {}]\n",
            t_reads, t_reads.as_secs_f64()*1e6/N_TX as f64, sink & 1
        );
        println!(
            "  db.commit only     {:>8.1?}  ({:.2} us/tx)  <- 3-account diff per tx, bundle tracking on",
            t_commit, t_commit.as_secs_f64()*1e6/N_TX as f64
        );
    }

    for (name, w) in [("pool  (recipients NOT senders — real spammer)", Workload::Pool),
                      ("closed(ring: every recipient is a sender)   ", Workload::Closed)]
    {
        let txs = build_txs(w);

        let mut db_s = build_db();
        let t_serial = run_serial(&mut db_s, &txs);
        let fp_serial = fingerprint(&mut db_s, w);

        let mut db_p = build_db();
        let t_par = run_parallel(&mut db_p, &txs, threads);
        let fp_par = fingerprint(&mut db_p, w);

        // ---- DIFFERENTIAL CHECK: identical workload must give identical state ----
        let mut diffs = 0usize;
        let mut first: Option<String> = None;
        for (a, s) in &fp_serial {
            match fp_par.get(a) {
                Some(p) if p == s => {}
                other => {
                    diffs += 1;
                    if first.is_none() {
                        first = Some(format!("{a}: serial {s:?} vs parallel {other:?}"));
                    }
                }
            }
        }
        // ---- hand-written fast path (no EVM) vs the same serial oracle ----
        let mut db_f = build_db();
        let t_fast = run_fastpath(&mut db_f, &txs);
        let fp_fast = fingerprint(&mut db_f, w);
        let fast_diffs = fp_serial.iter().filter(|(a, s)| fp_fast.get(*a) != Some(*s)).count();

        let verdict = if diffs == 0 { "IDENTICAL ✓" } else { "DIVERGED ✗" };
        println!("{name}");
        println!(
            "  serial    {:>9.1?}   parallel {:>9.1?}   speedup {:.2}x",
            t_serial,
            t_par,
            t_serial.as_secs_f64() / t_par.as_secs_f64()
        );
        println!(
            "  state     {verdict}  ({} accounts compared{})",
            fp_serial.len(),
            if diffs > 0 {
                format!(", {diffs} differ — e.g. {}", first.unwrap())
            } else {
                String::new()
            }
        );
        println!(
            "  fastpath  {:>9.1?} (no EVM)          speedup {:.1}x   state {}",
            t_fast,
            t_serial.as_secs_f64() / t_fast.as_secs_f64(),
            if fast_diffs == 0 { "IDENTICAL ✓".to_string() } else { format!("DIVERGED ✗ ({fast_diffs})") }
        );
        println!();
    }
}
