#!/usr/bin/env bash
# Fan-out variant of spam-fleet.sh: feeds EVERY machine's ELs directly instead of relying on
# payment-EL gossip. Use when gossip is broken/untrusted, or to exercise each node's own
# tx-ingestion path (RPC -> pool -> gossip) explicitly.
#
# NOTE the throughput cost: each ws send awaits its ack, so the slowest target (Wi-Fi boxes)
# gates the whole loop — measured ~1.5k tx/s vs 5k+ local-only. For raw throughput use
# spam-fleet.sh (local target, gossip distributes).
#   ./spam-fleet-fanout.sh start | stop | status    (same env knobs as spam-fleet.sh)
set -uo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export EVM_TGTS=${EVM_TGTS:-"ws://127.0.0.1:8546,ws://100.85.150.119:8646,ws://100.70.62.92:8746,ws://100.86.97.40:8846"}
export PAY_TGTS=${PAY_TGTS:-"ws://127.0.0.1:19546,ws://100.85.150.119:19646,ws://100.70.62.92:19746,ws://100.86.97.40:19846"}
exec "$DIR/spam-fleet.sh" "$@"
