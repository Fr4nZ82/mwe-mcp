#!/usr/bin/env python3
"""Validate bridge compat manifests and run the offline smokes.

The CI entrypoint for `agents-bridges/` (.github/workflows/bridges.yml),
equally runnable locally:

    python3 _harness/run_smokes.py --check           # manifests only
    python3 _harness/run_smokes.py                   # + run offline smokes (pinned upstream)
    python3 _harness/run_smokes.py --upstream-head   # canary: smokes against upstream HEAD

Each bridge directory (any direct child not starting with `_` or `.`)
must carry a `bridge.toml` — the schema is documented in ../README.md.
The smoke command runs from the bridge directory with BRIDGE_UPSTREAM_REF
set to the manifest pin (or HEAD in canary mode); fetching the host
framework at that ref is the smoke script's own job.

Standard library only (tomllib: Python >= 3.11).
"""

import argparse
import os
import subprocess
import sys
import tomllib
from pathlib import Path

# The per-turn contract version currently specified in INTEGRATING.md.
# Bump in the same commit as the stamp there; every in-repo bridge must
# follow in that commit too (the lockstep rule), so a mismatch fails.
CURRENT_CONTRACT = 1

ROOT = Path(__file__).resolve().parent.parent  # agents-bridges/


def discover_bridges():
    return sorted(
        p for p in ROOT.iterdir()
        if p.is_dir() and not p.name.startswith(("_", "."))
    )


def validate(bridge_dir):
    """Return (manifest, [errors]) for one bridge directory."""
    errors = []
    path = bridge_dir / "bridge.toml"
    if not path.is_file():
        return None, [f"missing {path.relative_to(ROOT)}"]
    try:
        manifest = tomllib.loads(path.read_text())
    except tomllib.TOMLDecodeError as e:
        return None, [f"{path.relative_to(ROOT)}: invalid TOML: {e}"]

    def need(table, key, kind, where):
        val = table.get(key)
        if not isinstance(val, kind) or (kind is str and not val.strip()):
            errors.append(f"{where}.{key}: required, must be a non-empty {kind.__name__}")
        return val

    if need(manifest, "bridge", str, "bridge.toml") != bridge_dir.name:
        errors.append(f"bridge = {manifest.get('bridge')!r} must equal the directory name {bridge_dir.name!r}")
    need(manifest, "description", str, "bridge.toml")
    contract = need(manifest, "contract", int, "bridge.toml")
    if isinstance(contract, int) and contract != CURRENT_CONTRACT:
        errors.append(
            f"contract = {contract} but the current per-turn contract is v{CURRENT_CONTRACT} "
            "(bridges update in the same commit as a contract bump)"
        )
    upstream = manifest.get("upstream")
    if not isinstance(upstream, dict):
        errors.append("[upstream]: required table")
    else:
        for key in ("name", "repo", "pin"):
            need(upstream, key, str, "upstream")
    smoke = manifest.get("smoke")
    if not isinstance(smoke, dict):
        errors.append("[smoke]: required table")
    else:
        offline = need(smoke, "offline", str, "smoke")
        if isinstance(offline, str) and offline.startswith("./") \
                and not (bridge_dir / offline).is_file():
            errors.append(f"smoke.offline: {offline} not found in {bridge_dir.name}/")
        live = smoke.get("live")
        if live is not None and not isinstance(live, str):
            errors.append("smoke.live: must be a string when present")
    return manifest, errors


def run_smoke(bridge_dir, manifest, upstream_ref):
    cmd = manifest["smoke"]["offline"]
    env = dict(os.environ, BRIDGE_UPSTREAM_REF=upstream_ref)
    print(f"--- {bridge_dir.name}: {cmd} (BRIDGE_UPSTREAM_REF={upstream_ref})", flush=True)
    proc = subprocess.run(cmd, shell=True, cwd=bridge_dir, env=env)
    return proc.returncode == 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="validate manifests only")
    parser.add_argument("--upstream-head", action="store_true",
                        help="canary mode: run smokes against upstream HEAD instead of the pin")
    args = parser.parse_args()

    bridges = discover_bridges()
    if not bridges:
        print("no bridges yet — nothing to validate or smoke")
        return 0

    failed = False
    manifests = {}
    for bridge_dir in bridges:
        manifest, errors = validate(bridge_dir)
        if errors:
            failed = True
            for err in errors:
                print(f"FAIL {bridge_dir.name}: {err}")
        else:
            manifests[bridge_dir] = manifest
            print(f"ok   {bridge_dir.name}: contract v{manifest['contract']}, "
                  f"upstream {manifest['upstream']['name']} @ {manifest['upstream']['pin']}")
    if args.check or failed:
        return 1 if failed else 0

    for bridge_dir, manifest in manifests.items():
        ref = "HEAD" if args.upstream_head else manifest["upstream"]["pin"]
        if not run_smoke(bridge_dir, manifest, ref):
            failed = True
            print(f"FAIL {bridge_dir.name}: offline smoke exited non-zero")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
