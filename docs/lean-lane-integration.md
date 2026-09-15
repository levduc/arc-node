# Lean payment lane — CL integration (v0.2)

A second, lean execution lane for high-volume native payments, running beside
the EVM lane under the same Malachite BFT certificate. The lane's node lives in
its own repository (`levduc/lean-lane`: node + `0x50` fan-out wire format +
load generator; links upstream reth crates, no fork). This document is what the
Arc CL needs to drive it, written for whoever integrates the series into `main`.

Everything is behind `ARC_PAYMENT_LEAN_LANE=1`. **Flag off is stock:** the wire
format of consensus values is the EVM payload's SSZ bytes exactly as today
(unit-tested), no lean code runs, no lean node is contacted.

## 1. The series

The CL delta versus `main` (`97f8da0`, v0.8.0 sync, reth v2.2.0) is two series
applied together: the four-commit `lean-lane-v0.1` base (payload plumbing,
wire framing, the shim client, the per-phase arms) with a five-commit
`lean-lane-v0.2` header-binding series layered directly on top of it. v0.2
replaces v0.1's two-lane value commitment with binding the lean block into the
EVM header instead, so this table lists the v0.2 commits — they are what
changes v0.1's behaviour, and a `main` integration applies both series
together, not v0.1 alone.

| # | commit | scope | what to look at |
|---|---|---|---|
| 1 | `eth-engine: LeanShim v0.2 calls (newBlock/getBlockBytes by commitment) and a mockable LeanBuilder` | `crates/eth-engine/src/{lean_shim,engine}.rs` | `new_block_by_commitment`/`get_block_bytes_by_commitment` (arc_newBlock/arc_getBlockBytes addressed by commitment); `LeanBuilt`, the mockable `LeanBuilder` trait; `PayloadGenerator::generate_block` gains a `prev_randao: B256` argument (upstream callers pass `B256::ZERO`, unchanged flag-off) |
| 2 | `consensus: build the lean block first and bind its commitment as prev_randao` | `crates/malachite-app/src/handlers/get_value.rs`, `payload.rs` | `generate_payload_with_retry` builds the lane block first (timestamp-locked), recomputes its commitment, passes it to reth as `prev_randao` — the EVM block hash now covers the lean block |
| 3 | `consensus: reject a proposal whose EVM header does not commit to its lean payload` | `crates/types/src/block.rs`, `payload.rs::validate_consensus_block` | `ConsensusBlock::header_lean_commitment()` (reads `prev_randao`, `None` when zero) and `lean_binding_ok()`; the binding check runs first in validation, before timestamp lockstep, parent linkage, or any shim call |
| 4 | `consensus: anchor the lean lane by the header commitment…` | `crates/malachite-app/src/handlers/decided.rs`, `state.rs`, `get_value.rs`, `received_proposal_part.rs`, `started_round.rs` | decide anchors `arc_newBlock{commitment}` read from the header, not bytes; `State::lean_undecided` and every mid-flight byte copy the CL used to carry between validate and decide are removed — the lean node holds the bytes (staged, queued, or peer-fetched) end to end |
| 5 | `consensus: vote on the EVM block hash; serve sync lean bytes by the header commitment` | `crates/types/src/block.rs`, `crates/malachite-app/src/handlers/{get_decided_values,process_synced_value}.rs`, `payload.rs`, `proposal_parts.rs`, `crates/consensus-db/**` | the v0.1 two-lane commitment scheme is fully removed — its accessor and combinator, and the `DecidedBlock` constructors that carried it, are gone; consensus votes on and certifies the plain `self_reported_block_hash()`, as upstream (the certificate's value is the EVM block hash); sync serving looks lean bytes up by `header_lean_commitment()` instead of an offset scan; `consensus-db` carries no lane-specific code (reverted to upstream shape, `lean_payload: None` at the four call sites) |

Docs (this file) and the local-testnet restart leg (`scripts/lean-testnet.sh`)
are the series' tail, verified in §1's local run below and recorded in the
worklog.

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
- **v0.2 header-binding series, same local testnet, plus a CL restart leg**:
  `up` (~26 s) to 5/5 at height 4, byte-identical; 240 s of fan-out load
  (N=50, pool-target 1,500) in which the spammer sent 143,500 txs over
  243.5 s (589 tx/s offered, pool-governed — well under the 3,000 tx/s asked
  for; a send rate, not a chain result), `restart 3` (`docker restart
  validator3_cl` only — its lean node never stopped) fired at T+96 s;
  validator3 read back within 3 heights of the tip immediately (`docker
  restart` completes in about a second locally). Two `status` samples 60 s
  apart straddling the restart: height 215→310, **86.4 heights/min
  (~1.44 blk/s)**, every block in both samples 369 txs = 100 % of the 100 M
  budget — the only window fullness was sampled in; fullness outside it was
  not sampled — `agreement: all 5 lean nodes identical` at both samples and at
  load end (height 388, then 421 once the pool drained). `docker inspect
  --format '{{.RestartCount}}'` reads 0 on all five — Docker does not count a
  manual `docker restart`, only restart-policy-triggered ones — but
  `State.StartedAt` on validator3 (02:21:54) versus the other four (02:20:05)
  confirms only it restarted; `docker logs validator3_cl | grep -c "Manual
  intervention"` = 0; no `ERROR` line on any of the five CLs mentions the lean
  lane (the few `ERROR` lines present are peer-dial noise at boot, all at
  02:20:05, before the restart). Cadence during the restart window is lower
  than the steady-state 117–119/min above — a single sample, not yet
  characterised as restart cost versus this run's own variance.

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
| `arc_newBlock{commitment}` (v0.2) | promotes a staged/queued block with that commitment, or fetches it from `--peers` by commitment first; same `VALID`/`SYNCING` result shape. This is what decide calls — the CL passes the header's commitment, never bytes |
| `arc_getHead` | `{commitment, number, timestampMs}` |
| `arc_getBlockBytes{number}` | `{blockBytes \| null}` |
| `arc_getBlockBytes{commitment}` (v0.2) | canonical, staged, or queued block with that commitment, else `null` — what sync serving calls |

Malformed or conflicting blocks are JSON-RPC errors; lag is `SYNCING`, never an
error. The node's transaction semantics are total-STF (an invalid tx inside a
block is a no-op), so a byzantine proposer can waste bytes but cannot fork or
halt the lane. The full contract, including node-side behaviour, is
`docs/integration.md` in the lean repo.

## 3. What the CL does, per phase

| phase | file | behaviour |
|---|---|---|
| boot | `node.rs` | `arc_getHead` with unbounded retry (same as the EVM engine) — a CL that boots before its lean node must not park |
| propose | `handlers/get_value.rs`, `payload.rs` | **lean first**: `arc_buildBlock(head, head.number+1, evm_ts*1000, ARC_PAYMENT_LEAN_BUDGET_GAS)`, recompute the commitment, then generate the EVM payload with that commitment as `prev_randao`; bytes attached as `lean_payload`, the header now carries the binding |
| stream / assemble | `proposal_parts.rs` | `encode_value` / `decode_value`; the Fin signature covers the encoded bytes |
| validate | `payload.rs::validate_consensus_block` | **header/lean binding check first**: `lean_binding_ok()` (header's `prev_randao` == recomputed lean commitment) — Invalid otherwise, before any shim call. Then EVM lane via the engine as today. Lean lane still **structural only** past the binding check: `timestamp_ms == evm_ts*1000`; tip linkage against our lean head (`parent == head.commitment && number == head+1`). `number <= head` = historic replay, valid iff byte-equal to our canonical block. `number > head+1` = we are behind: pull from `ARC_PAYMENT_LEAN_PEER_RPCS` first, then judge (waiting for value-sync deadlocks — a validator that cannot vote never reaches decide-time catch-up). Then `arc_stageBlock`, fire-and-forget |
| decide | `handlers/decided.rs` | anchor `arc_newBlock{commitment}` — the header's lean commitment, not bytes; the node resolves it itself (staged, queued, or peer-fetched) and holds the bytes end to end, no CL-side copy; wait-and-poll 30 s total (150 ms poll, 5 s grace for a self-healing node); returned commitment must equal the header's; historic no-op when the certificate's block is already canonical |
| sync serve | `handlers/get_decided_values.rs` | lean bytes **by header commitment**: EVM payload from reth as upstream; if its `prev_randao` is non-zero and the lane is on, fetch lean bytes from the local node by that commitment, verify the recomputed commitment matches before sending, frame with `encode_value`. No offset scan; a height the local node lacks is skipped (a peer serves it) |
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

The EVM header now binds the lane: `prev_randao` carries the lean block's
commitment (§1, propose/validate rows), so the framing below is about how the
lean bytes travel alongside that header on the wire — it no longer has to
carry a separate lane commitment into the voted value.

**prevrandao semantics.** Arc previously hard-coded `prev_randao = 0` in every
payload attribute and documented that contracts must not rely on it. With the
lane on, that field becomes the proposer-chosen lean commitment instead — a
32-byte value that changes every block and is *not* randomness, and never was;
any contract that asserted it stayed zero would break. Flag off, `prev_randao`
is still `B256::ZERO`, unchanged from today.

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
git clone git@github.com:levduc/lean-lane.git ../lean-lane   # next to this checkout, or set LEAN_LANE_DIR
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

`./scripts/lean-testnet.sh restart <n>` restarts one validator's CL container
only (`docker restart validator<n>_cl`) — the lean node underneath stays up —
then polls `arc_getHead` on every lean node until validator `n`'s lean height
is within 3 of the tip. This is the case v0.1's CL-side byte copy used to make
fragile: with the header binding, the CL never holds lean bytes across a
restart, so re-validating store-loaded rows on reboot needs nothing from the
lean node beyond its own head.

## 5. Measured (development branch, 4 heterogeneous validators over tailscale, 500 ms pacer, blocks 100 % of budget)

| config | blk/s | tx/s | payments/s |
|---|---|---|---|
| EVM lane, 75 M | 1.69 | 5,972 | 5,972 |
| lean, N=1, 100 M | 1.96 | 6,401 | 6,401 (85 % full) |
| lean, N=100, 150 M | 1.93 | 555 | 55,534 |
| lean, N=100, 225 M | 1.93 | 833 | 83,286 |
| lean, mixed N (avg 3.8), 225 M, parallel recovery | 1.86 | 10,504 | 39,484 |

### fleet, v0.2 on main 97f8da0

First fleet run of the header-binding series (2026-09-14, run `v02-0914-1945`,
`scripts/fleet-lean.sh`; 4 machines, lean budget 150 M, N=50, so a **100 %-full block
is 553 txs**: `21000 + 5000 × 50 = 271,000` gas each).

| config | blk/s | tx/s | payments/s | fullness |
|---|---|---|---|---|
| v0.2, N=50, 150 M, one spammer per machine | 0.73 | 404 | **20,227** | 553/553 = **100 %** on every sample |
| v0.2, N=50, 150 M, one spammer here driving all four | 1.54 | 491 | 24,539 | 202–474 of 553 = 38 % → 81 %, **delivery-bound** |

The second row is not a chain result: a single sender in backpressure mode offers about
`generators / RTT`, and it fed four pools that the lane never cross-propagates, so each
proposer saw a quarter of it (472 tx/s offered against a chain eating 460–590). It is kept
only to show what the cadence does when blocks are not full. The first row is the measured
point: every sampled head block exactly 553 txs for ten minutes.

Both arms: **0 CL restarts**, **0 `Manual intervention`** on all four CLs, **100 % of rounds
decided at round 0**, lean blocks byte-identical at every `status`. Nothing was saturated at
the full-block cadence — CLs 1–3 % CPU, ELs 4–17 %, lean nodes 60–100 % of one core — so the
0.73 blk/s is transport and shim round trips, not execution and not a failing round.

Track A on the fleet, both passing:

- `kill-lean 3 60` — validator3's lean node killed at lean height 1031; the other three
  advanced to 1062 (+31) during the outage; validator3's CL neither restarted
  (`RestartCount` 0 → 0) nor parked; on relaunch the lean node reached the tip in **2 s**
  and its EL followed, agreement byte-identical at 1065.
- `restart-cl 3` — `docker restart validator3_cl`; `State.StartedAt` moved (02:44:43 →
  02:57:27) while `RestartCount` stayed 0, rejoin within 3 of the tip in **2 s**, 0 parks,
  all four equal at 1085.

Caveats: n=1, one ten-minute window per arm, four heterogeneous machines (one on wifi,
one with 15 GB RAM also running 22 unrelated containers). The 0.73 blk/s at N=50 sits well
under the campaign's 1.93 blk/s at N=100/150 M for a block of almost the same wire size
(814 KB vs 824 KB) but twice the signatures (553 vs 287) — a cross-run comparison on a
different base, not a controlled experiment, and the cause is open.

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
