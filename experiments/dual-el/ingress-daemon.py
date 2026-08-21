#!/usr/bin/env python3
"""Closed-loop batched ingress daemon — the dedicated external load source.

Streams pre-signed raw txs (corpus files) to N payment ELs with POOL-AWARE pacing:
each target only receives while its pending+queued is below pool_target. Batches
of 100 (reth's JSON-RPC batch limit), optional global rate cap, hard tx and
wall-clock limits, heartbeat telemetry, single process, self-terminating.

Built after three invalid runs proved: acceptance != sustainable operation;
ingress must be closed-loop even when batched.

Usage:
  ingress-daemon.py --targets URL,URL,... --corpus-dir DIR --sets m0,m1,m2,m3 \
      --pool-target 21000 --rate 20000 --duration 900 [--max-txs N]

Target k is fed only from corpus files matching corpus-f*-<sets[k]>.txt (keeps
per-account nonce order: one sender's txs live in one set, streamed in order).
"""
import argparse, glob, json, re, sys, threading, time, urllib.request

p = argparse.ArgumentParser()
p.add_argument("--targets", required=True)
p.add_argument("--corpus-dir", default="/tmp")
p.add_argument("--sets", required=True)
p.add_argument("--pool-target", type=int, default=21000)
p.add_argument("--rate", type=int, default=20000, help="global tx/s cap")
p.add_argument("--duration", type=int, default=900, help="hard wall-clock stop (s)")
p.add_argument("--max-txs", type=int, default=0)
p.add_argument("--batch", type=int, default=100)
p.add_argument("--skip-files", type=int, default=0,
               help="skip the first N corpus files per set (their txs already mined)")
p.add_argument("--heartbeat", type=int, default=5)
args = p.parse_args()

targets = args.targets.split(",")
sets = args.sets.split(",")
assert len(targets) == len(sets), "one corpus set per target"

def fill_no(path):
    m = re.search(r"corpus-f(\d+)-", path)
    return int(m.group(1)) if m else 0

def rpc(url, method, params, timeout=10):
    req = urllib.request.Request(
        url, json.dumps({"jsonrpc": "2.0", "id": 1, "method": method,
                         "params": params}).encode(),
        {"Content-Type": "application/json"})
    return json.load(urllib.request.urlopen(req, timeout=timeout)).get("result")

def pool_depth(url):
    try:
        s = rpc(url, "txpool_status", [], timeout=5)
        return int(s["pending"], 16) + int(s["queued"], 16)
    except Exception:
        return -1  # unreachable counts as "do not feed"

class Feeder(threading.Thread):
    def __init__(self, idx, url, files, limiter):
        super().__init__(daemon=True)
        self.idx, self.url, self.limiter = idx, url, limiter
        self.lines = self._reader(files)
        self.sent = self.accepted = self.rejected = 0
        self.paused = False
        self.done = False

    def _reader(self, files):
        for f in files:
            with open(f) as fh:
                for line in fh:
                    line = line.strip()
                    if line:
                        yield line

    def run(self):
        batch = []
        depth = 0
        last_poll = 0.0
        while time.time() < DEADLINE and not STOP.is_set():
            now = time.time()
            if now - last_poll > 1.0:
                depth = pool_depth(self.url)
                last_poll = now
            if depth < 0 or depth >= args.pool_target:
                self.paused = True
                time.sleep(0.5)
                continue
            self.paused = False
            # assemble one batch
            try:
                while len(batch) < args.batch:
                    batch.append(next(self.lines))
            except StopIteration:
                if not batch:
                    self.done = True
                    return
            self.limiter.take(len(batch))
            body = json.dumps([
                {"jsonrpc": "2.0", "id": i, "method": "eth_sendRawTransaction",
                 "params": [tx]} for i, tx in enumerate(batch)])
            try:
                req = urllib.request.Request(
                    self.url, body.encode(), {"Content-Type": "application/json"})
                resp = json.load(urllib.request.urlopen(req, timeout=20))
                good = sum(1 for r in resp if "result" in r)
                self.accepted += good
                self.rejected += len(batch) - good
            except Exception:
                self.rejected += len(batch)
                time.sleep(0.5)
            self.sent += len(batch)
            batch = []
            if args.max_txs and TOTAL() >= args.max_txs:
                STOP.set()
                return


class Limiter:
    """Global tx/s cap shared by all feeders."""
    def __init__(self, rate):
        self.rate = rate
        self.lock = threading.Lock()
        self.t0 = time.time()
        self.granted = 0

    def take(self, n):
        if self.rate <= 0:
            return
        with self.lock:
            self.granted += n
            due = self.t0 + self.granted / self.rate
        wait = due - time.time()
        if wait > 0:
            time.sleep(wait)


STOP = threading.Event()
DEADLINE = time.time() + args.duration
limiter = Limiter(args.rate)
feeders = []
for i, (url, st) in enumerate(zip(targets, sets)):
    files = sorted(glob.glob(f"{args.corpus_dir}/corpus-f*-{st}.txt"), key=fill_no)
    files = files[args.skip_files:]
    if not files:
        print(f"FATAL: no corpus files for set {st}", flush=True)
        sys.exit(1)
    feeders.append(Feeder(i, url, files, limiter))

def TOTAL():
    return sum(f.sent for f in feeders)

for f in feeders:
    f.start()

t0 = time.time()
last_sent = 0
while time.time() < DEADLINE and not STOP.is_set() and not all(f.done for f in feeders):
    time.sleep(args.heartbeat)
    sent = TOTAL()
    rate_now = (sent - last_sent) / args.heartbeat
    last_sent = sent
    stat = " ".join(
        f"T{f.idx}:{'PAUSE' if f.paused else 'FEED'}/s{f.sent}/r{f.rejected}"
        for f in feeders)
    print(f"HB t={time.time()-t0:.0f}s sent={sent} rate={rate_now:.0f}/s {stat}",
          flush=True)

STOP.set()
time.sleep(1)
print(f"DAEMON_DONE sent={TOTAL()} accepted={sum(f.accepted for f in feeders)} "
      f"rejected={sum(f.rejected for f in feeders)} elapsed={time.time()-t0:.0f}s",
      flush=True)