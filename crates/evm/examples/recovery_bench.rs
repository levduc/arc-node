//! What does a payload transaction actually cost BEFORE it reaches the executor?
//!
//! Live measurement (recovery-probe.sh, 4-validator demo, ~9.3k-tx payment blocks) says the
//! newPayload execution loop spends ~78% of its time blocked on `transactions.next()` --
//! 10.0 us/tx with `--engine.state-root-fallback`, 12.9-14.1 us/tx stock -- versus only
//! 2.9-4.5 us/tx inside our `ArcBlockExecutor`. That wait is the ordered channel fed by
//! reth's signature-recovery producer (`payload_processor::spawn_tx_iterator`, which runs
//! RLP-decode + ECDSA-recover on the reth `cpu_pool` via `for_each_ordered_in`).
//!
//! The wait is stable across 2-vs-8 competing spammers and across both state-root
//! strategies, so it is NOT CPU contention. That leaves two very different causes, and they
//! imply opposite fixes:
//!
//!   (a) recovery is genuinely this expensive per transaction  -> parallelism is already
//!       near its ceiling; the only real fix is to SKIP recovery (reuse mempool senders).
//!   (b) recovery is cheap when actually parallelised, and the ~10 us is the ordered
//!       delivery machinery -> recovering eagerly in `tx_iterator_for_payload` (arc-evm,
//!       NO reth fork) hands reth a pre-recovered vector and removes the per-tx stall.
//!
//! This bench measures the floor directly: real secp256k1-signed EIP-1559 transfers,
//! encoded exactly as they arrive in a payload, then decoded + recovered serially and in
//! parallel on this box.
//!
//!   cargo run --release -p arc-evm --example recovery_bench

use std::time::Instant;

use alloy_consensus::{SignableTransaction, TxEip1559};
use reth_primitives_traits::transaction::signed::SignedTransaction;
use alloy_eips::eip2718::{Decodable2718, Encodable2718};
use alloy_primitives::{Address, Bytes, TxKind, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use rayon::prelude::*;

const N_TX: usize = 20_000;
const N_SIGNERS: usize = 64;

fn main() {
    let threads = rayon::current_num_threads();
    println!("recovery floor: {N_TX} real signed transfers, {threads} rayon threads\n");

    // Encode transactions the way a payload carries them: opaque 2718 bytes.
    let signers: Vec<PrivateKeySigner> = (0..N_SIGNERS).map(|_| PrivateKeySigner::random()).collect();
    let t = Instant::now();
    let raw: Vec<Bytes> = (0..N_TX)
        .map(|i| {
            let signer = &signers[i % N_SIGNERS];
            let tx = TxEip1559 {
                chain_id: 1338,
                nonce: (i / N_SIGNERS) as u64,
                gas_limit: 21_000,
                max_fee_per_gas: 20_000_000_000,
                max_priority_fee_per_gas: 1_000_000_000,
                to: TxKind::Call(Address::repeat_byte(0x42)),
                value: U256::from(1_000u64),
                access_list: Default::default(),
                input: Default::default(),
            };
            let sig = signer.sign_hash_sync(&tx.signature_hash()).expect("sign");
            let signed = reth_ethereum_primitives::TransactionSigned::new_unhashed(tx.into(), sig);
            Bytes::from(signed.encoded_2718())
        })
        .collect();
    println!("  (built+signed in {:.2}s)\n", t.elapsed().as_secs_f64());

    let decode = |b: &Bytes| {
        reth_ethereum_primitives::TransactionSigned::decode_2718_exact(b.as_ref()).expect("decode")
    };

    // 1. decode only -- isolates RLP from the elliptic-curve work.
    let t = Instant::now();
    let decoded: Vec<_> = raw.iter().map(decode).collect();
    let d_serial = t.elapsed().as_secs_f64() * 1e6 / N_TX as f64;

    // 2. recover only, from already-decoded transactions.
    let t = Instant::now();
    let mut sink = Address::ZERO;
    for tx in &decoded {
        sink ^= tx.try_recover().expect("recover");
    }
    let r_serial = t.elapsed().as_secs_f64() * 1e6 / N_TX as f64;

    // 3. the full closure reth runs per transaction, serially...
    let t = Instant::now();
    let mut sink2 = Address::ZERO;
    for b in &raw {
        let tx = decode(b);
        sink2 ^= tx.try_recover().expect("recover");
    }
    let full_serial = t.elapsed().as_secs_f64() * 1e6 / N_TX as f64;

    // 4. ...and the same closure across the whole rayon pool.
    let t = Instant::now();
    let recovered: Vec<Address> = raw
        .par_iter()
        .map(|b| {
            let tx = decode(b);
            tx.try_recover().expect("recover")
        })
        .collect();
    let full_par = t.elapsed().as_secs_f64() * 1e6 / N_TX as f64;

    assert_eq!(recovered.len(), N_TX);
    assert_ne!(sink, Address::repeat_byte(0xff));
    assert_ne!(sink2, Address::repeat_byte(0xff));

    println!("  decode only        (serial)   {d_serial:>7.2} us/tx");
    println!("  ecdsa recover only (serial)   {r_serial:>7.2} us/tx");
    println!("  decode + recover   (serial)   {full_serial:>7.2} us/tx");
    println!("  decode + recover   (parallel) {full_par:>7.2} us/tx   ({:.1}x, {threads} threads)",
             full_serial / full_par);

    let live_wait = 10.0_f64;
    println!("\n  live wait/tx (state-root-fallback, ~4.8k-tx blocks): {live_wait:.2} us/tx");
    println!("  ideal parallel floor:                                {full_par:.2} us/tx");
    println!(
        "
  => RESOLVED, AND NOT THE WAY THIS BENCH FIRST SUGGESTED (2026-08-08).

     The numbers above are real: recovery parallelises {:.1}x, and the live loop stalls for
     roughly {:.1} us/tx more than the parallel floor. The tempting conclusion was that reth's
     ordered per-tx delivery wastes that gap, and that recovering the whole payload eagerly in
     ArcEvmConfig::tx_iterator_for_payload would reclaim it.

     It does not. Measured live, same box, same 4,761-tx blocks, SIMULTANEOUS window, eager on
     val1 only: the execution histogram fell 58.2 -> 19.9 ms/blk and wait/tx fell 8.7 -> 0.9 us,
     but 'other' time inside newPayload rose 4.1 -> 43.2 ms -- and TOTAL newPayload latency was
     85.9 ms eager vs 81.1-87.5 ms stock, i.e. unchanged within noise.

     The stall is not waste: reth OVERLAPS recovery with execution, so the `wait` histogram
     measures pipeline overlap, not lost time. Recovering eagerly converts that overlap into a
     serial barrier and gives back exactly what it saves. The eager path was reverted.

     LESSON: `transaction_wait` + `transaction_execution` do NOT sum to the cost of newPayload.
     Optimising against them can relocate work out of the loop and look like a win. Always
     confirm against `reth_consensus_engine_beacon_new_payload_latency`.",
        full_serial / full_par,
        live_wait - full_par
    );
}
