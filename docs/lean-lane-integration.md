# Lean payment lane — consensus-layer integration

A second, lean execution lane for high-volume native payments, run beside the
EVM lane under the same Malachite certificate. The lane's state and
transactions live in a separate node, the **lean node** (the `lean-lane`
repository: node, `0x50` fan-out transaction format, load generator; it links
upstream reth crates, no fork). This document covers what the Arc consensus
layer (CL) does to drive it, where that code lives, and how to run it.

Everything is behind `ARC_PAYMENT_LEAN_LANE=1`. **With the flag off the node is
upstream Arc**: consensus values are the EVM payload's SSZ bytes, headers carry
a zero `prev_randao`, no lean code runs, no lean node is contacted, and logs,
metrics and errors are unchanged (§5 lists the tests that pin this).

## 1. Design

- **One certificate, one voted value.** Consensus votes on and certifies the
  plain EVM block hash, as upstream. The lean block is bound into that block
  by the EVM header: `prev_randao` carries the lean block's commitment. A
  certificate therefore commits to exactly one lean block.
- **Lean block first.** The proposer builds the lean block on its lean node's
  head, timestamp-locked to the EVM payload (`timestamp_ms = evm_timestamp *
  1000`), recomputes its commitment and asks the EVM engine for a payload
  with that `prev_randao`. The lean bytes travel beside the EVM payload in
  the proposal and in value-sync.
- **Structural voting.** Validators do not execute the lean block to vote.
  They check the binding, the timestamp lockstep and the parent linkage
  against their lean head, and stage the block on their lean node.
- **Execution at decide.** Decide appends the lean block by commitment; the
  lean node already holds it (staged, queued or fetched from a peer), so the
  CL carries no bytes to decide.
- **Total state transition.** An invalid lean transaction is a no-op, never a
  block rejection. A byzantine proposer can waste block space but cannot
  halt or fork the lane.
- **Robust node, thin CL.** Recovery (gossip, backfill, snapshots, the
  `SYNCING` queue) lives in the lean node. The CL is a client of five
  idempotent JSON-RPC verbs and needs no forkchoice, payload ids or JWT.

`prev_randao` is not randomness under the lane: it is a commitment the
proposer chooses. Arc already documents that contracts must not rely on it
(the note in `Engine::generate_block`); with the lane off it stays zero.

## 2. The lean node contract

Block bytes, and the commitment every party recomputes from them:

```
[parent 32][number u64 LE][timestamp_ms u64 LE][n_txs u32 LE] ([len u32 LE][tx bytes])*
commitment = keccak(parent ‖ number LE ‖ timestamp_ms LE ‖ keccak(bytes[48..]))
```

The tx hash covers the whole framed section, so two framings of the same
bytes never share a commitment.

| verb | result |
|---|---|
| `arc_buildBlock{parentCommitment, number, timestampMs, budgetGas}` | `{commitment, blockBytes}`, built from the pool, not appended |
| `arc_stageBlock{blockBytes}` | `STAGED` or `SYNCING`: pre-executed, not appended |
| `arc_newBlock{blockBytes}` or `{commitment}` | `{status: "VALID", commitment}` (appended or already known) or `{status: "SYNCING"}` |
| `arc_getHead{}` | `{commitment, number, timestampMs}` |
| `arc_getBlockBytes{number}` or `{commitment}` | `{blockBytes}` or `{blockBytes: null}` |

Params are by name, block bytes are standard padded base64. Malformed or
conflicting blocks are JSON-RPC errors; lag is `SYNCING`, never an error. The
full contract, node-side behaviour included, is `docs/integration.md` in the
`lean-lane` repository; how to run a node, with every flag, is the "Run a
node" section of its README.

## 3. What the CL does, per phase

| phase | code | behaviour |
|---|---|---|
| boot | `node.rs` | build the client from the env config, `arc_getHead` retried without bound (a CL that boots before its lean node waits), register `lean_no_verdict` |
| propose | `payload.rs::generate_payload_with_retry`, `handlers/get_value.rs` | `arc_getHead`, then per build attempt `arc_buildBlock(head, head+1, ts*1000, budget)`, strict decode, recomputed commitment must equal the node's claim, commitment passed as `prev_randao`; the proposer stages its own block. A failed build skips the round instead of stopping the node (chain anomalies still stop it) |
| re-propose | `get_value.rs::decide_reuse` | a stored block has lost its lean bytes; it is re-proposed only once they are fetched back by the header commitment. Otherwise a **signed** stored block declines the round (a different block for the same round would be equivocation) and an **unsigned** one is rebuilt |
| stream / assemble | `proposal_parts.rs` | values framed with `encode_value` / `decode_value` by the node's flag (§6); the Fin signature covers the framed bytes |
| vote | `lean_lane/binding.rs` via `payload.rs::validate_consensus_block` | after the engine accepts the payload, see §4 |
| decide | `handlers/decided.rs`, `lean_lane/anchor.rs` | `arc_newBlock{commitment}` with the header's commitment, polling through `SYNCING` and an unreachable node (150 ms) up to a 30 s deadline that bounds every call; the node must answer that exact commitment; a zero header commitment is an error |
| restream | `handlers/restream_proposal.rs` | rehydrate the stored block from the lean node first, or decline the restream: re-framed without its lean bytes it would not verify against the stored signature |
| sync serve | `handlers/get_decided_values.rs` | lean bytes fetched **by the header commitment**, recomputed and compared before framing; a height the local node lacks is skipped and another peer serves it |
| sync receive | `handlers/process_synced_value.rs` | decode by flag, validate as a network block (§4); staging happens there, the append at decide |

## 4. The lean verdict

`validate_lean_section(block, lean)` runs once the engine accepted the
payload. `lean` is the local lean node for a block that came from the network
(live parts, pending parts, value-sync) and `None` for blocks this node built
or re-validates from its own store, which keep the structural checks only.

1. **Resolve.** A network block whose header commits to a lean block but
   ships no lean bytes is valid only if the local node has that block
   (`arc_getBlockBytes{commitment}`, recomputed and compared). Voting for it
   on the EVM lane alone would certify a block whose anchor can never
   complete.
2. **Binding.** `prev_randao` must equal the recomputed lean commitment.
3. **Lockstep.** `timestamp_ms == evm_timestamp * 1000`.
4. **Linkage** against the local lean head: a block at or below the head must
   be byte-equal to ours (a replay of our past); the block right above the
   head must name it as parent; a block further ahead first triggers a
   catch-up from the peer lean nodes (`ARC_PAYMENT_LEAN_PEER_RPCS`), because
   waiting for value-sync deadlocks a validator that cannot vote. The
   catch-up has a 5 s budget, sliced evenly across the peers still alive
   (floor 250 ms), and never asks a peer twice after it failed. A lag over
   1024 blocks abstains without spending the budget.
5. **Stage.** A linked block is staged on the lean node, fire-and-forget.

The result is a `LeanVerdict`:

| verdict | when | effect |
|---|---|---|
| `Valid` | nothing observably wrong | vote |
| `Invalid` | an observed violation: binding, lockstep, wrong parent one below the proposal, conflicting historic block, a header naming a block the node cannot produce | forensic record, `invalid_payloads_count{source="lean_reject"}`, vote nil |
| `Abstain(reason)` | this node's own state: lean node away or behind | `lean_no_verdict{reason}`, one warning per height, `Err` returned: no vote, nothing recorded against the block |

Reasons: `gap_too_large`, `budget_exhausted`, `peers_timed_out`, `no_peers`,
`local_unreachable`, `head_ran_past`. Being behind must never produce
`Invalid`: an `Invalid` recorded against a value that later gets certified
sticks, because the valid-round rule re-proposes certified values without
re-validating them.

A lean node that does not answer after the client's transport retries (about
15 s, which covers a node restart) fails with `LeanNodeUnreachable`. That is
an abstain at vote time, a skipped height in sync serving, a skipped round at
propose, and a wait at decide. It is never process-fatal.

## 5. CL delta and porting order

The series against upstream `main`, one commit per step; each compiles and
passes its tests on its own.

| # | commit | files |
|---|---|---|
| 1 | `prev_randao` through `generate_block` | `eth-engine/src/engine.rs`, `payload.rs` (`PayloadGenerator`); every caller passes `B256::ZERO` |
| 2 | types | `types/src/lean.rs` (new: `LeanLanePayload`, `decode_lean_block`, `encode_value`/`decode_value`), `types/src/block.rs` (`lean_payload` field, `header_lean_commitment`); `lean_payload: None` in every `ConsensusBlock` literal, `consensus-db/src/decoder.rs` included |
| 3 | lean node client | `eth-engine/src/lean_shim.rs` (new: `LeanShim`, the `LeanNode` trait, `LeanNodeUnreachable`) |
| 4 | validation | `malachite-app/src/lean_lane/{mod,binding,catchup,test_lane}.rs` (new), `payload.rs` (`validate_consensus_block` and `establish_block_validity` take `Option<&dyn LeanNode>`, all callers `None`), `metrics/app.rs` (`lean_no_verdict`, `LeanReject`) |
| 5 | handler arms | `lean_lane/anchor.rs` (new), `env_config.rs`, `state.rs`, `node.rs`, `proposal_parts.rs`, `payload.rs`, `handlers/{get_value,received_proposal_part,started_round,decided,restream_proposal,process_synced_value,get_decided_values}.rs` |
| 6 | tests | flag-off pins and the decision tables |
| 7 | local testnet and this guide | `scripts/lean-testnet.sh`, `crates/quake/scenarios/localdev-lean.toml`, the compose `extra_hosts` entry, `Makefile` |

To port onto another fork of Arc, apply in that order. Step 2 breaks every
`ConsensusBlock` literal; run `lean_commitment_cross_pins_against_lean_node`
against the lean node's test vectors before going further. Steps 4 and 5 are
the only ones that touch handler logic, and every arm is a branch on
`state.lean_node()`.

Flag-off pins:

| pin | test |
|---|---|
| value encoding is the stock SSZ bytes | `types::lean::tests::encode_value_flag_off_is_stock_ssz`, `proposal_parts::tests::flag_off_proposal_data_is_the_stock_ssz_payload` |
| mixed flags fail closed | `types::lean::tests::decode_value_fails_closed` |
| `prev_randao` stays zero | `payload::tests::the_lean_commitment_becomes_prev_randao_and_is_zero_without_the_lane` |
| an EVM-only block is valid only with the lane off; lane-on it is Invalid without contacting the node | `lean_lane::binding::tests::an_evm_only_block_is_valid_only_with_the_lane_off` (`None`, and a double that panics on any call) |
| stored blocks re-proposed as before | `handlers::get_value::tests::a_stored_block_is_reused_rebuilt_or_declined` |
| lane off unless `1`/`true` | `env_config::tests::lean_lane_is_off_unless_set_to_1_or_true` |

## 6. Configuration and wire format

Read once at startup (`env_config.rs`):

| variable | default | |
|---|---|---|
| `ARC_PAYMENT_LEAN_LANE` | off | `1` or `true` enables the lane |
| `ARC_PAYMENT_LEAN_RPC` | `http://127.0.0.1:8560` | this validator's lean node |
| `ARC_PAYMENT_LEAN_BUDGET_GAS` | `300000000` | per-block budget for `arc_buildBlock` (gas = 21000 + 5000·N per fan-out tx of N outputs) |
| `ARC_PAYMENT_LEAN_PEER_RPCS` | empty | other validators' lean nodes, comma-separated: the catch-up source |

The upstream `ARC_SYNC_REQUEST_TIMEOUT` and `ARC_SYNC_BATCH_SIZE` are worth
raising for megabyte-sized values on slow links.

`encode_value(evm, lean, lean_lane)`:

- flag off: `evm.as_ssz_bytes()`, identical to upstream;
- flag on: `[u64 LE: len(evm) | LEAN_LANE_BIT (1<<62) if a lean block follows][evm SSZ][lean bytes]`
  on every value, EVM-only or not. Unknown flag bits, bad lengths and
  trailing bytes fail closed.

The format follows the node's own flag, never the bytes, so:

- enable the lane on **all validators together, from height 1 of a fresh
  chain**. A mixed-flag network fails closed with decode errors (proposals
  dropped, rounds time out) rather than diverging;
- there is no mid-chain activation: with the lane on, every decided header
  must name a lean block, and decide fails on a zero commitment.

## 7. Local testnet

Needs Docker, Foundry's `cast` and a checkout of `lean-lane` next to this
repository (or `LEAN_LANE_DIR` pointing at one).

```bash
make testnet-lean                       # images, 5 host lean nodes, quake localdev-lean.toml
make testnet-lean-load LOAD_SECS=180    # fan-out load (LOAD_RATE, FANOUT, POOL_TARGET)
make testnet-lean-status                # EL and lean heights, head fullness, lean agreement
scripts/lean-testnet.sh restart 3       # restart one CL; its lean node stays up
make testnet-lean-down
```

`scripts/lean-testnet.sh up` builds the lean node and the load generator if
needed, generates a fund file, starts one lean node per validator on ports
8561..8565 (peers: the other four), wipes and starts the quake scenario, and
waits for the lean chain to move. The CL containers reach the host lean nodes
as `host.docker.internal` (the `extra_hosts` entry in the local compose
template). `status` reports the head block's transaction count next to the
heights, since a rate without fullness measures delivery, not the chain.

## 8. Operating rules

- Start the lean node before the CL when possible. The CL waits for it, but
  a validator whose lean node is missing abstains every round while its
  containers look healthy; watch `lean_no_verdict` and attribute failed
  rounds by proposer.
- Enable the lane on every validator at once, from height 1 (§6).
- Never wipe a lean chain under a live consensus chain: certificates bind the
  destroyed blocks, and a lagging node can never sync that span.
- All lean nodes of a network need the same chain id and fund file; genesis
  is a function of both.
- Fan-out transfers drain sender balances; refund by regenerating genesis.
- Consensus parameters (pacer, timeouts) still change live through
  governance; the lane flag does not.

## 9. Measured

Four heterogeneous validators on a LAN, 500 ms pacer, fan-out N=100, 400 M
lean gas per block. Measured on the implementation this series was cleaned up
from (same behaviour, including the rules above), not yet re-measured on this
series.

| run | blocks/s | fullness | payments/s |
|---|---|---|---|
| 10 min at 400 M | 1.86–1.97 | 100 % | 142,443–150,965 |
| 30 min soak at 400 M | — | 100 % | 120,529–152,183 per minute; 28 of 28 minutes above 120,000 |

Execution is not the bottleneck: the lean node imports far faster than
consensus delivers blocks. The cost per height is dominated by streaming the
proposal, so the remaining structural lever is compact proposals (transaction
hashes against pre-distributed bodies), which this series does not attempt.
