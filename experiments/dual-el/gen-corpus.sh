#!/bin/bash
# Generate pre-signed raw-tx corpora for fast testnet prefills.
# Each FILL uses a DISJOINT account range (a drain consumes the nonces, so a
# reused range would be all nonce-too-low). 32k prefunded accounts / 800 per
# fill = up to 40 fills per corpus set; everything resets on a fresh chain.
#
# Usage: FILLS=12 TXS_PER_ACCT=240 ACCTS=200 MACHINES=4 OUT=/tmp/corpus ./gen-corpus.sh <ws-url>
# One fill = MACHINES x ACCTS x TXS_PER_ACCT = 4*200*240 = 192,000 txs.
set -e
WS=${1:?ws url of a live payment EL (nonce source; dump mode sends nothing)}
TXS=${TXS_PER_ACCT:-240}; A=${ACCTS:-200}; M=${MACHINES:-4}; F=${FILLS:-12}; OUT=${OUT:-/tmp/corpus}
SP=$(dirname "$0")/../../target/release/spammer
for f in $(seq 0 $((F-1))); do
  for m in $(seq 0 $((M-1))); do
    off=$(( (f*M + m) * A ))
    for attempt in 1 2; do
      rm -f ${OUT}-f${f}-m${m}.*
      SPAM_DUMP_FILE=${OUT}-f${f}-m${m} "$SP" ws --targets "$WS" -r 100000 -g 1 -a $A \
        --account-offset $off -t 120 --chain-id 1338 -l -x $TXS 2>/dev/null || true
      cat ${OUT}-f${f}-m${m}.[0-9]* > ${OUT}-f${f}-m${m}.txt 2>/dev/null && rm -f ${OUT}-f${f}-m${m}.[0-9]*
      n=$(wc -l < ${OUT}-f${f}-m${m}.txt 2>/dev/null || echo 0)
      [ "$n" -ge $((A*TXS*9/10)) ] && break
      echo "gen fill $f machine $m attempt $attempt: only $n txs; retrying"
    done
    [ "$n" -ge $((A*TXS*9/10)) ] || { echo "GEN_FAIL fill $f machine $m: only $n txs"; exit 1; }
  done
  echo "fill $f: $(cat ${OUT}-f${f}-m*.txt | wc -l) txs"
done
echo "CORPUS_OK: $F fills x $((M*A*TXS)) txs at ${OUT}-f*-m*.txt (accounts 0..$((F*M*A-1)))"
