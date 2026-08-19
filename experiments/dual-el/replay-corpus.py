#!/usr/bin/env python3
"""Prefill a payment EL from a pre-signed raw-tx corpus file (one 0x-hex tx per line).

Usage: replay-corpus.py <corpus-file> <rpc-url> [workers=8] [batch=100]
Streams eth_sendRawTransaction in reth-sized batches; prints accepted/rejected.
Corpus txs must match the chain (chainId, prefunded senders, nonce-0 fresh chain).
"""
import json, sys, threading, urllib.request

path, url = sys.argv[1], sys.argv[2]
workers = int(sys.argv[3]) if len(sys.argv) > 3 else 8
batch_n = int(sys.argv[4]) if len(sys.argv) > 4 else 100

txs = [l.strip() for l in open(path) if l.strip()]
batches = [txs[i:i+batch_n] for i in range(0, len(txs), batch_n)]
lock = threading.Lock()
idx = 0
ok = err = 0

def worker():
    global idx, ok, err
    while True:
        with lock:
            global idx
            if idx >= len(batches): return
            my = batches[idx]; idx += 1
        body = json.dumps([
            {"jsonrpc":"2.0","id":i,"method":"eth_sendRawTransaction","params":[tx]}
            for i, tx in enumerate(my)
        ]).encode()
        req = urllib.request.Request(url, body, {"Content-Type":"application/json"})
        try:
            resp = json.load(urllib.request.urlopen(req, timeout=30))
            good = sum(1 for r in resp if "result" in r)
            with lock:
                globals()['ok'] = ok + good
                globals()['err'] = err + len(my) - good
        except Exception:
            with lock:
                globals()['err'] = err + len(my)

threads = [threading.Thread(target=worker) for _ in range(workers)]
import time; t0 = time.time()
for t in threads: t.start()
for t in threads: t.join()
dt = time.time() - t0
print(f"replayed {len(txs)} txs in {dt:.1f}s ({len(txs)/dt:.0f} tx/s): accepted={ok} rejected={err}")
