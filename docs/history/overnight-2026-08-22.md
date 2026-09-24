# Overnight autonomous run — 2026-08-22 (user-directed)

## GOAL (sharpened): a fleet running BOTH lanes under constant load that sustains 2 blk/s.
## Everything below serves that. Single-box work is preparatory / fallback.

User goals: (1) N∈{1,10,50} fan-out sweep at real load; (2) cadence diagnosis;
(3) remaining I4 gates (restart legs); (4) fleet run (I5); (5) sustain the chain a few
HOURS with constant load on BOTH lanes; (6) in parallel, prune irrelevant code from
~/reth-fork. Decide autonomously; record everything; morning report.

## Phase 0 (parallel, all night): fork pruning — AGENT
Branch `lean-prune` off `lean-native-transfer`: reduce the workspace to the dependency
closure of lean-native + lean-lane-node (drop unused member crates, reth bins, book/docs
noise). Gates: full workspace check green, lean tests + lockstep gate + fuzz smoke pass,
LEAN-NATIVE.md/CLAUDE.md updated with what was removed and why. No pushes.

## Phase 1 (single box, chain already live)
- 1a corpus: spammer dump-mode fanout corpora per node partition (200 accts each):
  N=1 ~1.2M txs/node, N=10 ~400k/node, N=50 ~150k/node.
- 1b lean-feeder.py: arc_sendRawTxBatch poster (length-framed base64, 2k txs/batch),
  txpool_status-governed (pool-target), rate-capped, self-terminating.
- 1c sweep arms N=1/10/50: ~8 min feed each; measure landed tx/s, outputs/s, cadence,
  outputs/block vs the 42k budget, 4-head commitment agreement per arm.
- 1d drain arm at N=10: prefill ~100k txs/node, stop intake, measure full-block cadence
  = the chain-limited ceiling on this box.
- 1e restart legs UNDER LOAD: (i) docker restart validator1_cl; (ii) kill lean node #4
  for 60s then restart (expects decide-time peer catch-up recovery). Gates: chain never
  halts >60s, node rejoins, 4/4 commitment agreement after.

## Phase 2 (fleet, I5 + endurance)
- Teardown local demo. Ship arc images to remotes (slow, wifi — overnight OK) + lean
  binary (10MB) + fund file. Boot demo-fleet with GLOBAL CL env (works on fleet: each
  machine's lean node is local, same port ⇒ same ARC_PAYMENT_LEAN_RPC everywhere; peer
  list = all four tailscale IPs:8560 — self-including is harmless).
- Smoke: 4/4 lean lockstep, then one measured N=10 arm (fleet outputs/s number).
- ENDURANCE (rest of night, target ≥4h): constant load BOTH lanes — per-machine ws
  spammer on its local lean node (fanout=10, ~500 tx/s each ⇒ ~20k outputs/s offered,
  live-signed so it runs indefinitely) + EVM-lane transfer spam ~200 tx/s from ginny.
  Every 30 min: heads, lean commitment agreement at settled height, EVM hash agreement,
  cadence, docker mem, disk. Gates: zero divergence; any stall >5 min or node death is
  RECORDED and (once per node) auto-restarted — a second death of the same node ends
  intervention and the run continues degraded (data, not firefighting).
- papaduck note: old machine, known flaky — its failure is a recovery data point, not an
  emergency.

## Failure policy
Independent phases: a hard failure records evidence (logs to /tmp/overnight/), skips
forward. No unattended debugging of NOVEL consensus bugs — capture and continue. Chain
left running for morning inspection. Everything appended to /tmp/overnight/report.md as
it happens; notebook/CLAUDE.md updated in the morning from that.
