#!/usr/bin/env bash
# Fleet spammer: demo-bloat load profile against ALL fleet machines, run from this box.
#   ./spam-fleet.sh start | stop | status
# Profile (env-tunable): EVM lane = guzzler storage-write bloat, payment lane = pool-update
# transfers over the 10M preseed + slow organic growth. All loops pass -l (nonce resync on
# every respawn) so an EL restart anywhere self-heals within one loop cycle (<=10 min).
#   EVM_RATE=6 SLOTS=250 PAY_RATE=1500 POOL=0x2000000000:10000000 GROW_RATE=50
# Targets default to the LOCAL node only: payment ELs are gossip-meshed, so one feed reaches
# every pool — and each ws send awaits its ack, so remote (esp. Wi-Fi) targets gate the whole
# send loop (measured: 4-target incl. Wi-Fi ~1.5k tx/s vs local-only 5k+). Override
# EVM_TGTS/PAY_TGTS to fan out explicitly (e.g. if gossip is broken).
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"; cd "$REPO"
RUN=/tmp/dualel-fleet; mkdir -p "$RUN"; STOP="$RUN/spam.stop"
EVM_RATE=${EVM_RATE:-6}; SLOTS=${SLOTS:-250}
PAY_RATE=${PAY_RATE:-1500}; POOL=${POOL:-0x2000000000:10000000}; GROW_RATE=${GROW_RATE:-50}
EVM_TGTS=${EVM_TGTS:-"ws://127.0.0.1:8546"}
PAY_TGTS=${PAY_TGTS:-"ws://127.0.0.1:19546"}

start(){
  [ -f "$RUN/spam.pid" ] && kill -0 "$(cat "$RUN/spam.pid")" 2>/dev/null && { echo "!! already running (pid $(cat "$RUN/spam.pid")) — './spam-fleet.sh stop' first"; exit 1; }
  rm -f "$STOP"
  ( bloatloop(){ while [ ! -f "$STOP" ]; do target/release/spammer ws --targets "$EVM_TGTS" -r "$EVM_RATE" -t 600 -g 2 -a 200 -l --mix guzzler=100 --guzzler-fn-weights "storage-write=100@${SLOTS}" >"$RUN/spam_evm.log" 2>&1; sleep 1; done; }
    payloop(){ while [ ! -f "$STOP" ]; do target/release/spammer ws --targets "$PAY_TGTS" -r "$PAY_RATE" -t 600 -g 20 -a 1000 -l --recipient-pool "$POOL" --mix transfer=100 >"$RUN/spam_pay.log" 2>&1; sleep 1; done; }
    growloop(){ while [ ! -f "$STOP" ]; do target/release/spammer ws --targets "$PAY_TGTS" -r "$GROW_RATE" -t 600 -g 2 -a 200 -l --fresh-recipients --mix transfer=100 >"$RUN/spam_grow.log" 2>&1; sleep 1; done; }
    bloatloop & payloop & growloop & wait ) >/dev/null 2>&1 &
  echo $! >"$RUN/spam.pid"
  echo "✅ fleet spam started: evm ${EVM_RATE}tx/s guzzler@${SLOTS} | pay ${PAY_RATE}tx/s pool | grow ${GROW_RATE}tx/s"
  echo "   logs: $RUN/spam_{evm,pay,grow}.log    stop: ./spam-fleet.sh stop"
}
stop(){
  touch "$STOP"; sleep 2
  [ -f "$RUN/spam.pid" ] && kill "$(cat "$RUN/spam.pid")" 2>/dev/null; rm -f "$RUN/spam.pid"
  pkill -x spammer 2>/dev/null
  echo "✅ fleet spam stopped"
}
status(){
  echo "spammers running: $(pgrep -xc spammer || echo 0)"
  for f in spam_evm spam_pay spam_grow; do
    [ -f "$RUN/$f.log" ] && echo "$f: $(tail -1 "$RUN/$f.log" | sed 's/\x1b\[[0-9;]*m//g' | cut -c1-90)"
  done
}
case "${1:-}" in start) start;; stop) stop;; status) status;; *) echo "usage: $0 {start|stop|status}"; exit 1;; esac
