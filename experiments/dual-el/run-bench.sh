#!/usr/bin/env bash
# One-shot payment-lane throughput benchmark for the dashboard button:
#   start distributed spam -> warm up -> measure a window -> stop -> emit result JSON.
# Writes live progress + final result to /tmp/pay-bench/bench-status.json so the dashboard can poll it.
#
#   WINDOW=300 S=4 ACCTS=1000 ./run-bench.sh
set -uo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUN=/tmp/pay-bench; mkdir -p "$RUN"
STATUS="$RUN/bench-status.json"
WINDOW=${WINDOW:-300}
S=${S:-4}; ACCTS=${ACCTS:-1000}; RATE=${RATE:-12000}
DUR=$((WINDOW+60))
st(){ printf '%s\n' "$1" > "$STATUS"; }

st "{\"state\":\"running\",\"phase\":\"starting distributed spam\",\"window_s\":$WINDOW}"
if ! S=$S ACCTS=$ACCTS RATE=$RATE DUR=$DUR "$DIR/fleet/spam-fleet-distributed.sh" start >"$RUN/bench.log" 2>&1; then
  st '{"state":"error","phase":"spam start failed (see /tmp/pay-bench/bench.log)"}'; exit 1
fi
st "{\"state\":\"running\",\"phase\":\"warming up\",\"window_s\":$WINDOW}"
sleep 15
st "{\"state\":\"running\",\"phase\":\"measuring ${WINDOW}s\",\"window_s\":$WINDOW}"
rm -f "$RUN/result.json"
WINDOW=$WINDOW "$DIR/pay-throughput-bench.sh" measure >>"$RUN/bench.log" 2>&1 || true
"$DIR/fleet/spam-fleet-distributed.sh" stop >>"$RUN/bench.log" 2>&1 || true
if [ -f "$RUN/result.json" ]; then
  python3 -c "import json;r=json.load(open('$RUN/result.json'));print(json.dumps({'state':'done','result':r}))" > "$STATUS"
else
  st '{"state":"error","phase":"no result produced (see /tmp/pay-bench/bench.log)"}'
fi
