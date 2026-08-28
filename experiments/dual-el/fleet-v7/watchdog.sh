#!/bin/bash
R=/tmp/v7/report.md
while true; do
  sleep 180
  grep -q 'V7 COMPLETE' "$R" 2>/dev/null && exit 0
  AGE=$(( $(date +%s) - $(stat -c %Y "$R" 2>/dev/null || echo 0) ))
  if [ $AGE -gt 1200 ]; then
    echo "[$(date +%H:%M:%S)] 🚨 V7-WATCHDOG: report stale ${AGE}s — killing runner" >> "$R"
    pkill -9 -f '[v]7/run.sh' 2>/dev/null
    exit 1
  fi
done
