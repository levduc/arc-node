# Loading the payment lane correctly (the spam technique)

Measured 2026-08-09/10 on the 4-machine fleet, 100M gas, 16k prefunded accounts
(MISSION-EL.md iters 17-18). The chain was never modified — every number below is a
property of HOW load is delivered.

## The ladder (fleet, 100M, sustained)

| technique                          | landed tx/s | height | pool state |
|------------------------------------|------------:|-------:|------------|
| old defaults (RATE=400/spammer)    | 6,827       | 697 ms | fill-paced (starved) |
| RATE=1000 open loop                | 8,094       | 588 ms | fill-paced |
| RATE>=1500 open loop               | **3-4k**    | —      | **collapsed** |
| **RATE=2500 + `--pool-target 12000`** | **8,656** | 550 ms | stable: ~14k pending, ~0 queued |
| (drain test = zero-ingress bound)  | 11,699      | 407 ms | pre-filled, no ingress |

## The two failure modes (both measured, do not rediscover them)

1. **Under-offering — fill-pacing.** The builder's deadline keeps its transaction
   iterator open pulling txs AS THEY ARRIVE, so blocks read "100% full" while height
   == txs/delivery-rate. Fullness is NOT a saturation check; the drain test is.
2. **Over-offering — the pool cliff.** Offered load past what the chain consumes
   drives the pool to its cap (200k). Eviction at cap breaks per-account nonce
   continuity, the pool fills with QUEUED (non-executable) txs, pending starves,
   blocks go near-empty, and backpressure senders collapse into "txpool is full"
   retry storms at ~300 tx/s each. It does not degrade — it collapses (8.1k -> 3.2k).

`--fire-and-forget` is also dead for sustained load: optimistic nonces have no repair
path, so a handful of early rejections nonce-gap their accounts and the queue
collapses the same way (measured: 2,787 tx/s, 193-tx blocks).

## The fix: closed-loop pool governor

`spammer --pool-target N` — a background task polls the target's `txpool_status`
every 250 ms; the shared rate limiter pauses ALL senders while pending+queued > N.
The spammer then finds the chain's consumption rate automatically: RATE becomes a
ceiling that is safe to over-provision.

- Target ~2-3 full blocks' worth of transactions: `N = 3 * gas_limit / 21000`,
  floor 12,000. (At 100M gas: 4,761 txs/block -> target 12-15k.)
- Overshoot above target by up to a few thousand is normal (250 ms poll lag).

## Recommended fleet load

```bash
POOL_TARGET=12000 S=4 ACCTS=1000 RATE=2500 \
  experiments/dual-el/fleet/spam-fleet-distributed.sh start
```

`RATE` no longer needs tuning per experiment — the governor equilibrates. Scale
`POOL_TARGET` with the gas limit as above for big-block runs.

## What delivery can and cannot buy

With the governor, the pool sits at/above target (senders pausing) — delivery is no
longer the limiter. The residual gap to the drain bound (8.65k vs 11.7k at 100M) is
the cost of CONCURRENT INGRESS: RPC admission + pool signature checks + gossip riding
on the validator boxes while they run consensus. Closing that requires ingress
isolation (dedicated RPC/admission nodes), not better spammers.

## Rules that exist because breaking them produced wrong numbers

- After rebuilding the spammer, smoke-test the VERBATIM arg string the scripts use —
  a branch rebuild can silently drop flags (`--chain-id`, `--account-offset` lived
  only on payment-lane-gas for weeks; two fleet windows were voided by this).
- Between sequential load windows on one chain, verify pending AND queued are both
  drained — a poisoned queued subpool silently ruins the next window.
- Quote landed tx/s together with offered rate and mode; "delivery plateau" numbers
  are properties of the load config, not the chain.
