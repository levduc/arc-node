# Lean payment lane — CL integration (v0.1)

A second, lean execution lane for high-volume native payments, running beside
the EVM lane under the same Malachite BFT certificate. The lane's node lives in
its own repository (`lean-lane`: node + `0x50` fan-out wire format + load
generator; builds against upstream reth, no fork). This document is what the
Arc CL needs to drive it, written for whoever integrates the series into `main`.

Everything is behind `ARC_PAYMENT_LEAN_LANE=1`. **Flag off is stock:** the wire
format of consensus values is the EVM payload's SSZ bytes exactly as today
(unit-tested), no lean code runs, no lean node is contacted.

## 1. The series

Six commits on `lean-lane-v0.1`, based on `main` at `97f8da0` (v0.8.0 sync,
reth v2.2.0). Commits 1–4 are one unit (the `ConsensusBlock` struct change
ripples; they compile together, not individually). 5 and 6 are docs and the
local testnet.

| # | commit | scope | what to look at |
|---|---|---|---|
| 1 | `types: dual-lane consensus block…` | `crates/types/src/block.rs` | `lean_payload: Option<LeanLanePayload>`; `value_id = commit_lanes(evm_hash, lean_commitment)`; `decode_lean_block` (strict header/tx-boundary decoder, commitment recomputed); `encode_value`/`decode_value` (wire mode by flag) over `frame_lanes`/`unframe_lanes`; `DecidedBlock` carries the lane commitment. Tests: round-trips, fail-closed decodes, cross-pin vector, `encode_value_flag_off_is_stock_ssz` |
| 2 | `consensus-db: persist decided blocks keyed by value_id…` | `crates/consensus-db/src/{decoder,encoder,store}.rs` | lookups by `certificate.value_id` instead of EVM block hash; EVM-only rows decode unchanged (no migration) |
| 3 | `eth-engine: LeanShim JSON-RPC client + TransientDependencyError marker` | `crates/eth-engine/src/{lean_shim,transient}.rs`, `lib.rs`, `Cargo.toml` (+base64) | five verbs, ~15 s transport retry, then `TransientDependencyError`; `is_transient()` walks the eyre chain |
| 4 | `consensus: lean-lane arms, gated by ARC_PAYMENT_LEAN_LANE` | `crates/malachite-app/src/{env_config,node,app,state,payload,proposal_parts}.rs`, `handlers/{get_value,started_round,received_proposal_part,restream_proposal,process_synced_value,decided,get_decided_values}.rs`, `metrics/app.rs` | one arm per phase, each described in the commit message; every arm is inside `if let Some(shim) = lean_shim` or takes `lean_lane = lean_shim.is_some()` |
| 5 | `docs: lean-lane integration guide…` | this file | |
| 6 | `local lean testnet: make testnet-lean…` | `scripts/lean-testnet.sh`, `Makefile`, `crates/quake/scenarios/localdev-lean.toml`, `crates/quake/templates/local/compose.yaml.hbs` (+2 lines: CL gets `host.docker.internal`) | §4b |

How the series sits on upstream's own recent changes: `ConsensusBlock` votes
through `to_proposed_value_with_validity` / `self_reported_block_hash`, so the
lane only redirects the value id (`value_id()` = self-reported hash, or the
two-lane commitment); `establish_block_validity` and `validate_consensus_block`
take an extra `lean_shim` argument and the lean structural check runs after the
EVM engine verdict; a transient dependency error surfaces to sync as upstream's
`SyncedValueOutcome::LocalTransientError` (plus the
`transient_dependency_skips` counter); sync serving keeps upstream's
`ExtendedCommitCertificate` form with the value bytes re-encoded per lane mode.
Upstream's `ARC_SYNC_REQUEST_TIMEOUT` / `ARC_SYNC_BATCH_SIZE` cover the
value-sync tunables the lane used to add. The arc spammer's fan-out support was
dropped from the series; the lean repo's spammer drives the lane.

Apply with `git cherry-pick`/`format-patch`; each message carries the design
reasoning so the code comments do not have to.

Verification done on this base (`97f8da0`):

- `cargo test -p arc-node-consensus -p arc-eth-engine -p arc-consensus-types
  -p arc-consensus-db` — all suites green (403/99/194/106/31/20/1) except
  `cli_db_migrate::test_migrate_command_without_home_flag`, which fails on any
  machine with an existing `~/.arc/consensus/store.db` and passes with an
  isolated `HOME` (pre-existing, unrelated).
- Local 5-validator testnet (§4b, images built from this series): up in ~20 s;
  4 min of fan-out load at 3,000 tx/s offered, N=50, pool-target 1,500, 800
  accounts — 117–119 heights/min throughout (~1.95 blk/s), every block during
  the load 369 txs = 100 % of the 100 M budget, 181,424 of 182,900 sent txs
  included (9.07 M payments), 5/5 lean nodes byte-identical, 0 CL restarts /
  parks / transient errors.

**What was measured vs. what was ported.** All fleet numbers (§5) and the
outage live-tests were produced on the development branch (reth v2.3.0 port
plus several abandoned experiments). This series is the lane delta re-applied
on `main` with those removed: unit-tested and run on a single-machine
5-validator testnet on `main`, **not yet run on the 4-machine fleet from
`main`**. The lane-enabled code paths are what ran on the fleet, adapted to
upstream's validation refactor as described above; the flag-off wire path is
`main`'s own code.

## 2. Contract with the lean node

Block bytes (opaque to consensus except the header):

```
[parent 32B][number u64 LE][timestamp_ms u64 LE][n_txs u32 LE] ( [len u32 LE][tx bytes] )*
commitment = keccak( parent ‖ number LE ‖ timestamp_ms LE ‖ keccak(bytes[48..]) )
```

Shim (JSON-RPC over HTTP, by-name params, base64 `blockBytes`):

| verb | result |
|---|---|
| `arc_buildBlock{parentCommitment,number,timestampMs,budgetGas}` | `{commitment, blockBytes}` — builds, does not append |
| `arc_stageBlock{blockBytes}` | `{"status":"STAGED",commitment}` \| `{"status":"SYNCING",number}` — vote-gap pre-execution, fire-and-forget |
| `arc_newBlock{blockBytes}` | `{"status":"VALID",commitment}` (appended or already known — idempotent) \| `{"status":"SYNCING",number}` (queued; node backfills itself from its peers) |
| `arc_getHead` | `{commitment, number, timestampMs}` |
| `arc_getBlockBytes{number}` | `{blockBytes \| null}` |

Malformed or conflicting blocks are JSON-RPC errors; lag is `SYNCING`, never an
error. The node's transaction semantics are total-STF (an invalid tx inside a
block is a no-op), so a byzantine proposer can waste bytes but cannot fork or
halt the lane. The full contract, including node-side behaviour, is
`docs/integration.md` in the lean repo.

## 3. What the CL does, per phase

| phase | file | behaviour |
|---|---|---|
| boot | `node.rs` | `arc_getHead` with unbounded retry (same as the EVM engine) — a CL that boots before its lean node must not park |
| propose | `handlers/get_value.rs` | after the EVM payload: `arc_buildBlock(head, head.number+1, evm_ts*1000, ARC_PAYMENT_LEAN_BUDGET_GAS)`; bytes attached as `lean_payload` |
| stream / assemble | `proposal_parts.rs` | `encode_value` / `decode_value`; the Fin signature covers the encoded bytes |
| validate | `payload.rs::validate_consensus_block` | EVM lane via the engine as today. Lean lane **structural only**: recompute commitment; `timestamp_ms == evm_ts*1000`; tip linkage against our lean head (`parent == head.commitment && number == head+1`). `number <= head` = historic replay, valid iff byte-equal to our canonical block. `number > head+1` = we are behind: pull from `ARC_PAYMENT_LEAN_PEER_RPCS` first, then judge (waiting for value-sync deadlocks — a validator that cannot vote never reaches decide-time catch-up). Then `arc_stageBlock`, fire-and-forget |
| decide | `handlers/decided.rs` | anchor `arc_newBlock(bytes)`; wait-and-poll 30 s total (150 ms poll, 5 s grace for a self-healing node); returned commitment must equal the certificate's; historic no-op when the certificate binds an already-canonical block |
| sync serve | `handlers/get_decided_values.rs` | lean bytes by number from the local node, re-encoded with the EVM payload, verified against the height's certificate before sending; EVM-only heights detected via `commit_lanes(evm, None) == value_id` |
| sync receive | `handlers/process_synced_value.rs` | decode, validate as above, execute both lanes (`arc_newBlock`) |

**Transient dependency handling (Track A).** Past the shim's ~15 s retry the
error is a `TransientDependencyError`: validation returns *no verdict* for the
round (never `Invalid` — an Invalid recorded against a certified value sticks
forever under the valid-round rule); the anchor loop keeps polling to its
deadline; value-sync replies `None` so the peer re-requests
(`transient_dependency_skips{source="sync"}` counter). Never process-fatal.
Live-tested on the 4-validator fleet: lean node killed for 60 s under load (CL
restarts 0, chain advanced 278→596, 3 transient warnings); CL restarted while
its lean node was down (rejoined, re-signed within 90 s).

Configuration (all read once at boot, `env_config.rs`):

| variable | default | |
|---|---|---|
| `ARC_PAYMENT_LEAN_LANE` | unset (off) | `1`/`true` enables the lane |
| `ARC_PAYMENT_LEAN_RPC` | `http://127.0.0.1:8560` | this validator's lean node |
| `ARC_PAYMENT_LEAN_BUDGET_GAS` | `300000000` | per-block budget passed to `arc_buildBlock` (gas = 21000 + 5000·N per tx) |
| `ARC_PAYMENT_LEAN_PEER_RPCS` | empty | other validators' lean nodes — catch-up source |
| `ARC_SYNC_REQUEST_TIMEOUT`, `ARC_SYNC_BATCH_SIZE` (upstream) | unchanged | sync tunables worth raising for ~1 MB lane payloads on slow links |

## 4. Wire format and rollout

`encode_value(evm, lean, lean_lane)`:

- flag off → `evm.as_ssz_bytes()` — identical to `main`;
- flag on → `[u64 LE len(evm) | LEAN_LANE_BIT(1<<62)?][evm SSZ][lean bytes?]` on
  **every** value, EVM-only heights included, so a lane-enabled fleet has one
  format across activation. `unframe_lanes` fails closed on unknown flag bits,
  oversized lengths, trailing bytes.

The format is chosen by the node's own flag, never sniffed from the bytes.
Consequences: turn the flag on for **all validators together, from height 1 of
a fresh chain**; a mixed-flag fleet fails closed with decode errors (proposals
dropped, rounds time out) rather than diverging. Changing consensus parameters
(pacer, timeouts) still works live via governance; changing the lane flag does
not.

## 4b. Local testnet (quake)

```bash
git clone <lean-lane repo> ../lean-lane      # next to this checkout, or set LEAN_LANE_DIR
make testnet-lean                            # docker images + 5 host lean nodes + quake localdev-lean.toml
make testnet-lean-load LOAD_SECS=180         # fan-out load through the lean nodes
make testnet-lean-status                     # EL/lean heights + byte-level lean agreement
make testnet-lean-down
```

`scripts/lean-testnet.sh` starts one lean node per validator on the host
(`8561..8565`, `--bind 0.0.0.0`, peered with each other, one shared fund
file), then runs quake with `crates/quake/scenarios/localdev-lean.toml`. That
scenario uses quake's `cl.env` tables to set
`ARC_PAYMENT_LEAN_LANE=1`, the budget, the peer list and each validator's own
`ARC_PAYMENT_LEAN_RPC=http://host.docker.internal:856N` (the series adds the
`extra_hosts` entry to CL containers in the local compose template). There is
no full node in the scenario: a lane-disabled CL cannot decode lane-framed
values (§4). `up` is always fresh on both chains and regenerates the whole
testnet directory, on purpose.

## 5. Measured (development branch, 4 heterogeneous validators over tailscale, 500 ms pacer, blocks 100 % of budget)

| config | blk/s | tx/s | payments/s |
|---|---|---|---|
| EVM lane, 75 M | 1.69 | 5,972 | 5,972 |
| lean, N=1, 100 M | 1.96 | 6,401 | 6,401 (85 % full) |
| lean, N=100, 150 M | 1.93 | 555 | 55,534 |
| lean, N=100, 225 M | 1.93 | 833 | 83,286 |
| lean, mixed N (avg 3.8), 225 M, parallel recovery | 1.86 | 10,504 | 39,484 |

Execution is 7–15 % of a height on both lanes; consensus transport (proposal
stream 57 %, votes 13 %, anchor 10 %) is the bound. ±15 % run-to-run, n=1 for
most points, longest soak 6 h.

## 6. Decisions deliberately left open

These are not in v0.1; each is a small, separable change once decided:

- **State/checkpoint root exposure.** The lane's state is a flat map committed
  only through the commitment chain; nothing in the certificate names a state
  root. Options: a lagged checkpoint root in the lean header, or none (replay is
  the proof). Affects `decode_lean_block` and the node's header only.
- **Header version + proposer field** in the lean block, and **charging the
  proposer** for invalid transactions (today they are free no-ops).
- **Beneficiary from the block** (fees currently accrue per node config).
- **Transaction propagation** between lean nodes (today ingress is per node;
  the proposer packs what it has).
- **Candidate removals** once the node's self-healing has soaked longer: the
  validation-time peer catch-up loop and the anchor's peer-fetch fallback both
  exist only to compensate for a node that could not yet heal itself.
- **Compact proposals** (hashes against pre-distributed bodies) are the one
  remaining structural lever on the 57 % proposal stream; parked, not started.

## 7. Left out of v0.1 on purpose

Present on the development branch, not in this series: the reth v2.3.0 port;
a second reth EL as payment lane; builder pre-build; compact proposals;
deferred EVM execution; the 8 s→120 s engine timeouts; historical fleet
launchers; the value-sync env tunables (upstream has `ARC_SYNC_*`); the arc
spammer's fan-out mode (the lean repo's spammer has it). The lean node itself
moved to its own repo.
