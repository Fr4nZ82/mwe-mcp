#!/usr/bin/env bash
# Offline smoke for the hermes bridge: fetch hermes-agent at
# BRIDGE_UPSTREAM_REF (HEAD = upstream tip — the canary), install the
# plugin pair into a scratch checkout, and run smoke_test.py through
# hermes's own plugin-discovery seams against the recording stub
# endpoint. No mwe-mcp server, no LLM, no third-party deps.
#
# Env:
#   BRIDGE_UPSTREAM_REF  upstream ref to test against (default: the manifest pin)
#   HERMES_SRC           optional local hermes-agent checkout to clone from
#                        (instead of the remote repo — fast local runs)
set -euo pipefail
cd "$(dirname "$0")"

REF="${BRIDGE_UPSTREAM_REF:-$(python3 -c 'import tomllib; print(tomllib.load(open("bridge.toml","rb"))["upstream"]["pin"])')}"
SRC="${HERMES_SRC:-$(python3 -c 'import tomllib; print(tomllib.load(open("bridge.toml","rb"))["upstream"]["repo"])')}"

SCRATCH="$(mktemp -d -t mwe-hermes-smoke.XXXXXX)"
trap 'rm -rf "$SCRATCH"' EXIT

echo "fetching hermes-agent @ ${REF} from ${SRC}"
git clone --quiet "$SRC" "$SCRATCH/hermes"
if [ "$REF" != "HEAD" ]; then
    git -C "$SCRATCH/hermes" checkout --quiet "$REF"
fi

# Install the plugin trio the way an operator does: the memory provider
# and the gateway media hook into $HERMES_HOME/plugins/ (out-of-tree),
# the context engine into the checkout's plugins/context_engine/ (its
# discovery has no user dir). The media hook is opt-in: smoke_test.py
# writes the matching `plugins.enabled: [mwe-media]` config.yaml.
mkdir -p "$SCRATCH/home/plugins"
ln -s "$(pwd)/plugins/memory/mwe" "$SCRATCH/home/plugins/mwe"
ln -s "$(pwd)/plugins/gateway/mwe-media" "$SCRATCH/home/plugins/mwe-media"
ln -s "$(pwd)/plugins/context_engine/mwe-truncate" "$SCRATCH/hermes/plugins/context_engine/mwe-truncate"

HERMES_HOME="$SCRATCH/home" PYTHONPATH="$SCRATCH/hermes" python3 smoke_test.py
