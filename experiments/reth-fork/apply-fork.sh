#!/usr/bin/env bash
# Point Arc's 41 reth deps at a LOCAL reth checkout so we can modify reth itself
# (engine-tree parallel execution, state-root path) and rebuild incrementally.
#
#   RETH_FORK=~/reth-fork ./apply-fork.sh      # add the [patch] section to Cargo.toml
#   ./apply-fork.sh revert                       # remove it (required for Docker/CI/prod builds)
#
# WHY this is a script and NOT a committed Cargo.toml edit: the patch uses ABSOLUTE local
# paths, which are machine-specific and break the Docker image build + anyone else's checkout.
# So the fork wiring lives here and is applied on demand, on the dev box only.
#
# One-time setup of the fork itself:  cp -a ~/reth-2.3-ref ~/reth-fork   (v2.3.0 source)
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CARGO="$REPO/Cargo.toml"
MARK_BEGIN="# >>> reth-fork patch (apply-fork.sh) — DO NOT COMMIT >>>"
MARK_END="# <<< reth-fork patch <<<"
RETH_FORK="${RETH_FORK:-$HOME/reth-fork}"

revert(){
  if grep -qF "$MARK_BEGIN" "$CARGO"; then
    python3 - "$CARGO" "$MARK_BEGIN" "$MARK_END" <<'PY'
import sys
p,b,e=sys.argv[1:4]
s=open(p).read()
i,j=s.find(b),s.find(e)
if i!=-1 and j!=-1: s=s[:i].rstrip('\n')+'\n'+s[j+len(e):].lstrip('\n')
open(p,'w').write(s)
PY
    echo "reverted: reth-fork patch removed from Cargo.toml"
  else echo "nothing to revert (no reth-fork patch in Cargo.toml)"; fi
}

apply(){
  [ -d "$RETH_FORK/crates" ] || { echo "!! RETH_FORK not found: $RETH_FORK (cp -a ~/reth-2.3-ref ~/reth-fork)"; exit 1; }
  grep -qF "$MARK_BEGIN" "$CARGO" && { echo "already applied (revert first to re-point)"; exit 0; }
  # the exact 41 reth crates Arc pulls from the git tag
  mapfile -t deps < <(grep -oE '^reth-[a-z0-9-]+ = \{ git = "https://github.com/paradigmxyz/reth"' "$CARGO" | sed -E 's/ =.*//' | sort -u)
  [ "${#deps[@]}" -gt 0 ] || { echo "!! no reth git deps found in Cargo.toml"; exit 1; }
  RETH_FORK="$RETH_FORK" python3 - "$CARGO" "$MARK_BEGIN" "$MARK_END" "${deps[@]}" <<'PY'
import os, re, glob, sys
cargo, mb, me = sys.argv[1], sys.argv[2], sys.argv[3]
want = set(sys.argv[4:])
fork = os.environ["RETH_FORK"]
name2path = {}
for ct in glob.glob(fork + "/crates/**/Cargo.toml", recursive=True):
    m = re.search(r'(?m)^\s*name\s*=\s*"([^"]+)"', open(ct).read())
    if m and m.group(1) in want:
        name2path[m.group(1)] = os.path.dirname(ct)
missing = sorted(want - set(name2path))
if missing:
    sys.exit(f"!! could not locate in fork: {missing}")
lines = [mb, '[patch."https://github.com/paradigmxyz/reth"]']
for n in sorted(name2path):
    lines.append(f'{n} = {{ path = "{name2path[n]}" }}')
lines.append(me)
open(cargo, "a").write("\n" + "\n".join(lines) + "\n")
print(f"applied: {len(name2path)} reth crates -> {fork}")
PY
  echo "now: cargo build --release -p arc-node-execution   (first build ~10 min)"
}

case "${1:-apply}" in apply) apply;; revert) revert;; *) echo "usage: $0 [apply|revert]  (env: RETH_FORK)"; exit 1;; esac
