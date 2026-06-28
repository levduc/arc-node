// Two-EL, two-root prototype for the Arc payment-lane architecture.
//
// A mock CL drives TWO execution layers over (conceptually) the Engine API and
// combines their state commitments into ONE header with TWO roots:
//   * EVM-EL    -> account state + MPT root   (stands in for Arc's reth EL)
//   * Payment-EL -> RAM UTXO set + ECMH root  (the new minimal lane)
//
// The ELs build IN PARALLEL, so block time = max(T_evm, T_pay), not the sum.
// Because T_pay (RAM + flat ECMH) is small, adding the payment lane is ~free
// until payment load grows enough that T_pay overtakes T_evm.

use std::collections::HashMap;
use std::time::Instant;

use alloy_primitives::{keccak256, B256};
use alloy_trie::{HashBuilder, Nibbles};
use curve25519_dalek_ng::ristretto::RistrettoPoint;
use sha2::Sha512;

// ---- tiny deterministic PRNG ----
struct Rng(u64);
impl Rng {
    #[inline]
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    #[inline]
    fn below(&mut self, n: usize) -> usize {
        (self.next() % (n as u64)) as usize
    }
}

// ============================ Payment EL ============================
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct OutPoint {
    txid: [u8; 32],
    vout: u32,
}
#[derive(Clone, Copy)]
struct Output {
    owner: [u8; 20],
    amount: u64,
}

#[inline]
fn utxo_bytes(op: &OutPoint, out: &Output) -> [u8; 64] {
    let mut b = [0u8; 64];
    b[..32].copy_from_slice(&op.txid);
    b[32..36].copy_from_slice(&op.vout.to_le_bytes());
    b[36..56].copy_from_slice(&out.owner);
    b[56..64].copy_from_slice(&out.amount.to_le_bytes());
    b
}
#[inline]
fn ecmh_point(op: &OutPoint, out: &Output) -> RistrettoPoint {
    RistrettoPoint::hash_from_bytes::<Sha512>(&utxo_bytes(op, out))
}

struct PaymentEl {
    set: HashMap<OutPoint, Output>,
    keys: Vec<OutPoint>,
    acc: RistrettoPoint, // ECMH accumulator = sum of per-UTXO points
    rng: Rng,
    nonce: u64,
}
impl PaymentEl {
    fn new(genesis: usize) -> Self {
        let mut el = PaymentEl {
            set: HashMap::new(),
            keys: Vec::new(),
            acc: RistrettoPoint::default(),
            rng: Rng(0xDEADBEEF),
            nonce: 0,
        };
        for _ in 0..genesis {
            el.create();
        }
        el
    }
    fn create(&mut self) {
        let mut txid = [0u8; 32];
        txid[..8].copy_from_slice(&self.nonce.to_le_bytes());
        txid[8..16].copy_from_slice(&self.rng.next().to_le_bytes());
        self.nonce += 1;
        let op = OutPoint { txid, vout: 0 };
        let mut owner = [0u8; 20];
        owner[..8].copy_from_slice(&self.rng.next().to_le_bytes());
        let out = Output { owner, amount: self.rng.next() % 1_000_000 };
        self.acc += ecmh_point(&op, &out);
        self.set.insert(op, out);
        self.keys.push(op);
    }
    // Engine-API analog: build a block of `n` payments, return (ecmh_root, duration_ms)
    fn build_block(&mut self, n: usize) -> (B256, f64) {
        let t0 = Instant::now();
        for _ in 0..n {
            if !self.keys.is_empty() {
                let idx = self.rng.below(self.keys.len());
                let op = self.keys.swap_remove(idx);
                if let Some(out) = self.set.remove(&op) {
                    self.acc -= ecmh_point(&op, &out); // spend = subtract point
                }
            }
            self.create(); // create = add point
        }
        let root = B256::from_slice(&self.acc.compress().to_bytes());
        (root, t0.elapsed().as_secs_f64() * 1000.0)
    }
}

// ============================ EVM EL (real MPT work) ============================
// Stands in for Arc's reth EL: a sorted account set committed by an MPT root.
struct EvmEl {
    accounts: Vec<(B256, [u8; 32])>, // (keccak(addr), value), kept sorted by key
    rng: Rng,
}
impl EvmEl {
    fn new(n: usize) -> Self {
        let mut rng = Rng(0x1234_5678);
        let mut accounts: Vec<(B256, [u8; 32])> = (0..n)
            .map(|_| {
                let key = keccak256(rng.next().to_le_bytes());
                let mut v = [0u8; 32];
                v[..8].copy_from_slice(&rng.next().to_le_bytes());
                (key, v)
            })
            .collect();
        accounts.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        accounts.dedup_by(|a, b| a.0 == b.0);
        EvmEl { accounts, rng }
    }
    // Engine-API analog: apply `updates` account writes, recompute MPT state root.
    fn build_block(&mut self, updates: usize) -> (B256, f64) {
        let t0 = Instant::now();
        let n = self.accounts.len();
        for _ in 0..updates {
            let i = self.rng.below(n);
            let r = self.rng.next().to_le_bytes();
            self.accounts[i].1[..8].copy_from_slice(&r); // mutate value (key/order unchanged)
        }
        let mut hb = HashBuilder::default();
        for (key, val) in &self.accounts {
            hb.add_leaf(Nibbles::unpack(key.as_slice()), val.as_slice());
        }
        (hb.root(), t0.elapsed().as_secs_f64() * 1000.0)
    }
}

fn short(h: &B256) -> String {
    let b = h.as_slice();
    format!("{:02x}{:02x}{:02x}{:02x}", b[0], b[1], b[2], b[3])
}

fn main() {
    let evm_accounts = 200_000usize; // -> T_evm ~ a couple hundred ms (MPT root stand-in)
    let evm_updates = 500usize;
    let pay_genesis = 100_000usize;

    eprintln!("booting EVM-EL ({evm_accounts} accounts) + Payment-EL ({pay_genesis} UTXOs)...");
    let mut evm = EvmEl::new(evm_accounts);
    let mut pay = PaymentEl::new(pay_genesis);

    // growing payment load per block, to find the T_pay vs T_evm crossover
    let loads = [1_000usize, 2_000, 5_000, 10_000, 20_000, 40_000, 80_000];

    println!(
        "\n{:>3} | {:>9} | {:>8} | {:>8} | {:>13} | {:>11} | {:>10} | header (2 roots)",
        "h", "payments", "T_evm", "T_pay", "parallel(max)", "serial(sum)", "lane cost"
    );
    println!("{}", "-".repeat(108));

    for (i, &load) in loads.iter().enumerate() {
        let height = (i + 1) as u64;

        // ---- mock CL: fan out to both ELs in parallel, then combine ----
        let wall = Instant::now();
        let mut evm_root = B256::ZERO;
        let mut pay_root = B256::ZERO;
        let (mut t_evm, mut t_pay) = (0.0f64, 0.0f64);
        std::thread::scope(|s| {
            let he = s.spawn(|| evm.build_block(evm_updates));
            let hp = s.spawn(|| pay.build_block(load));
            let (er, te) = he.join().unwrap();
            let (pr, tp) = hp.join().unwrap();
            evm_root = er;
            pay_root = pr;
            t_evm = te;
            t_pay = tp;
        });
        let parallel_ms = wall.elapsed().as_secs_f64() * 1000.0;
        let serial_ms = t_evm + t_pay;

        // ---- combine the two roots into one header ----
        let mut hbytes = [0u8; 72];
        hbytes[..32].copy_from_slice(evm_root.as_slice());
        hbytes[32..64].copy_from_slice(pay_root.as_slice());
        hbytes[64..72].copy_from_slice(&height.to_le_bytes());
        let header = keccak256(hbytes);

        // ---- "lane cost" = what the payment lane ADDS to block time = max - T_evm ----
        let lane_cost = (parallel_ms - t_evm).max(0.0);

        println!(
            "{:>3} | {:>9} | {:>6.0}ms | {:>6.0}ms | {:>11.0}ms | {:>9.0}ms | {:>8.0}ms | evm:{} pay:{} -> {}",
            height,
            load,
            t_evm,
            t_pay,
            parallel_ms,
            serial_ms,
            lane_cost,
            short(&evm_root),
            short(&pay_root),
            short(&header)
        );
    }

    println!("{}", "-".repeat(108));
    println!("CL drives both ELs concurrently -> block time = max(T_evm, T_pay).");
    println!("While T_pay < T_evm the payment lane adds ~0 to block time ('lane cost' ~ 0);");
    println!("once payment load is large enough that T_pay > T_evm, the lane becomes the binding stage.");
}
