# CLAUDE.md — arc-node working notes

Arc is an EVM-compatible L1: a **customized Reth** execution layer (reth SDK, currently
`v2.3.0`) driven by the **Malachite** BFT consensus layer over the Engine API. This repo also
carries a second, experimental **lean payment lane** that runs beside the EVM lane under the
same consensus.

Full chronological history (campaigns, wrong turns, retractions): **`docs/campaign-log.md`**.
This file is the working document — current state, how to run it, what not to repeat.

---

## 1. Toolchain

- **Rust** per `rust-toolchain.toml`, via rustup.
- **clang + libclang-dev** — reth-mdbx bindgen needs them (EVM lane only; the lean node has no C deps).
- **Docker** for `make testnet` / `make build-docker` (EVM lane CL+EL images).
- **Foundry** pinned in `.foundry-version` (`cast` — governance txs, fund-file generation).
- **Node ≥20.19 / 22.x (even majors)** — genesis step only. Node 18 fails with a misleading `HH19`.

## 2. Common commands

```bash
make build                 # cargo build the node
make genesis               # assets/localdev/genesis.json (idempotent)
make testnet               # genesis + docker images + quake (5 validators + monitoring)
make testnet-down / -clean
make test-unit             # rust unit tests + lint
cargo build --release -p lean-lane-node -p spammer   # lean lane + load generator
```

`quake` is the in-repo testnet manager (`crates/quake`), run via the Makefile, not a global
binary. Local RPCs for a running testnet: `.quake/localdev/nodes.json`.

**Crate-name landmine:** the CL crate is `arc-node-consensus`. `cargo check -p
arc-malachitebft-app` silently checks the *git* dependency and prints "Finished" having done
nothing — it masked 8 real errors once. Always `-p arc-node-consensus`.

---

## 3. The two lanes

| | EVM lane | lean payment lane |
|---|---|---|
| client | customized reth (`crates/node`, `crates/evm`, …) | `crates/lean-lane-node` (this repo) |
| chain id | 1337 (localdev) | 1338 |
| interface to CL | Engine API (IPC) | 4-verb shim RPC (`docs/lean-lane-integration.md`) |
| state | full MPT, receipts, logs | flat nonce+balance map, keccak commitment chain |
| tx | EIP-1559 native transfer, 122 B | fan-out type `0x50`, 72 + 28N B |
| enabled by | always | `ARC_PAYMENT_LEAN_LANE=1` (default off ⇒ stock behaviour) |

Both lanes are committed under **one BFT certificate**: `value_id = keccak(evm_hash ‖
lean_commitment)`. Voting on the lean lane is **structural only** (parent linkage + timestamp
lockstep); execution happens in the vote gap and is anchored at decide. Total-STF semantics
(an invalid tx is a no-op) mean a byzantine proposer can waste bytes but cannot halt or fork
the lane.

**The lean lane is self-contained in this repo** — `crates/lean-native` (wire format + pool
integration) and `crates/lean-lane-node` (the node) build against upstream reth `v2.3.0` like
everything else. `~/reth-fork` is history only; nothing here depends on it.

---

## 4. Running it

**Verify a machine in one command** (~1 min, no docker/fleet/fork):

```bash
./experiments/dual-el/lean-smoke.sh
```

Boots three lean nodes on loopback, drives them with a stand-in CL, feeds signed fan-out
transactions, asserts convergence. Last run: 40 heights, 9,856 txs, 98,560 payments, 3/3 nodes
at one commitment.

**Genesis funding** — the node funds a fixed address set; it must be exactly the accounts the
spammer signs with, derivation `m/44'/60'/1'/0/i` (note the **`1'`**, not `0'`):

```bash
./experiments/dual-el/gen-lean-fund.sh 800 ~/lean-fund.txt
```

**Multi-machine**: `cp experiments/dual-el/fleet.env.example experiments/dual-el/fleet.env`
(hosts/IPs/paths), then `./experiments/dual-el/deploy-lean.sh` (builds + ships binary,
spammer, feeder, fund file — sha-verified *after* transfer).

**Benchmark** — one trigger, either lane:

```bash
./experiments/dual-el/lane-bench.sh evm  75000000
./experiments/dual-el/lane-bench.sh lean 150000000 100 600
```

It carries the whole measurement protocol (§6). **Extend this script rather than hand-rolling
per-experiment shell** — every gate in it was paid for with a wrong number.

Setup guide for a fresh machine: `docs/lean-lane-setup.md`.

---

## 5. Where things stand (measured, 4-machine fleet, 500 ms pacer)

At the **2 blk/s product target**, all blocks 100 % of budget unless noted:

| config | cadence | tx/s | payments/s |
|---|---|---|---|
| EVM lane, 50M | 1.75 | 4,165 | 4,165 |
| EVM lane, 75M | 1.69 | 5,972 | 5,972 |
| EVM lane, 100M | 1.47 | 6,032 | 6,032 (68 % full — delivery-bound) |
| lean, N=1, 100M | 1.96 | 6,401 | 6,401 (85 % full — headroom left) |
| lean, N=100, 150M | 1.93 | 555 | **55,534** |
| lean, N=100, 225M | 1.93 | 833 | **83,286** |

Fan-out sweep at fixed ~830 KB blocks: N=5 → 35,927 · N=10 → 45,427 · N=20 → 50,590 · N=50 →
54,172 · N=100 → 55,534 payments/s. Saturates by N=50 at the 28 B/payment wire floor.

Drain (zero ingress) ceiling: **83,286 outputs/s**; lean EL alone imports at **1.03 M
outputs/s** (`lean-replay-bench.py`).

### The four durable findings

1. **Execution is not the bottleneck.** The EL is 7–15 % of a height on *both* lanes. The lean
   node imports at 0.97 µs/output while consensus delivers ~40 µs/output.
2. **Consensus transport is.** Height decomposition at drain size: proposal stream+assemble
   57 %, votes 13 %, anchor 10 %, validate 2 %. Signatures are irrelevant at high N.
3. **At one payment per signature the lanes are equals** (~6 k tx/s). A leaner execution engine
   does not make single payments faster; both meet the same ecrecover/coordination wall.
4. **Fan-out buys everything else**: 9× the payments on 1/11th the signatures, because it
   amortises the per-*transaction* costs (signature, mempool entry, envelope) — not the
   per-payment ones. Per 100 payments: 100→1 signatures, 3.3 ms→33 µs ecrecover, 100→1 pool
   entries, 10,000→2,872 wire bytes; execution unchanged but conflict-free by construction.

---

## 6. Measurement protocol (why every number needs it)

A validator can look perfectly healthy — containers up, agreement passing, blocks 100 % full —
while contributing **nothing**. Two ways seen live: a **parked CL** (booted while its lean node
was down → "Manual intervention required"), and a CL degenerated into a **pure sync-follower**
(decides by fetching, never proposes). Each costs ~25 % of cadence and is invisible to
agreement checks. **Always attribute failed round-0s by proposer.**

Baked into `lane-bench.sh`, and mandatory for any hand-run experiment:

- per-machine **container census** before measuring;
- **pool wipe before recreating CLs** (a CL that boots while its lean node is down parks);
- pre-measure **CL health gate** (live + unparked);
- **chain-static check** before `-l` corpus generation (a surviving feeder advances nonces →
  corpus born stale → queued forever);
- **corpus probe**: submit one tx, require `pending ≥ 1`;
- **per-machine corpus generation** (never ship 90 MB over tailscale);
- **remote-effect verification** — tailscale ssh exit codes lie when its session check expires;
- **fullness with every number** — a non-full block measures delivery, not the chain;
- sample from an **uninvolved wired validator**.

### Ops landmines (each cost a run)

- `pkill`/`pgrep -f` matches the shell carrying the pattern; `pkill` returning 1 aborts a
  compound block. Split kill and launch into separate calls; guard with `|| true`.
- Remote background launches need `(setsid nohup … &)` + `</dev/null`; tailscale-ssh teardown
  kills a plain `&`.
- bash bare `wait` blocks on *any* backgrounded job — `disown` daemons, wait on explicit PIDs.
- zsh aborts a compound command on a no-match glob (`setopt nonomatch` or enumerate).
- zsh does **not** word-split `set -- $var`.
- RPC bodies go through **files, never argv** — a budget-full block's base64 is ~1 MB and trips
  "Argument list too long".
- Fan-out **drains sender balances one-way** (~4.4 k txs at N=100 bankrupts them even at 10×
  funding) — refund by regenerating genesis.
- Never wipe a lean chain under a live CL chain: certificates bind destroyed bytes and
  laggards can never sync that span.
- The lean `getTransactionCount` is **pool-inclusive** — generate `-l` corpora only against
  empty pools.

---

## 7. Layout

```
crates/lean-native/        fan-out wire format (0x50), pool integration
crates/lean-lane-node/     the lean node (shim RPC, flat state, append-only log)
crates/malachite-app/      CL: lean lane arms are flag-gated (ARC_PAYMENT_LEAN_LANE)
crates/eth-engine/         lean_shim.rs — the 4-verb client
experiments/dual-el/       lean-smoke.sh · lane-bench.sh · deploy-lean.sh ·
                           gen-lean-fund.sh · lean-feeder.py · fleet.env.example
experiments/dual-el/fleet/ historical launchers (hardcode the old fleet — history, not the live path)
docs/lean-lane-setup.md    fresh-machine recipe
docs/lean-lane-integration.md  shim contract (4 verbs + SYNCING), CL delta, candidate removals
docs/campaign-log.md       full chronological history
2026.arc.payment.highway.claude/  paper + lab notebook (append-only, figures)
```

---

## 8. Session log — REQUIRED

Every working session appends one dated entry to **`docs/worklog.md`** before it
ends. This is how the work stays reviewable remotely (`git log -p docs/worklog.md`),
so it is not optional and not a summary of the chat — it is the record:

- **Changed** — what was edited/built, with commit hashes.
- **Measured** — numbers *with cadence and fullness*, and the config that produced
  them. A number without fullness is not a result (§6).
- **Broke / retracted** — anything that failed, and any earlier claim this session
  invalidated. Retractions are the most valuable lines in the file.
- **Decided** — design decisions taken, with the reason.
- **Open** — what the next session should pick up.

Mark speculation as **BRAINSTORM** so it is never mistaken for measurement.
Durable conclusions get promoted into this file (§5/§6); the worklog stays raw.

---

## 8. Scope — what is done, what is next

**Done and measured.** Dual-lane consensus with one certificate; fan-out transaction type;
structural voting + vote-gap execution + anchored promote; self-healing lean nodes (announce/
pull gossip, SYNCING queue, peer backfill, snapshot recovery); the numbers in §5; repo
self-containment.

**Not done / known limits.** Longest verified soak is 6 h (lean load died at 1.6 h on the
balance drain). n=1 for most points, ±15 % run-to-run. Four validators, heterogeneous, one of
them on wifi. No comparison against a published system. Sync-storm under drain load is
characterised but unfixed. No formal safety argument for the multi-lane commitment.

**Next, in the order I would do them:**

1. **Endurance at the operating point** (one night, unattended) — add `--fanout-amount` to the
   spammer (fixed tiny amounts, so senders don't bankrupt), then 12 h+ at N=50/150M with the
   watchdogs. This is the gap between "measured" and "trustworthy" and is cheap.
2. **Compact proposals** (multi-day, CL-side) — propose tx *hashes* against pre-distributed
   bodies. It is the only remaining structural lever: it attacks the 57 %-of-height proposal
   stream, the p90 tail, *and* the value-sync storm at once. Byte-law estimate: ~10× fewer
   consensus bytes. Note honestly that this is Narwhal's separation applied to an existing BFT
   stack, not a new idea.
3. **Cheap robustness** — fold lean-node teardown into `clean-fleet.sh`; add per-validator
   proposer-turn attribution to the health gate (it catches parked/hung CLs, not slow ones).

**Explicitly parked** (measured, not promising): parallel execution (execution is ~1 µs/
payment); bigger blocks (knee mapped at ~225 M); further packing (28 B/payment is the wire
floor); prebuild/builder-separation beyond v1.1 (needs fork-level pending visibility).
