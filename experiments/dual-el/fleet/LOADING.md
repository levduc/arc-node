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

## Reproducing the headline numbers (4-machine fleet)

Prereqs: tailscale authed to all remotes (`tailscale status`), images shipped
(`fleet/ship-images.sh`), `cargo build --release -p spammer` — then smoke the VERBATIM
spam arg string once (see rules above).

```bash
cd ~/arc-node-paymentlane

# 1. Boot the fleet at an operating point (example: 1s heartbeat, 200M payment blocks)
PAY_GAS=200000000 EXTRA_ACCOUNTS=16000 BLOCK_TIME_MS=1000 \
  experiments/dual-el/fleet/demo-fleet-metamask.sh start
# verify PRODUCING via RPC heads on all 4 (never by grepping the start log):
#   ports http 19545/19645/19745/19845 on val1..4's tailscale IPs

# 2. Governed load on all 4 machines (disjoint account ranges, pool-depth closed loop)
#    POOL_TARGET = 3 * gas / 21000  (floor 12000); RATE is safe to over-provision
POOL_TARGET=28569 S=4 ACCTS=1000 RATE=3500 \
  experiments/dual-el/fleet/spam-fleet-distributed.sh start

# 3. Measure a window (landed tps from block contents, fullness, height)
WINDOW=240 experiments/dual-el/pay-throughput-bench.sh measure

# 4. Re-configure AT RUNTIME (no restart) — STOP LOAD FIRST, verify the flip took:
experiments/dual-el/fleet/spam-fleet-distributed.sh stop
experiments/dual-el/set-block-time.sh 500                # pacer (0 = unpaced)
PAY_GAS=100000000 EVM_GAS=30000000 \
  experiments/dual-el/fleet/set-lane-economics.sh apply   # gas limit (verify header!)

# 5. Capacity bound (drain test): blast the pool full, stop ALL spam, measure the drain
python3 experiments/dual-el/fleet/drain-test.py "my-label" 100000000

# 6. Agreement + teardown (verify 0 containers / 0 spammers on all 4 after)
experiments/dual-el/fleet/clean-fleet.sh
```

What you should see (frozen, reproduced n=2 within 1%, chain age ~3-6k blocks):

| config | height | sustained tps |
|--------|-------:|--------------:|
| 50M @ BLOCK_TIME_MS=500  | ~518 ms (HELD) | ~4,600 |
| 200M @ BLOCK_TIME_MS=1000 | ~1,035 ms (HELD) | ~9,200 |
| 100M unpaced | ~540-550 ms | ~8,650-8,850 |
| 100M drain (capacity bound) | ~407 ms | ~11,700 |

300M @ 1s is age/health-sensitive: 8.2-10.6k @ 1.35-1.74s — quote the range.
Numbers assume healthy validators; run fleet/mem-soak.sh alongside anything >30 min.
ENDURANCE (updated 2026-08-10): the historical ~0.4-0.6 GiB/min "unbounded" growth was reth's
RPC eth cache (ENTRY-bounded LRU, 5000-block default = GBs at payment block sizes). With
`--rpc-cache.max-blocks 200 --rpc-cache.max-receipts 200` (now the launch-payment-els.sh
default, env PAY_RPC_CACHE_BLOCKS/RECEIPTS) memory PLATEAUS ~1.7 GiB under 5k tps — the old
27-min OOM bound on 11 GiB validators is gone. If a validator DOES die, the chain continues
3-of-4 at ~1.85 s / ~5.1k tps (proposer slots burn round timeouts: a dead validator costs
~45%, not 25%).

## The per-sender slot cap gates prefill depth (2026-08-11)

Building a drain-test backlog on a LIVE chain has two gates, discovered wiring the dashboard's
one-click benchmark (run-bench.sh):

1. **Consumption outruns delivery.** At 200M the chain consumes ~12-14k tps when fed but spam
   delivery tops at ~8k, so the pool can never build at normal cadence — the chain fill-paces
   every block. The pacer can't throttle it either: `targetBlockTimeMs` is CL-bounded to
   **[0, 1s]** (crates/types/src/consensus_params.rs — out-of-range values silently reset to
   the 500ms default). The working throttle is the GAS LIMIT: shrink the payment lane to 25M
   at runtime (ProtocolConfig governance, quiet pool), fill, then flip to the drain size with
   an aggressive priority fee (the governance tx must outbid the backlog) and verify on a live
   header before stopping intake.
2a. **reth's per-sub-pool BYTE caps.** Even with counts at 200k and slots at 256, each
   sub-pool has a ~20MB size cap (heap accounting ≈ 40-70k transfers): the fill raced to 70k
   in 12s then eviction nonce-gapped senders and the pool collapsed to 5k. Launchers now pass
   `--txpool.pending/queued/basefee-max-size=512`. With ALL THREE layers raised the fill
   reaches 165k in ~24s and the drain runs at the chain's own block size (verified live:
   200M drain = 16,615 tps @ 573ms, 20 consecutive 100%-full 9,523-tx blocks).
2b. **reth's per-sender slot cap.** `--txpool.max-account-slots` defaults to **16**: the pool
   hard-ceilings at senders x 16 (800 spam accounts = 12.8k — measured plateau 12.7-12.9k on
   four separate fills). A 200M drain needs ~50k+. Launchers now pass
   `--txpool.max-account-slots=${PAY_ACCOUNT_SLOTS:-256}` so fresh chains fill to 150k+;
   chains started before the fix cannot produce a full-size drain (run-bench.sh detects the
   shallow pool and reports why instead of printing a confusing small-block capacity number).

The stop window itself costs ~8k txs of backlog (parallel remote pkill ~2s while the chain
keeps consuming) — another reason a drain needs a deep pool, and why run-bench.sh skips the
drain below a 30k backlog.
