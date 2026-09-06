#!/usr/bin/env bash
# Offline smoke for the nanoclaw bridge: fetch nanoclaw at BRIDGE_UPSTREAM_REF,
# install the skill and the template into a scratch fork the way an operator
# does, then drive the real poll loop against the recording stub endpoint.
# No mwe-mcp server, no model, no Docker.
#
# Env:
#   BRIDGE_UPSTREAM_REF  upstream ref to test against (default: the manifest pin)
#   NANOCLAW_SRC         a local nanoclaw checkout to clone from, instead of the
#                        remote — fast local runs
#   MWE_SMOKE_KEEP       keep the scratch fork on exit and print where it is
set -euo pipefail
cd "$(dirname "$0")"
BRIDGE="$(pwd)"

REF="${BRIDGE_UPSTREAM_REF:-$(python3 -c 'import tomllib; print(tomllib.load(open("bridge.toml","rb"))["upstream"]["pin"])')}"
SRC="${NANOCLAW_SRC:-$(python3 -c 'import tomllib; print(tomllib.load(open("bridge.toml","rb"))["upstream"]["repo"])')}"

for tool in bun pnpm python3 git; do
    command -v "$tool" >/dev/null || { echo "smoke needs $tool on PATH"; exit 2; }
done

SCRATCH="$(mktemp -d -t mwe-nanoclaw-smoke.XXXXXX)"
STUB_PID=""
cleanup() {
    [ -n "$STUB_PID" ] && kill "$STUB_PID" 2>/dev/null || true
    if [ -n "${MWE_SMOKE_KEEP:-}" ]; then
        echo "scratch fork kept at $SCRATCH"
    else
        rm -rf "$SCRATCH"
    fi
}
trap cleanup EXIT

FORK="$SCRATCH/nanoclaw"
echo "fetching nanoclaw @ ${REF} from ${SRC}"
git clone --quiet "$SRC" "$FORK"
if [ "$REF" != "HEAD" ]; then
    git -C "$FORK" checkout --quiet "$REF"
fi

# Install the bridge the way the served installer will: two directory copies.
mkdir -p "$FORK/.claude/skills" "$FORK/templates"
cp -R "$BRIDGE/skills/add-mwe-memory" "$FORK/.claude/skills/add-mwe-memory"
cp -R "$BRIDGE/templates/mwe" "$FORK/templates/mwe"

echo "installing dependencies (host, then agent runner)"
(cd "$FORK" && pnpm install --frozen-lockfile --silent)
(cd "$FORK/container/agent-runner" && bun install --silent)

# The template, through nanoclaw's own parser — the same one `ncl groups
# create` uses, minus the database a stamp would need.
echo "validating the template"
(cd "$FORK" && bun "$BRIDGE/smoke_template.ts")

# The skill, applied headlessly through nanoclaw's own apply engine. The
# operator answers three questions; here they come from the environment, which
# is the engine's documented pipeline posture.
echo "applying add-mwe-memory"
(
    cd "$FORK"
    export NC_INPUT_MWE_SERVER_URL="http://127.0.0.1:8742/mcp"
    export NC_INPUT_MWE_ADMIN_SENDER="telegram:1"
    export NC_INPUT_MWE_ADMIN_USER="alice"
    bun "$BRIDGE/smoke_apply.ts"
)

# Applying twice must change nothing, and removing must give nanoclaw's own
# files back byte for byte. That is what makes the skill safe to re-run after
# an upgrade — and a replacement usually contains its own anchor, so getting
# this wrong duplicates every spliced line instead of failing loudly.
echo "checking the install is idempotent"
(
    cd "$FORK"
    PATCHER=.claude/skills/add-mwe-memory/apply-fork-patches.ts
    UPSTREAM="container/agent-runner/src/poll-loop.ts container/agent-runner/src/memory"
    UPSTREAM="$UPSTREAM src/modules/index.ts container/agent-runner/src/mcp-tools/index.ts"
    before="$(git status --porcelain | sort)"
    bun "$PATCHER" | grep -q '^applied: 0 edit(s)' || { echo "FAIL: re-applying changed the fork"; exit 1; }
    bun "$PATCHER" --remove >/dev/null
    # shellcheck disable=SC2086
    git diff --quiet -- $UPSTREAM || { echo "FAIL: --remove did not restore nanoclaw's own files"; exit 1; }
    bun "$PATCHER" >/dev/null
    [ "$before" = "$(git status --porcelain | sort)" ] || { echo "FAIL: the round trip left the fork different"; exit 1; }
    echo "ok   applying twice is a no-op, and --remove restores the originals"
)

# The tests the skill ships, run where they landed.
echo "running the installed tests"
(cd "$FORK" && pnpm exec vitest run src/mwe-wiring.test.ts src/modules/mwe/mwe-host.test.ts)
(cd "$FORK/container/agent-runner" && bun test src/mwe/mwe-turn.test.ts)

# Typecheck both trees: the host module and the container module compile
# against the real nanoclaw types, which is the cheapest proof that a seam did
# not move under them.
echo "typechecking the patched fork"
(cd "$FORK" && pnpm exec tsc --noEmit)
(cd "$FORK/container/agent-runner" && pnpm exec tsc --noEmit)

# The per-turn contract, through the real poll loop.
echo "starting the stub endpoint"
URL_FILE="$SCRATCH/stub-url"
CALLS_FILE="$SCRATCH/stub-calls.json"
RESPONSES_FILE="$SCRATCH/stub-responses.json"
echo '{}' > "$RESPONSES_FILE"
python3 "$BRIDGE/stub_runner.py" --url-file "$URL_FILE" --calls "$CALLS_FILE" --responses "$RESPONSES_FILE" &
STUB_PID=$!
for _ in $(seq 1 100); do [ -s "$URL_FILE" ] && break; sleep 0.1; done
[ -s "$URL_FILE" ] || { echo "the stub endpoint never came up"; exit 1; }

NANOCLAW_FORK="$FORK" \
MWE_STUB_URL="$(cat "$URL_FILE")" \
MWE_STUB_CALLS="$CALLS_FILE" \
MWE_STUB_RESPONSES="$RESPONSES_FILE" \
    bun "$BRIDGE/smoke_test.ts"
