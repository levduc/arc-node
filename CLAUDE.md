# CLAUDE.md — arc-node working notes

Arc is an EVM-compatible L1: a **customized Reth** execution layer (built on the Reth SDK, git tag `v1.11.3`) driven by the **Malachite** BFT consensus layer over the standard Ethereum **Engine API** (IPC/RPC). See `README.md`, `crates/node/README.md`, `crates/malachite-app/README.md`.

## Toolchain (and gotchas that cost real time)

- **Node.js ≥ 20.19 / 22.x (even versions).** Node 18 breaks the Hardhat genesis step with a misleading `HH19` error — the real cause is `ERR_REQUIRE_ESM` on the ESM-only `@noble/ed25519`. CI uses Node 24. Install via nvm; do not use system Node 18.
- **Rust** per `rust-toolchain.toml` (currently `1.91.1`), via rustup.
- **clang + libclang-dev** are required on the host to build `quake`/the node — `reth-mdbx-sys`'s bindgen needs clang's builtin headers (`stdarg.h`). Missing them gives `failed to run custom build command for reth-mdbx-sys`.
- **Docker** for `make testnet` (builds CL/EL images, runs containers via `quake`).
- **Foundry** pinned in `.foundry-version`.

## Common commands

```bash
make build                 # cargo build the node
make genesis               # generate assets/localdev/genesis.json (idempotent)
make testnet               # genesis + build-docker + quake start (5 validators + 5 CL + monitoring)
make testnet-down          # stop testnet
make testnet-clean         # remove testnet artifacts
make testnet-load RATE=1000 TIME=60   # drive load (wraps quake load)
make test-unit             # rust unit tests + lint
```

`quake` is the in-repo testnet manager (`crates/quake`), run as `cargo run --bin quake -- <args>` (the Makefile wraps this). It is NOT a global binary. Node RPC endpoints for a running local testnet are in `.quake/localdev/nodes.json` (validator1 EL: http `8545`, ws `8546`).

## Execution-throughput benchmarking

Two tools measure execution-layer performance:

- **`arc-engine-bench`** (`crates/engine-bench`) — replays real Arc blocks into a fresh node via `engine_newPayloadV4` + `engine_forkchoiceUpdatedV3` and reports per-block + aggregate import throughput. This is the apples-to-apples way to compare execution clients (no consensus gating).
- **`spammer`** (`crates/spammer`) / `quake load` — generates transaction load. ERC20 transfer load: `spammer ws --targets ws://127.0.0.1:8546 -r 4000 -g 4 -t 60 --mix erc20=100`.

A end-to-end baseline harness lives at **`scripts/bench/engine-baseline.sh`**. Workflow: drive load into a running testnet → `prepare-payload --from 1 --to <head>` → replay into a fresh target node booted with the **testnet's own genesis** (`.quake/localdev/assets/genesis.json`) → read results.

Benchmarking gotchas:
- The replay source must be **real consensus blocks**, not a reth `--dev` node — engine-bench assumes Arc's convention `parent_beacon_block_root = parent execution block hash` (`crates/eth-engine/src/engine.rs`), which dev blocks violate (→ "block hash mismatch").
- The target node's genesis must exactly match the source testnet's, or you get an "expected parent block 0" mismatch.
- Run bench nodes on non-testnet ports (e.g. http `7545`, authrpc `7552`, p2p `30402`); never `pkill -f "arc-node-execution node"` (it matches the testnet containers).
- `summary.csv` averages are diluted by empty blocks — filter `combined_latency.csv` to `gas_used > 0` for the real number.

### Key finding (ERC20 workload)

For ERC20-transfer block import, the per-stage cost split (from Reth's `reth_tree_root_*` / `reth_sync_execution_*` / persistence metrics) is roughly: **EVM execution ~4%, trie/state-root ~58%, disk persistence ~38%.** EVM execution is not the bottleneck — state-root/merklization dominates. Implication for alternative execution clients (e.g. Galxe's gravity-reth): a faster *parallel EVM* (grevm) targets the ~4% slice; *parallel merklization* targets the ~58% slice.

Validated on **real testnet state** (169 GB snapshot, block 48.75M): a 2.7M-gas block imports in **27.5 ms (97 Mgas/s)** vs ~1,900 Mgas/s on shallow state — **~20× slower**, with merkle/state-root **27.8 ms** and persistence **24.5 ms** dominating, EVM execution only **4 ms**. A warm re-replay (trie pages cached) halves it to 11.3 ms (235 Mgas/s), proving ~60% of merkle is cold disk-I/O. This is exactly what **reth 2.0's Sparse Trie Cache + Partial Proofs** target (published ~2 ms state-root), making a reth-2.0 upgrade the highest-value execution lever for Arc.

## reth 2.0 upgrade — attempt log

Goal: port Arc's execution layer from reth 1.11.3 → reth 2.0 (the upstream, maintained fix for the state-root/I-O bottleneck above). This is a multi-week port, NOT a dependency bump. Status: **LAYER 1 COMPLETE — `cargo generate-lockfile` resolves clean (exit 0, 0 errors, 103 reth crates at v2.0.0). Compilation not yet attempted (blocked on layer 2).** Branch: `reth-2.0-layer1`.

How layer 1 was resolved: repointed 42 `reth-*` deps to `tag = "v2.0.0"`; aliased the removed crate via `reth-primitives-traits = { ..., package = "reth-ethereum-primitives" }` (placeholder so the lockfile resolves — the real per-type relocation is a layer-2 task); bumped `rust-toolchain.toml` 1.91.1 → 1.93.0. **The resolve immediately surfaced the layer-2 wall: the lockfile now contains BOTH revm 34 (Arc's own EVM crates) AND revm 36 (reth 2.0) — they don't unify.** Arc's revm sub-crate pins are revm-34-era (`revm-interpreter = 32`, `revm-primitives = 22`, `revm-context-interface = 14`); reth 2.0 needs the revm-36-era set. So step 1 of layer 2 is bumping ALL `revm-*` workspace pins to reth-2.0's exact versions, then porting Arc's EVM code to the new revm API.

The 5 layers of the port (in order):
1. **Crate-source reconciliation** — reth crates are git-only (NOT on crates.io; verified). Of Arc's 42 `reth-*` deps, **40/42 exist at git tag `v2.0.0`**; only **`reth-primitives-traits` is removed** (primitives reorganized under `crates/ethereum/primitives`, `crates/payload/primitives`, `crates/engine/primitives`; its traits likely moved to alloy / `reth-ethereum-primitives` — TODO: find new import paths). Earlier mechanical `sed v1.11.3→v2.0.0` failed *only* on that one dangling crate. Scope of the relocation is small: Arc imports just **~8 types** from `reth_primitives_traits` across **~16 sites** (`SignerRecoverable`, `RecoveredBlock`, `SealedHeader`, `NodePrimitives`, `SealedBlock`, `Recovered`, `Block`, `InvalidTransactionError`) — `grep -rn reth_primitives_traits:: crates/` to find them; repoint to their reth-2.0 home.
2. **revm 34 → 36** — Arc's `crates/evm` (handler.rs, evm.rs, opcode.rs, log.rs, precompiles) is written against revm 34's `Handler`/`Context`/`Evm` API; revm 36 has breaking changes. Bulk of the work #1.
3. **reth SDK 1.11 → 2.0** — `ConfigureEvm`, `BlockExecutorFactory` (alloy-evm), node builder, payload primitives, and **Storage V2** APIs changed; port `crates/node`, `crates/evm-node`, `crates/execution-config`. Bulk of the work #2.
4. **Toolchain** — rust 1.91.1 → 1.93 (reth 2.0 `rust-version`), bump alloy 1.6.3 → ~1.8.2 (resolves up automatically as same-major).
5. **Datadir migration** — existing nodes' MDBX data is Storage-V1; reth 2.0 defaults to Storage V2 (`storage_v2: true`), so a re-sync/migration from snapshot is required.

Reference clone at `/home/papaduck/reth-2.0-ref` (git tag v2.0.0). Crate name list: `/tmp/reth20_names.txt`.

### PROGRESS LOG (most recent first)
- **🎯 LIVE 4-NODE reth-2.3 + MALACHITE CONSENSUS TESTNET — WORKS END-TO-END (2026-06-26).** Both binaries compile on reth-2.3 (EL arc-node-execution + CL arc-node-consensus; CL needed only PayloadAttributes.slot_number since CL↔EL is just the Engine API). Built reth-2.3 Docker images (Dockerfile base rust 1.91→1.93). Also bumped workspace reqwest 0.12→0.13 (alloy-2.0 pulls 0.13) + renamed reqwest TLS features (rustls-tls→rustls, rustls-tls-native-roots→rustls, add json feature) so quake/arc-checks build. Ran `quake -f crates/quake/scenarios/localdev4.toml start` (new 4-validator manifest). RESULT: 4 validators (CL+EL) produce blocks via Malachite BFT (~2 blocks/s), reth-2.3 EL executes with storage_v2=true on FRESH chain (no migration). ERC20 spam (45s): 73,595 txs ALL landed in 88 blocks, many FULL at 30M gas / 868 ERC20 transfers (0xa9059cbb), state-root ~277µs shallow. → reth-2.3+Malachite operates as a normal consensus node. GOTCHA: `cast block` in a tight loop times out on a busy node — use direct eth_getBlockByNumber RPC. TODO: "join public Arc testnet" (needs public CL bootnodes/chainspec + V1→V2 migration or genesis sync + live P2P).
- **reth 2.3 PORT COMPLETE — full binary typechecks (2026-06-26), branch `reth-2.3-upgrade`.** `cargo check -p arc-node-execution` = 0 errors against reth 2.3 / revm 40 / alloy 2.0.5 / ssz 0.10. Deltas beyond reth-2.0: arc-precompiles (revm-40 redesign: PrecompileError=Fatal-only, failures→PrecompileOutput.status Revert/Halt, new(g,b,0)/revert()/halt(PrecompileHalt::OutOfGas)); arc-evm (BlockExecutor new Result/TxResult, commit_transaction→GasOutput, set_state_hook REMOVED→reth installs at DB layer via State::set_state_hook so Arc's on_state forwarders were redundant, SELFDESTRUCT opcode→revm-40 Err-return contract, known_bytecode now (B256,Bytecode) executed directly, Send bounds, create_executor sig); arc-consensus-types (ssz 0.9→0.10 to match alloy 2.0 ExecutionPayloadV3 Encode); arc-execution-validation (ConsensusError::Other→msg(), validate_block_post_execution 5-arg+whole-result); arc-execution-payload (parent_block_info, slot_number=None, mark_invalid by-value, gas_used.tx_gas_used(), EthBuiltPayload::new 4-arg=Arc::new(block)+None); arc-evm-node (RpcAddOns::new 6-arg, slot_number). Building /tmp/arc-reth23. NOT VALIDATED: consensus-correctness (esp. SELFDESTRUCT opcode + known_bytecode) needs differential replay. Ref: /home/papaduck/reth-2.3-ref, cargo checkout 9384bc5.
- **🎯 STORAGE-V2 WIN MEASURED ON REAL ARC STATE (2026-06-26).** Migrated the 169GB V1 snapshot → V2 via `arc-node-execution db migrate-v2` (44min: changesets→static files, history/tx-hash→RocksDB, MDBX 142→58GB, flip storage_v2=true), then merkle rebuild on boot (~12min, MerkleExecute re-merklizes from hashed state). GOTCHAS (pruned snapshot): migrate-v2 reset SenderRecovery to 0 but txs only exist from 48.5M (pruned) — set checkpoint via `db stage-checkpoints set --stage sender-recovery`; the TransactionSenders static file is empty/buggy (reth-2.3 migrate-v2 + pruned bug: "append as block #0 but expected 48500000") so LIVE persistence of new blocks crashes — WORKAROUND: SenderRecovery=tip + boot with `--engine.persistence-threshold 20000 --engine.memory-block-buffer-target 20000` so blocks import IN-MEMORY (state-root computed pre-persistence). RESULT replaying 300 real blocks (48835032+) into reth-2.3 V2: **state-root 2.44ms avg (vs V1 cold 27.8ms = 11.4x, vs V1 warm 11.3ms = 4.6x)**; block import newPayload 4.89ms (vs V1 cold 27.5ms = 5.6x, vs legacy-V1 warm 9.7ms = 2x); 373 Mgas/s. Matches reth's published ~2ms state-root target ON REAL ARC STATE. Replay imported all 300 with correct hashes → also consensus-validates the reth-2.3 port + the migration. CAVEAT: V2 ran in-memory (persistence buffered due to senders bug) — newPayload/state-root are the synchronous compute (persistence is async anyway) so comparison is sound for execution+state-root; full on-disk persistence in V2 blocked by the senders static-file bug on this pruned snapshot.
- **reth-2.0 port VALIDATED + head-to-head MEASURED (2026-06-26).** Release binaries: reth-1.11 `/tmp/arc-reth1`, reth-2.0 `/tmp/arc-reth2`, engine-bench `/tmp/bench-reth1`. 176-block loaded-ERC20 fixture `/tmp/exp-fixture` (genesis `/tmp/exp-genesis.json`, block0 0xc7e3c695...). Replayed SAME fixture into both (isolated). reth-2.0 accepted all 176 blocks with IDENTICAL hashes/state-roots → **differential replay validated the port is consensus-correct** (storage_v2=true). Throughput IDENTICAL on shallow state: reth-1.11 2154 Mgas/s, reth-2.0 2129 Mgas/s. EXPECTED — reth-2.x's Sparse-Trie-Cache win only appears when state EXCEEDS RAM (62GB); a fresh chain can't reach it. Win already measured via warm/cold on real 169GB: 27.8ms cold vs 11.3ms warm = 2.5x.
- **reth 2.3 bump (branch `reth-2.3-upgrade`): deps resolve, arc-evm has 27 revm-40 errors.** 2.3 = revm 36→40 + alloy 1.6→2.0.5 + reth-primitives-traits 0.4.1. GOTCHA: pin alloy `~2.0.5` (2.1.0 adds EthPayloadAttributes.target_gas_limit, breaks reth's reth-engine-local); revm-statetest-types=19.0.3. arc-evm errors are revm-40 precompile API: `PrecompileOutput::new_reverted`/`reverted` gone, `PrecompileError::{OutOfGas,Other}` renamed — find new names in revm-precompile 36.x. Grindable batch, plus alloy-2.0 changes still to surface. NOTE: 2.3 also won't change the shallow benchmark.
- **STRATEGIC: to see the win on Arc needs LARGE (RAM-exceeding >62GB) state.** Rebuild-from-genesis = TOO MUCH (~48M public-testnet blocks, days). BEST PATH (confirmed feasible-ish): reth 2.x AUTO-DETECTS storage version from existing datadir metadata (reth source `crates/storage/db-common/src/init.rs:170` reads stored settings, defaults v1; `EnvironmentArgs::storage_settings` honors `--storage.v2` for NEW dbs only) and supports `StorageSettings::v1()` legacy mode. So the reth-2.0 binary can likely OPEN the reth-1.11 V1 169GB snapshot in legacy mode — gets the in-memory Sparse-Trie-Cache win (~2.5x, the main one) but NOT the on-disk Storage-V2 win (needs migration). Unknown: reth-2.x legacy on-disk table compat with reth-1.11 exact format. TEST: `arc-snapshots download --chain arc-testnet` (~1-2h, EL 169GB), then `/tmp/arc-reth2 node --chain arc-testnet --datadir <snapshot> [--storage.v2 false]`, then engine-bench replay real blocks 48.75M+ → measure vs reth-1.11 baseline (27.8ms cold). The reth-2.0 binary is consensus-correct so it CAN execute Arc blocks.
- **🎯 FULL `arc-node-execution` BINARY BUILDS + RUNS ON reth 2.0** (`cargo build --bin arc-node-execution` → 2.4GB binary; `--version`/`node --help` work). All crates ported: arc-evm, arc-execution-payload, arc-evm-node, and the rest (execution-config/txpool/validation, types, node) compiled with the recurring fixes. Recurring patterns: (1) `EthPayloadBuilderAttributes`→`EthPayloadAttributes` + drop `PayloadTypes::PayloadBuilderAttributes` assoc type; payload_id moved attributes→PayloadConfig; BuildArguments gained execution_cache/trie_handle (sparse-trie cache, passed None); `BlockBuilder::finish(sp, None)`; `EthBuiltPayload::new` dropped id; attributes method→field access. (2) `TaskExecutor` is now a type alias for `reth_tasks::Runtime` (drop Box). (3) `builder.extra_data` is `Bytes` → use `default_extra_data_bytes()`.
  **NOT YET VALIDATED:** compiles + runs ≠ consensus-correct. Unverified: differential block/state-root output; the `set_state_clear_flag`-removal and sparse-trie-cache-as-None choices need Arc's differential/e2e suite. Also: reth 2.0 uses **Storage V2** so it CANNOT read the old reth-1.11 169GB Arc snapshot — running it needs a FRESH genesis/chain. So a real reth-2.0-on-Arc number is now *reachable* (fresh chain + load + engine-bench), but NOT on the existing snapshot, and only trustworthy after differential validation.
- **`arc-evm` COMPILES against reth 2.0 / revm 36 / alloy-evm 0.30 (10 errors → 0).** This is the core/hardest crate (all EVM customizations). Fixes: `TxResult::into_result` added; revm-36 `Bytecode` struct (`eip7702_address()`/`new_eip7702()`); state-clear now unconditional; and the big one — alloy-evm 0.30 **DB model**: executor DB is bounded by `StateDB` (Database+DatabaseCommit) directly, not `&mut State<DB>`. Updated `create_executor` (`Evm<DB,I>` / `DB: StateDB` / `Inspector<Context<DB>>`), both `ArcBlockExecutor` impl blocks (`E: Evm<DB: StateDB>`), and `builder_for_next_block`'s `BlockExecutorFor` DB arg → `&'a mut State<DB>`. Commits `87e643c` and earlier. (2 cosmetic warnings left: a test-only `State` import, deprecated `map_frame`→`map_item`.)
- **Next crate: `arc-execution-payload` (9 errors)** — reth 2.0 payload-builder API: `EthPayloadBuilderAttributes`/`PayloadBuilderAttributes` moved, `PayloadTypes::PayloadBuilderAttributes` assoc-type change, changed fn arities, and structs that gained fields (`execution_cache`, `trie_handle`, `payload_id`) needing pattern updates. After that: `execution-config/txpool/validation`, `node`, `malachite-app`. Same pattern, different API surfaces. The compile front is moving ~1 crate at a time; consensus-correctness validation (differential/e2e) is still the separate multi-week cost.
- **arc-evm: 10 → 8 errors.** Fixed 3 revm-36 API breaks (verified against revm source in `~/.cargo/registry/src/.../revm-*-{9,12,16,17}.0.0`): (a) `Bytecode` is now an opaque struct — use `eip7702_address()`/`new_eip7702()`; (b) `set_state_clear_flag` removed, EIP-161 clearing is unconditional in revm 36 (`revm-database` CacheState `// EIP-161 state clear`), so dropping the `(true)` call is behavior-preserving. Remaining 8 are STRUCTURAL reth-2.0-SDK (alloy-evm 0.30): `BlockExecutor::into_result` (must ADD per-tx `ResultAndState` accumulation to `ArcBlockExecutor` — it only tracks receipts/gas_used; ref impl `EthBlockExecutor::into_result` returns `self.result`), `create_executor` signature change, and `Evm`/`Inspector<Context<...>>`/`DatabaseCommit` generic re-wiring (evm.rs:1787/1827/367). These touch block-execution result/state → consensus-critical; need Arc's differential test suite to validate, NOT just compilation. Do not blind-edit.
- **Layer 1 + 2a DONE & COMMITTED** (`c80c70d`, `2799354`, plus reth-primitives-traits/DownloadDefaults fix): deps resolve, revm unified to single 36. Key fixes: `reth-primitives-traits` moved to crates.io (use `version = "0.1"` not git — independently versioned 0.1.x, NOT removed); reth 2.0's `DownloadDefaults` (in `reth-cli-commands`) gained required `snapshot_api_url: Cow<'static,str>` field.
- **`cargo check -p arc-evm` now compiles the entire reth-2.0 + revm-36 tree and gets down to 10 real API errors** (all in `crates/evm/src/evm.rs` + `executor.rs`) — the revm-34→36 EVM-code port. These are SECURITY-SENSITIVE (EIP-7702 delegation, state-clear, block execution) — port carefully with revm 36 docs, do NOT guess. The 10:
  - `executor.rs:360` `set_state_clear_flag` — removed/renamed on revm 36 `State` (find successor in revm-database 12).
  - `evm.rs:986,4047-4049` `Bytecode::Eip7702(..)` — revm 36 made `Bytecode` a struct (was enum); use the new accessor/constructor in revm-bytecode 9.
  - `executor.rs:80` `create_executor` incompatible trait signature (alloy-evm 0.30 `BlockExecutorFactory`).
  - `executor.rs:355` missing `into_result` — `BlockExecutor` trait gained a method to implement.
  - `evm.rs:1786,1788,1826,1827` `Inspector<revm::Context<...>>` / `DB: DatabaseCommit` bound failures + `BlockExecutor::Evm == ArcEvm` type mismatches — revm 36 Inspector/Evm generics changed.
- After arc-evm: expect similar in `crates/evm-node`, `crates/execution-*`, then layer 3 (reth SDK node-builder/payload/Storage-V2), then datadir migration.

HONEST STATUS: dependency + reconciliation layers are DONE (big milestone — from "won't resolve" to "10 specific core-EVM API errors"). Remaining = deep semantic port of Arc's EVM executor to revm 36 + reth-2.0 SDK; multi-day, human-reviewed (consensus-critical). No working build yet, so **no real "reth 2.0 on Arc delivery number" exists** — do not fabricate one. Closest real proxies: warm-cache 235 Mgas/s (Arc reth 1.11) and reth 2.0's published ~2 ms state-root / 1.7 Ggas/s on large blocks.

## Payment-lane / UTXO experiment (branch `utxo-state-experiment`)

Goal: test the paper's payment-sector thesis empirically — can a UTXO payment state (a) stay in RAM
as tx volume grows, (b) be committed cheaply, and (c) beat reth + simple native (account) transfers?
Standalone crate **`experiments/utxo-state/`** (own `[workspace]`, detached from the reth fork; deps
cached: alloy-primitives, alloy-trie, k256/ecdsa, curve25519-dalek-ng, rayon). Bins
(`cargo run --release --bin <name>`):

- `utxo-bench` — RAM UTXO set + 3 commitment schemes. **25M UTXOs = 3.4 GB RAM; ECMH commit flat
  ~259 ms/block across a 250× state increase; MPT/Merkle full roots are O(n).**
- `merkle_acc` — hash-only incremental Merkle accumulator (no DLog). **Flat ~325 ms/block to 25M,
  5.5 GB RAM, ~11 µs/update** (constant depth). Hash-only commitment is forced if you drop DLog (the
  additive multiset hash is broken by Wagner's k-sum; ECMH needs a group).
- `two_el` — two-EL/two-root prototype: a mock CL fans out to an MPT-rooted EVM-EL and an ECMH-rooted
  UTXO Payment-EL **in parallel** and merges both roots into one header. Shows **block time =
  max(T_evm, T_pay)**; lane ~free while T_pay < T_evm (crossover ~10k payments/block at T_evm≈140 ms).
- `utxo_tx` — real secp256k1-signed UTXO payments end-to-end. **~8.5k tx/s single-thread, verify =
  75% of cost (~82 µs k256 ECDSA); state+Merkle ~3× cheaper.**
- `compare` — UTXO vs account-native, single-thread, same secp256k1+Merkle. **~equal (account
  marginally faster: fewer state writes).**
- `parallel_cmp` — parallel throughput. ~104k tx/s both, ~9× scaling. **(Had a bug — see below.)**
- `contention` — the corrected contention test (real balance RMW).

### KEY FINDINGS (honest — all measured on i7-11700F 8C/16T, 62 GB, testnet stopped for parallel runs)
- **For SIMPLE payments, UTXO is NOT faster than reth-style native transfers.** Single-thread and
  parallel they are ~equal (~105k tx/s on 16 threads), both **signature-verification-bound** (~9 µs/tx
  parallel k256 ECDSA; libsecp256k1 ~2× faster). Verification is contention-free and parallelizes
  identically for both models.
- **Account contention is real but invisible for simple transfers.** `contention` models REAL balance
  read-modify-write: hot recipients give 97% conflict and a hot account touched 1309× (non-commutative).
  Throughput is unchanged because the contended work (a balance add) is ~0 µs vs 9 µs/tx verify.
  Contention only bites when the contended *work* is expensive (contracts).
- **BUG I made and fixed (do not repeat):** the first `parallel_cmp` "account" path wrote a CONSTANT
  leaf and batched changes like UTXO — it never modeled RMW or hot-account serialization, so its
  "account == UTXO" was an artifact. `contention` fixes it. Also fixed a parallel-Merkle early-`break`
  that skipped levels to the root when changes collapsed into one subtree.
- **Where UTXO actually wins (architecture, not per-tx speed):** (1) a small **RAM-resident** payment
  lane avoids reth's disk-bound 169 GB shared MPT (App C: 0.1–0.37 Ggas/s on real state vs ~105k tx/s
  in RAM); (2) it **avoids the Block-STM contention machinery** (deterministic parallel, zero aborts —
  robust under adversarial contention); (3) conflict-free parallelism for EXPENSIVE/contract execution.
  The win is **isolation + small RAM state**, not "UTXO > account".
- **Commitment choice:** Merkle is fine. ECMH (32 B, O(1)) needs DLog → rejected. Hash-only ⇒ incremental
  Merkle (additive multiset hash is k-sum-broken). Mature alternatives: Bitcoin Core **MuHash**
  (incremental multiset hash, group-based, what `coinstatsindex` ships) or **libbitcoinkernel** (C++/FFI,
  full Bitcoin tx/Script semantics; needs cmake — NOT installed here). No reth-style EL uses Utreexo;
  Utreexo lives in Bitcoin nodes (utreexod, Floresta/`rustreexo`).
- **How Bitcoin scales sig verify (no batching):** libsecp256k1 + parallel `CCheckQueue` + **signature
  cache** (verify-once at mempool, skip at block connect). reth caches recovered senders similarly.

- `locality` — locality-keyed vs hash-keyed state+commitment (same Merkle shape, only the node
  store differs: contiguous Vec vs HashMap by node index = MPT-style). **Dense beats sparse
  1.1×→1.4× and the gap WIDENS with size** (dense −12% vs sparse −30% as it spills cache, to 12M
  entries). Honest scope: 1.4× is RAM-only (cache effect) and understates it — a real MPT keys by
  `keccak(addr)` (fully random) and pays on DISK (the 27.8 ms cold). So the data-structure win is
  modest in RAM (~1.4×), large on disk.

### REFINED THESIS (after the account-vs-UTXO + locality work — supersedes "UTXO is better")
- The 169 GB / 27.8 ms problem is the **shared general-purpose hash-keyed MPT**, not payment data.
- **First-order fix = ISOLATION:** a dedicated payment lane whose state grows with USERS (not tx
  volume or contract activity) → small → fits in RAM → no disk-bound merklization. Works for an
  account lane too; it is NOT a UTXO-specific win.
- **Pragmatic implementation = reuse reth** (EVM account + Block-STM + mempool + sender cache):
  payment-only account state is compact (≤ UTXO, ~1 entry/user), throughput = UTXO (~105k tx/s,
  signature-bound), and reth's MPT on a SMALL RAM state costs only ~1.4× vs an ideal structure —
  acceptable. This is the low-risk, high-reuse design.
- **Secondary refinement = a locality-keyed, unified state+commitment structure** (dense Merkle /
  Jellyfish-MT / Verkle): ~1.4× in RAM, more on disk; worth it at scale, not required.
- **UTXO is an ALTERNATIVE lane**, justified only by deterministic conflict-free parallelism (no
  Block-STM machinery/aborts) + statelessness — NOT speed or state size (both ≈ or worse than a
  minimal account lane). Do not pitch UTXO as faster/smaller.
- Architecture unchanged: second minimal EL + one extra header root; CL fans out + combines;
  block time = max(T_evm, T_pay), payment lane ~free while T_pay < T_evm.

DO NOT fabricate numbers. k256 (pure Rust) is ~2× slower than libsecp256k1.

### PAPER WRITING STATE (paper repo: `2026.arc.payment.highway.claude/main.tex`, Overleaf-connected)
The paper is being rewritten to a HONEST, NON-UTXO-LEANING framing the user approved:
**(1) the bottleneck is the shared general-purpose disk-bound MPT; (2) first-order fix = ISOLATE
payments into a lean lane (grows with users, fits in RAM); (3) pragmatic implementation = REUSE
RETH (account + Block-STM, Approach 2); (4) UTXO is an HONEST ALTERNATIVE (deterministic
conflict-free parallelism + statelessness), NOT faster/smaller; (5) the RESEARCH FRONT = the
COMMITMENT for the lean EVM state (unify store+commitment, locality-key it, cheap as it scales
past RAM, cheap proofs).** Push to Overleaf after each compile-clean change.

Done so far (all pushed, compiles clean ~25 pp): App C (reth-2.3 measurements), App D (reality
check), **App E "A payment lane in RAM"** (the experiments/utxo-state results — measured, honest
ranges, variance noted, reproducible via the crate README); **§4 pivot** (recommend Approach 2 /
reuse-reth, UTXO as honest alternative, tied to App E); **Research Questions** now lead with
"the commitment for the lean EVM state" (\S sec:rq-commitment) as the central front.

VERIFICATION (user demanded no fake numbers / reproducible): re-ran all experiments; logical
results are deterministic and reproduce exactly (utxo-bench 25M = **3.42 GB**, merkle_acc 25M =
**5.52 GB** both reconfirmed to the decimal). Timings vary ±~15% run-to-run; parallel throughput
is load-sensitive (92k contended vs 105k idle) — paper now states ranges, not point values.

DONE (end-to-end consistency pass, all pushed, compiles clean 25 pp): read the whole paper;
removed every UTXO-lean spot (abstract, §2 RQ list, §3 forward-ref, §4.3 "State and commitment"
now account-first, §4 takeaway now RECOMMENDS Approach 2, open-questions list). Moved the
memory/storage **schematic into the body §3** (replaces the stage-bar as `fig:exec-anatomy`,
"why I/O is bad"); App C keeps the cold/warm/V2 **bar chart**. Consolidated **§4.4** into a
"three measurements, one conclusion" synthesis table (reth shallow/big + Block-STM + UTXO-vs-account
→ isolate + reuse-reth + commitment-is-the-lever); moved the detailed Block-STM per-thread table to
**App E item 7**. Verified numbers reproduce (3.42 / 5.52 GB exact; timings stated as ranges).
Paper now consistent end-to-end: isolation + reuse-reth thesis; UTXO an alternative; the commitment
for the lean EVM state is the central research front. Avoid re-introducing UTXO-leaning or unmeasured
numbers in future edits.

### Dual-EL payment-lane implementation (branch `dual-el-payment-lane`)
Goal: each node = 1 CL (Malachite) + 2 ELs (EVM-EL + lean payment-EL, both reth); every block carries
TWO roots (evmStateRoot + paymentRoot), both built by the proposer and re-executed by every validator;
a dual-lane spammer stress-tests both. This is the paper's full design — **multi-day, consensus-critical**
(NOT one-night). Full file-by-file plan in **`docs/dual-el-payment-lane.md`** (commit e32d77c). Status:
branch + blueprint done; **no consensus code written yet** (deliberately — the riskiest piece, step 3, is
an SSZ + proposal-streaming change to `ConsensusBlock` that breaks consensus if rushed). Step order
(low-risk first): (1) 2nd Engine additive in eth-engine/config; (2) CLI+quake 2nd EL endpoint + 2nd
genesis; (3) `ConsensusBlock.payment_payload` field [HIGH risk]; (4) proposer builds both; (5) validators
re-execute both; (6) dual-lane spammer. Each consensus change must pass differential replay (all
validators compute identical paymentRoot) before trusting it.

**STEP 2 DONE + VERIFIED (2026-06-28):** `experiments/dual-el/launch-payment-els.sh` launches a 2nd
reth EL ("payment lane") per validator on the running localdev4 testnet (real `arc_execution` image,
own datadir `reth-pay`, genesis, ports http 19545/19645/19745/19845, discovery off, on
`arc_testnet_host-access` — NOT the `internal:true` `arc_testnet_default`, or published ports don't
route). Verified: 4 payment ELs up, RPC chainId=1337 block=0 (idle), while EVM ELs advance (blk 213+).
Gotcha: localdev EL uses IPC auth (no `jwtsecret` file) → don't pass `--authrpc.jwtsecret`; let reth
auto-gen `<datadir>/jwt.hex`. **STEP 1 NOT done** (CL driving EL2): coupled to step 4 and blocked by
the consensus Docker image rebuild (Cargo.toml reth `file://` fork unreachable in Docker build) —
do steps 1+3+4 together next. The base testnet here was started with `-e 50000 --monitoring false`
(blockscout fails otherwise); validator1 sometimes doesn't boot (3/4 is enough for BFT).

**STEPS 3+4 DONE — consensus code for two-root blocks compiles + tested (2026-06-28, branch `dual-el-payment-lane`).**
- STEP 3 (commit ad95465): `ConsensusBlock.payment_payload: Option<ExecutionPayloadV3>`; `SszBlock`
  7→8 tuple (ethereum_ssz supports Tuple9, verified); proposal streaming length-frames the two lanes
  (`[u64 len(evm)][evm ssz][payment ssz?]`); store encode/decode carry it. Round-trip test +82 db tests pass.
- STEP 4 (commit 0ef9046): CL builds+validates+finalizes BOTH ELs. config/CLI gained `payment_*`
  endpoint flags + `payment_engine_config()`; node.rs connects an optional 2nd `Engine`; threaded
  `payment_engine: Option<&Engine>` through run→go→handlers (started_round/get_value/
  received_proposal_part/process_synced_value/decided). `build_block` builds the payment payload on
  EL2's own head (`get_block_by_number("latest")`) aligned to the EVM timestamp; `validate_consensus_block`
  re-executes it (newPayload) — block valid only if BOTH lanes validate; `decide` forkchoices EL2 to the
  decided payment block. Single-EL path unchanged (None default). lib+bin+tests compile; 261 lib tests pass.
- **v0 limit:** value-sync carries only the EVM payload (synced blocks have no payment lane); the live
  proposal-streaming path carries both. NOT yet differential-validated on a live testnet.
- **TO RUN (remaining):** (a) payment genesis with blockGasLimit=100M; (b) revert the 41 reth `file://`
  deps in Cargo.toml → upstream `tag=v2.3.0` so the CL Docker image rebuilds (backup
  /tmp/Cargo.toml.preforkpatch); rebuild CL+EL images; (c) quake passes `--payment-execution-endpoint`
  (+ws/jwt) to each CL pointing at its payment EL; (d) boot, drive both lanes (dual spammer), confirm
  all validators compute identical paymentRoot. The Docker rebuild is the long pole.

**🎯 value_id NOW COMMITS TO BOTH LANES + 24h LIVE SOAK PASS (2026-07-02, commit `4c86b4f`).**
Closes the equivocation hole where the certified consensus value bound only the EVM lane (a proposer
could stream two payment payloads under one EVM hash → fork the payment lane beneath a valid EVM
commit certificate). Now `value_id = keccak(evm_block_hash ‖ payment_block_hash)` when a payment lane
is present, else exactly the EVM hash (single-EL blocks byte-for-byte unchanged, no migration).
- **types/block.rs:** `commit_lanes(evm, Option<pay>)` = single source of truth; `ConsensusBlock::value_id()`
  / `payment_block_hash()` (block_hash() stays the real EVM hash for EL ops); the two `From` impls
  (`ProposedValue`/`LocallyProposedValue`) vote on `value_id()`; `DecidedBlock` carries `payment_payload`,
  `new()` re-checks the commitment vs the certificate, `from_stored_evm_only()` for DB reconstruction;
  extracted `frame_lanes`/`unframe_lanes` (shared by streaming + sync).
- **consensus-db/store.rs:** undecided blocks keyed by `value_id` (not EVM hash) so the decide-path
  lookup by `certificate.value_id` resolves; DB reconstruction uses `from_stored_evm_only`.
- **sync now carries BOTH lanes (closes the v0 gap above):** `process_synced_value` unframes both,
  re-validates the payment lane, dedups by value_id; `get_decided_values`+`app.rs` thread
  `payment_engine`, fetch EL2 payloads per height, verify the fetched lanes reproduce the certificate
  value_id before shipping, frame both lanes. Decided store still persists EVM-only (no schema
  migration); the authoritative both-lane commitment lives in `certificate.value_id`, payment block
  is canonical in EL2.
- **Tests:** 6 new `block::tests` incl. the equivocation guard (`value_id_changes_when_only_payment_lane_changes`),
  order-sensitivity, frame/unframe round-trips. types 167 + consensus-db 82 + consensus lib 261 pass.
- **24h SOAK PASS:** rebuilt the CL image from this commit, ran `soak4` (4× CL+EVM-EL+payment-EL) with
  continuous dual-lane spam (300 tx/s/lane). **1422/1422 60s-checks, 0 failures over 86,400s**; both
  lanes lockstep to ~329k blocks; per-lane block hash identical across all 4 validators at every settled
  height (→ all validators computed an identical value_id every height); 12/12 containers, disk fine.
  A nondeterministic/mismatched commitment would have halted or forked within minutes — it did not.
  Harness: `experiments/dual-el/soak.sh` (this run used a path-adapted copy for the `arc-node-paymentlane`
  clone). NOTE: two clones of this branch exist — `/home/papaduck/arc-node` (older, prior soak) and
  `/home/papaduck/arc-node-paymentlane` (this commit); they have diverged.

**DUAL-EL DEMO TOOLING + STATE-BLOAT/PRESEED RUNS (2026-07-10..13, commits e91ff14..8a36e23+).**
Tooling (experiments/dual-el/): `demo.sh` (start/stop/status: quake soak4 + payment ELs + dual load +
dashboard; stop DELETES datadirs), `demo-bloat.sh` (state-experiment variant: preseeds 10M payment
accounts in genesis at start — regenerated EVERY run — guzzler bloat on EVM lane, pool-update load on
payment lane, memory caps applied at start), `demo-empty.sh` (clean chain, no preseed/load),
`dashboard.py` (localhost:8080: per-lane block/root/tx cards, value_id callout via cast keccak,
state-vs-history split [state=db/ = what a pruned snapshot ships; history=static_files+rocksdb minus
RocksDB WAL churn], touched-accounts counter, side-by-side headers, per-lane exec_ms+root_ms from reth
prometheus + disk I/O + RAM from cgroups, block-production rate; settled height anchors on MAX head so
an offline validator can't pin the display; "agree"=no-fork at settled height, NOT all-in-sync).
Spammer additions: `--fresh-recipients` (every transfer to a new address = new account/tx; wall-time-
seeded counter), `--recipient-pool base:size` (seq walk w/ wrap: first pass creates pool, then pure
balance updates — separates onboarding from steady-state payments). Payment ELs gossip via static
admin_addPeer mesh (launch-payment-els.sh; un-fed validator otherwise proposes empty payment blocks on
its round-robin turn). PAYMENT_GENESIS env selects payment-lane genesis.

KEY FINDINGS (all measured; trend log ~/dualel-bloat-trend.log):
1. **Genesis-resident alloc RAM**: reth keeps the entire genesis alloc in process RSS forever
   (~250B/account): 10M preseed = ~2.7G/EL, 30M = ~8G/EL, on top of disk state (10M=0.66GB, 30M=2.1GB
   MDBX) and working caches. 100M preseed = 7.2GB genesis JSON, ~30-60min parse+root per EL BOOT (OOM
   cascade if boots overlap; must boot sequentially, uncapped, then clamp). Production: don't preseed
   huge allocs; organic/tx-created accounts live only in DB.
2. **Proposer build-failure crash-exit (FIXED 509f404)**: payment getPayload timeout during get_value
   crashed the CL → restart-loop on same height → quorum loss + stall. Fix: log + skip round (same as
   existing None path). Confined to our get_value.rs handler, no consensus semantics.
3. **Value-sync batch livelock (OPEN)**: both-lane sync batches (10 heights × ~500tx payloads ≈ 2.2MiB)
   exceed client request timeout under load → lagging validator loops request/timeout forever and its
   request storm taxes healthy peers. Fix pending: value_sync.batch_size 10→~3 in scenario config.
   (Sync itself validated live: val2 caught up 2000 heights with both lanes re-validated — 4c86b4f.)
4. **Shared-box ceiling + caps**: 12 nodes on 62G thrash-froze uncapped under heavy state loads (swap
   sawtooth; sheds every ~45min; one hard machine freeze, no oom-kill logged — hardware suspected).
   Per-container caps (docker update, live) fix isolation: one container OOMs alone (137, ~3min
   recovery) instead of box death. Budget caps for END-state (EVM ELs pinned at 97% of 2.5G once the
   guzzler trie grew → 0.16blk/s; 5G fixed it). Current: pay 10G / evm 5G (bump demo-bloat from 2.5G!)
   / cl 0.75G. After ANY EL pool wipe (restart) spammers MUST be bounced (-l re-syncs nonces) or all
   their txs queue forever nonce-gapped.
5. **Latency coupling measured**: one certificate/height couples cadence = max(T_evm,T_pay)+overhead.
   Guzzler-full EVM blocks (50-90M gas keccak+cold-SSTORE, all txs serial on ONE contract+counter — the
   hot-account worst case; executed TWICE: build+re-execute) drag the pair to 0.1-0.7 blk/s while
   payment exec stays ~5ms. Light lanes hit ~3.8-3.9 blk/s (24h soak) ≈ the 250ms target. Builder
   deadline bounds block FULLNESS, not cadence (2000→250ms changed nothing under saturated mempool).
6. **RUN7 3h averages (10M preseed, cumulative histograms)**: EVM(guzzler) root 10.79ms/exec 57.25ms
   per block (~18tx) vs PAY root 2.59ms/exec 5.20ms per block (273-4761tx) → per-tx gap 2-3 orders of
   magnitude; payment transfers ≈ 33µs/tx exec on 10M accounts; payment proposer builds full blocks
   ~40x faster (exec proxy). Zero forks/divergence across ALL runs incl. OOM kills, CL restarts,
   460-height sync catch-up. Payment state root flat 1-11ms on 10M throughout.
7. Ops runbook: CL "Manual intervention required" park after EL races → docker restart CL once ELs up;
   exit-137 pay EL → boot uncapped (60g) then clamp; /tmp wiped on reboot → trend log in home dir.
RECOMMENDED DEMO CONFIG: 10M preseed + fixed CL (509f404) + caps pay10/evm5/cl0.75 + batch_size fix
before 4-validator demos. demo-empty.sh for clean-slate demos. Exec benchmark (empty vs 10M preseed,
identical 1500tx/s transfers both lanes, 1h each): results land in /tmp/execcmp2-results.txt (2026-07-13).

## Payment-lane state commitment: MPT vs JMT vs dense vs SALT (branch `salt-payment-lane`)

Goal: find a better state commitment for the lean payment lane than reth's MPT. Four primitives
implemented behind ONE pair of reth seams, selected at runtime by `ARC_PAYMENT_ROOT`:

    overlay_root*        (vendor/reth-trie-db)  -> read-only root, pure, O(k log n)
    write_hashed_state   (vendor/reth-provider) -> the sole writer, advances the committed base

`crates/arc-payment-commitment/src/{persistent,dense,salt_commitment}.rs` + reth's own MPT.
Why those two seams: reth calls the root ~11x per block speculatively, plus newPayload validation
and witness re-execution. The root fn MUST be pure or build/validate disagree and the chain halts
at block 2 (this happened; it is what killed the first JMT attempt).

### THE HEADLINE
**No primitive dominates. Whichever one avoids touching disk wins by 1-2 orders of magnitude, and
the deciding variable is CACHE RESIDENCY -- not state size, trie depth, or arithmetic.**

  in RAM   (fleet, 5M accts, equal load)   MPT 3.23ms  vs SALT 7.76ms   -> MPT 2.4x
  past RAM (real 168GB Arc snapshot)       MPT 222.9ms vs SALT 11.0ms   -> SALT 20.2x
  ... but that MPT figure is reth's SYNCHRONOUS overlay_root. Against its tuned pipeline
  (27.8ms measured) SALT's edge is only ~2.5x. Quoting 20x compares optimised-SALT to
  unoptimised-MPT.

### PRIMITIVE COMPARISON (identical keys/changes/scale, `commit-bench --primitives`)
N=10M, 200 changed/round:
    JMT (redb)     6.40GB  median 5.448ms  p99  13.4ms  (2.5x)   survives an 8G cap
    dense (mmap)   6.00GB  median 2.725ms  p99 127.1ms  (47x)    OOM-killed at 8G
    SALT (heap)    2.77GB  median 3.195ms  p99 120.5ms  (38x)    OOM (unevictable)
Medians within ~2x; the separation is TAIL LATENCY, which bounds block cadence.
**JMT is the sleeper and the pick if shipping today**: keccak-only (no discrete-log assumption),
tightest tail, the ONLY alternative that degrades instead of dying under memory pressure (redb keeps
nodes AND values on disk). It lost the first head-to-head on its redb WRITE path -- tuning, not
structure. dense is NOT evictable despite the mmap: its values live in a heap BTreeMap.

### VERIFIED CORRECT, ON REAL HARDWARE
- 4-machine tailscale fleet, SALT payment lane: all 4 validators computed the SAME payment-lane
  stateRoot at every settled height, 0 mismatches, under load. Both lanes agree (EVM=MPT, PAY=SALT)
  under one consensus certificate. `experiments/dual-el/fleet/demo-fleet-salt.sh`
- SALT seeded from the REAL Arc snapshot: 44,429,999 accounts, 14.1GB, 444s (~0.32 KB/key).

### GOTCHAS THAT COST REAL TIME (each one invalidated a measurement)
1. **Foreign load generators.** Stale `spam-fleet.sh`/`headtohead.sh` respawn loops drove 6500 tx/s
   into one lane vs 300 into the other -> 4761-vs-12 txs/block. `pkill` does NOT stop them; the
   harnesses respawn children. Kill the PARENT. `fair-compare.sh preflight` now refuses to run if
   any spammer exists.
2. **Fixed PRNG seed** -> every run samples the SAME accounts -> run 2+ measures PAGE-CACHE HITS
   (19.8ms, ZERO disk reads) not the structure. Always vary `--seed`.
3. **Empty AccountsTrie** -> `overlay_root` rebuilds the whole trie from scratch, O(N) per block
   (815ms at 1M). Build intermediate nodes first or the MPT number is meaningless.
4. **Key generator with the index in the HIGH bytes** -> every key collides into dense's slot 0 and
   `slot_digest` hashes all N per update. Looked exactly like an O(N) bug in dense. Use uniform keys.
5. **Preseed != organic state.** Same ~5M accounts: MPT 3.23ms bulk-loaded vs 8.62ms grown by
   transactions. Preseeding bulk-loads a contiguous cache-friendly layout and FLATTERS the MPT.
6. **Preseed fights MEMLIMIT**: reth pins the whole genesis alloc in unevictable RSS
   (~250 B/account), so the preseed eats the memory cap you were trying to squeeze.
7. **`rm -rf` cannot wipe a reth datadir**: docker creates `db/` root-owned, so the wipe silently
   fails and the EL dies with "genesis hash in the storage does not match". Wipe via a root
   container, and VERIFY.
8. **MDBX aborts long-lived read transactions** (-96000). Seeding 44M accounts takes ~7min, so
   re-open a fresh tx per chunk and resume from the last key.
9. **cargo-chef cannot cook a `[patch]`-to-workspace-member tree** -- it stubs members to empty
   libs and the patched upstream crates then fail to compile. `deployments/Dockerfile.execution.salt`
   builds directly instead.
10. **`git worktree add` does not init submodules** -> hardhat genesis fails on missing OpenZeppelin.
11. **Fleet scripts had a hardcoded worktree path** (`gen-fleet.py` -> arc-node-paymentlane), so
    running from any other checkout silently produced no compose files.

### STILL OPEN (blocks production use of any alternative)
- **Contract storage is not committed** by JMT/dense/SALT -- Arc's state has 568,885,040 storage
  slots they cannot represent. Either commit storage, or prohibit contracts on the lane AND ENFORCE
  it, or the root authenticates only part of the state.
- No reorg/unwind rollback for the alternative commitments.
- `eth_getProof` still assumes the MPT (SALT has its own Witness API).
- dense is GRINDABLE: 23-bit slot + flat bucket chaining, ~2^23 hashes lands an address in a chosen
  slot, and cost is O(bucket) forever. The MPT is structurally immune (full 256-bit paths => cost
  scales with DEPTH, not occupancy).
- `salt-commitment` is default-ON across three crates because reth-trie-db is a `[patch]` crate that
  `--features` cannot target. Needs real feature plumbing before main.

Results + raw CSVs: `experiments/dual-el/fleet/results/`. Harnesses: `fair-compare.sh` (guard-railed
live comparison), `commit-bench` (offline primitives + real-snapshot modes), `fleet-stats.py`.
Snapshot lives on papaduck at `/mnt/blockchain.ssd/arc-snap/execution` (168GB, 44.4M accounts).
