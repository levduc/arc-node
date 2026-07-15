#!/usr/bin/env python3
"""Full-fleet compose surgery: validator1 stays local; validators 2-4 each move to their own
tailscale host. Reads .quake/soak4/compose.yaml (fresh from quake), writes:
  - compose.yaml (in place, backup .prefleet): only val1 services remain, peers -> tailscale
  - compose-valN.yaml for N in 2..4: that validator's CL+EL, paths under /home/papaduck/arc-fleet,
    peers -> tailscale, EL P2P published as 3030N:30303
All EVM ELs (local + remote) publish 3030N:30303 so every pair can dial over the tailnet.
"""
import copy, yaml

LOCAL_TS = "100.124.148.61"
HOSTS = {1: LOCAL_TS, 2: "100.85.150.119", 3: "100.70.62.92", 4: "100.86.97.40"}
LOCAL_BASE = "/home/papaduck/arc-node-paymentlane/.quake/soak4"
REMOTE_BASE = "/home/papaduck/arc-fleet/soak4"
COMPOSE = f"{LOCAL_BASE}/compose.yaml"
CL_IP = lambda n: f"172.21.1.{n-1}"
EL_IP = lambda n: f"172.21.2.{n-1}"
CL_HOSTPORT = lambda n: 27000 + (n - 1)
EL_HOSTPORT = lambda n: 30300 + n

CL_SUBS = [(f"/ip4/{CL_IP(n)}/tcp/27000", f"/ip4/{HOSTS[n]}/tcp/{CL_HOSTPORT(n)}") for n in HOSTS]
EL_SUBS = [(f"@{EL_IP(n)}:30303", f"@{HOSTS[n]}:{EL_HOSTPORT(n)}") for n in HOSTS]

def rw(cmd, subs):
    return [functools_reduce(a, subs) for a in cmd]
def functools_reduce(arg, subs):
    for o, n in subs: arg = arg.replace(o, n)
    return arg

c = yaml.safe_load(open(COMPOSE))
orig = copy.deepcopy(c)
for n in HOSTS:
    cl = c["services"][f"validator{n}_cl"]; cl["command"] = rw(cl["command"], CL_SUBS)
    el = c["services"][f"validator{n}_el"]; el["command"] = rw(el["command"], EL_SUBS)
    ports = el.setdefault("ports", [])
    pub = f"{EL_HOSTPORT(n)}:30303"
    if pub not in ports: ports.append(pub)

for n in (2, 3, 4):
    svcs = {}
    for kind in ("cl", "el"):
        name = f"validator{n}_{kind}"
        svc = c["services"].pop(name)
        svc["volumes"] = [v.replace(LOCAL_BASE, REMOTE_BASE) for v in svc["volumes"]]
        nets = svc.get("networks")
        if isinstance(nets, dict):
            for v in nets.values():
                if isinstance(v, dict): v.pop("ipv4_address", None)
        svc.pop("depends_on", None)
        svcs[name] = svc
    remote = {"services": svcs,
              "networks": {"default": {"name": "arc_testnet_default", "driver": "bridge", "internal": True},
                 "host-access": {"name": "arc_testnet_host-access", "driver": "bridge"}}}
    open(f"{LOCAL_BASE}/compose-val{n}.yaml", "w").write(yaml.dump(remote, sort_keys=False))

open(COMPOSE + ".prefleet", "w").write(yaml.dump(orig, sort_keys=False))
open(COMPOSE, "w").write(yaml.dump(c, sort_keys=False))
print("fleet surgery done: local=val1 only; compose-val{2,3,4}.yaml written")
