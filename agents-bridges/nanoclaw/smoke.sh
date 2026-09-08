#!/usr/bin/env bash
# Offline smoke for the nanoclaw bridge: fetch nanoclaw at BRIDGE_UPSTREAM_REF,
# copy the skill and the template into a scratch fork, apply the skill through
# the headless driver the bridge ships (the one the served installer runs),
# apply it a second time the way an upgrade does — including over a fork an
# older version of the skill wrote — then drive the real poll loop against the
# recording stub endpoint.
# No mwe-mcp server, no model, no Docker — so the served installer's own half
# (nanoclaw's setup, Telegram, the restart) is not exercised here.
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
SKILL_IN_FORK=".claude/skills/add-mwe-memory"
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

# The skill, applied headlessly through the driver the bridge ships into the
# fork — the same one the served installer runs. The operator answers three
# questions; here they come from the environment, which is the engine's
# documented pipeline posture. The restart is the caller's: this harness owns
# process lifecycle.
echo "applying add-mwe-memory"
(
    cd "$FORK"
    export NC_INPUT_MWE_SERVER_URL="http://127.0.0.1:8742/mcp"
    export NC_INPUT_MWE_ADMIN_SENDER="telegram:1"
    export NC_INPUT_MWE_ADMIN_USER="alice"
    export NC_SKIP_EFFECTS=restart
    pnpm exec tsx "$SKILL_IN_FORK/apply-headless.ts" "$SKILL_IN_FORK"
)

# There is no memory server here to claim a token from, and the host module
# wants one on disk to consider itself configured. A fake is the only kind
# that belongs in a test.
printf '\nMWE_TOKEN=test-jwt\n' >> "$FORK/.env"

# Splicing the reach-ins twice must change nothing, and removing them must give
# nanoclaw's own files back byte for byte. That is what makes the wiring safe to
# re-run after an upgrade — and a replacement usually contains its own anchor,
# so getting this wrong duplicates every spliced line instead of failing loudly.
echo "checking the reach-ins are idempotent"
(
    cd "$FORK"
    PATCHER=.claude/skills/add-mwe-memory/apply-fork-patches.ts
    UPSTREAM="container/agent-runner/src/poll-loop.ts container/agent-runner/src/memory"
    UPSTREAM="$UPSTREAM src/modules/index.ts container/agent-runner/src/mcp-tools/index.ts"
    UPSTREAM="$UPSTREAM container/agent-runner/src/destinations.ts src/claude-md-compose.ts"
    before="$(git status --porcelain | sort)"
    bun "$PATCHER" | grep -q '^applied: 0 edit(s)' || { echo "FAIL: re-applying changed the fork"; exit 1; }
    bun "$PATCHER" --remove >/dev/null
    # shellcheck disable=SC2086
    git diff --quiet -- $UPSTREAM || { echo "FAIL: --remove did not restore nanoclaw's own files"; exit 1; }
    bun "$PATCHER" >/dev/null
    [ "$before" = "$(git status --porcelain | sort)" ] || { echo "FAIL: the round trip left the fork different"; exit 1; }
    echo "ok   splicing them twice is a no-op, and --remove restores the originals"
)

# The other half of an upgrade: the fork being re-applied to was written by the
# PREVIOUS version of the skill, and an edit whose own lines changed shape since
# then must be brought forward, not spliced in beside the shape it replaces.
echo "checking an older install upgrades in place"
(
    cd "$FORK"
    bun "$BRIDGE/upgrade_check.ts"
    # It puts every file back where it found it, so the fork is still applied.
    bun .claude/skills/add-mwe-memory/apply-fork-patches.ts | grep -q '^applied: 0 edit(s)' || {
        echo "FAIL: the upgrade check left the fork off its applied state"
        exit 1
    }
)

# An upgrade is a re-apply, and the modules have to come with it. `nc:copy`
# leaves a destination that already exists, so a skill without its refresh step
# reports success on every step and leaves the fork on the code it had — which
# from the outside looks like nothing at all, until a wiring test fails.
echo "checking a re-apply carries a changed module"
(
    cd "$FORK"
    MARKER='// a module the skill changed since the last apply'
    echo "$MARKER" >> .claude/skills/add-mwe-memory/host/events.ts
    export NC_INPUT_MWE_SERVER_URL="http://127.0.0.1:8742/mcp"
    export NC_INPUT_MWE_ADMIN_SENDER="telegram:1"
    export NC_INPUT_MWE_ADMIN_USER="alice"
    export NC_SKIP_EFFECTS=restart
    out="$(pnpm exec tsx "$SKILL_IN_FORK/apply-headless.ts" "$SKILL_IN_FORK")"
    grep -qF "$MARKER" src/modules/mwe/events.ts || {
        echo "FAIL: re-applying the skill left the fork on the old module"
        exit 1
    }
    # The token step is owed until somebody satisfies it, and by now the
    # token is in .env — printed again it would be an instruction to redo
    # something already done, which is what the served installer's run
    # would read like from beginning to end.
    if echo "$out" | grep -q 'manual step'; then
        echo "FAIL: the token step is still printed once the token is in .env"
        exit 1
    fi
    echo "ok   the token instruction stops once the token is in .env"
    # Back to what the bridge ships, on both sides of the copy.
    cp "$BRIDGE/skills/add-mwe-memory/host/events.ts" .claude/skills/add-mwe-memory/host/events.ts
    pnpm exec tsx .claude/skills/add-mwe-memory/refresh-modules.ts >/dev/null
    ! grep -qF "$MARKER" src/modules/mwe/events.ts || {
        echo "FAIL: the refresh step did not put the module back"
        exit 1
    }
    echo "ok   a re-apply carries a changed module into the fork"
)

# The restart step the harness skips (it owns process lifecycle), run here for
# the one path that needs no host: a fork where no group carries the plugin
# must say so and exit clean, not reach for ncl.
echo "checking the group restart step"
(
    cd "$FORK"
    out="$(pnpm exec tsx .claude/skills/add-mwe-memory/restart-mwe-groups.ts)"
    echo "$out" | grep -q 'no group carries the mwe plugin yet' || {
        echo "FAIL: the restart step did not recognise a fork with no mwe group: $out"
        exit 1
    }
    echo "ok   the restart step is a no-op with no mwe group"
)

# The tests the skill ships, run where they landed. Then nanoclaw's own
# conformance suite, narrowed to this skill: it drives SKILL.md's fences with
# the fixtures beside them, so a capture nothing answers or a when:-guard no
# scenario reaches is red here rather than in the fork's CI. Narrowed, because
# a red in some other skill's fixtures is nanoclaw's news, not this bridge's.
echo "running the installed tests"
(cd "$FORK" && pnpm exec vitest run src/mwe-wiring.test.ts src/modules/mwe/mwe-host.test.ts)
(cd "$FORK/container/agent-runner" && bun test src/mwe/mwe-turn.test.ts)
(cd "$FORK" && pnpm exec vitest run scripts/skill-conformance.test.ts -t add-mwe-memory)

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
