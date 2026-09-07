#!/bin/sh
# The second half of the served NanoClaw installer: it turns a fork that has
# the bridge files in it into an assistant answering on Telegram with this
# memory. The dashboard appends it, verbatim, to the script that resolves the
# fork and writes the files — see `render_install_sh_nanoclaw` in
# `crates/mwe-dashboard/src/routes/bridges.rs`. It never travels into a fork.
#
# What it reads, all set by that first half (or exported by hand to re-run it):
#   NANOCLAW_DIR   the fork it works in                              required
#   MWE_ORIGIN     this memory server's public origin, no trailing / required
#   MWE_CLAIM      a single-use install claim from the dashboard     optional
#
# What it asks the person, on the terminal, unless the environment answers:
#   MWE_TELEGRAM_BOT_TOKEN   the bot token from BotFather
#   MWE_TELEGRAM_OPERATOR_ID the person's own numeric Telegram id
#   MWE_OPERATOR_USER        their user id in this memory (a claim carries it)
#
# Knobs:
#   MWE_FILES_ONLY=1   stop after the files, for a fork you set up yourself
#   MWE_WAIT_SECONDS   how long to watch for the first turn (default 120)
#
# It needs a terminal: NanoClaw's own setup asks two questions no environment
# variable answers, and the Claude sign-in opens a browser. Every step is
# re-runnable — the whole command is also the way to update an install.

MWE_BOT_CONSUMER="mwe"
MWE_AGENT_GROUP="mwe"
MWE_SKILL_DIR=".claude/skills/add-mwe-memory"

say() { printf '%s\n' "$*"; }
step() { printf '\n== %s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

[ -n "${NANOCLAW_DIR:-}" ] || die "NANOCLAW_DIR is not set — run the command the dashboard serves"
[ -n "${MWE_ORIGIN:-}" ] || die "MWE_ORIGIN is not set — run the command the dashboard serves"
cd "$NANOCLAW_DIR" || die "cannot enter $NANOCLAW_DIR"

MWE_ENV="$NANOCLAW_DIR/.env"
NCL="$NANOCLAW_DIR/bin/ncl"

if [ "${MWE_FILES_ONLY:-}" = "1" ]; then
  say ""
  say "Stopping after the files, because MWE_FILES_ONLY=1. What is left is the"
  say "manual path in agents-bridges/nanoclaw/README.md: stamp the agent, apply"
  say "the skill, put a standard consumer token in .env as MWE_TOKEN, fill in"
  say "senderMap in mwe.json, connect a channel."
  exit 0
fi

# ---------------------------------------------------------------------
# The terminal, and the answers only a person has
# ---------------------------------------------------------------------

# NanoClaw's setup reads its own prompts from stdin in raw mode, and its Claude
# sign-in reads /dev/tty directly. Piped into `sh`, this script's stdin is the
# script itself, so everything interactive is handed the terminal explicitly.
# Without one there is nothing to hand it, and hanging forever is the worst
# possible answer.
# The probe runs in a subshell: a redirection that fails on `exec` kills a
# non-interactive shell outright, and this one has a message to deliver first.
if ! ( : </dev/tty ) 2>/dev/null; then
  die "no terminal available. Run this command in a terminal you are sitting at:
       NanoClaw's setup asks two questions of its own and opens a browser for
       the Claude sign-in, and neither can be answered by a pipe."
fi

# ask <var> <question> — one line, read from the terminal rather than stdin.
ask() {
  printf '%s\n> ' "$2" >&2
  read -r _answer </dev/tty || die "nothing to read from the terminal"
  eval "$1=\$_answer"
}

# Replace a key in .env, or add it, without ever echoing the value.
#
# Rewrite-then-move rather than test-then-append: `grep -v` exits non-zero
# when it keeps nothing, which is exactly the case where the file holds
# that one key — and an append after a skipped rewrite leaves the stale
# line first, which is the one nanoclaw reads.
env_set() {
  _tmp="$MWE_ENV.mwe-tmp"
  grep -v "^$1=" "$MWE_ENV" > "$_tmp" 2>/dev/null || :
  printf '%s=%s\n' "$1" "$2" >> "$_tmp"
  mv "$_tmp" "$MWE_ENV"
}
env_has() { grep -qs "^$1=." "$MWE_ENV"; }

step "What only you can tell me"

if [ -z "${MWE_TELEGRAM_BOT_TOKEN:-}" ] && env_has TELEGRAM_BOT_TOKEN; then
  say "Keeping the Telegram bot token already in .env."
  MWE_TELEGRAM_BOT_TOKEN="$(sed -n 's/^TELEGRAM_BOT_TOKEN=//p' "$MWE_ENV" | head -n1)"
fi
if [ -z "${MWE_TELEGRAM_BOT_TOKEN:-}" ]; then
  say "Telegram bots are made by a bot. In Telegram, message @BotFather, send"
  say "/newbot, pick a name and a username ending in \"bot\", and it hands you a"
  say "token that looks like 123456:ABC-DEF... Paste it here."
  ask MWE_TELEGRAM_BOT_TOKEN "The bot token from BotFather"
fi
case "$MWE_TELEGRAM_BOT_TOKEN" in
  [0-9]*:?*) : ;;
  *) die "that does not look like a BotFather token (<numbers>:<secret>)" ;;
esac

# Ask Telegram whether the token is live now, rather than twenty minutes in.
if command -v curl >/dev/null 2>&1; then
  if curl -fsS "https://api.telegram.org/bot$MWE_TELEGRAM_BOT_TOKEN/getMe" >/dev/null 2>&1; then
    say "Telegram recognises the bot."
  else
    die "Telegram refuses that token. A /revoke or a fresh /token in @BotFather
       invalidates the old one — send /token to @BotFather for the current value."
  fi
fi

if [ -z "${MWE_TELEGRAM_OPERATOR_ID:-}" ]; then
  say ""
  say "Your own Telegram id is a number. Any \"what is my id\" bot tells you,"
  say "@userinfobot among them."
  ask MWE_TELEGRAM_OPERATOR_ID "Your numeric Telegram id"
fi
case "$MWE_TELEGRAM_OPERATOR_ID" in
  '' | *[!0-9]*) die "a Telegram user id is digits only, and yours is a positive number" ;;
esac

env_set TELEGRAM_BOT_TOKEN "$MWE_TELEGRAM_BOT_TOKEN"

# ---------------------------------------------------------------------
# The consumer token
# ---------------------------------------------------------------------

step "The consumer token"

if [ -n "${MWE_CLAIM:-}" ]; then
  command -v curl >/dev/null 2>&1 || die "curl is needed to redeem the install claim"
  say "Redeeming the install claim (good once, and only for a few minutes)…"
  _claimed="$(printf 'claim=%s' "$MWE_CLAIM" |
    curl -fsS -X POST --data-binary @- \
      -H 'Content-Type: application/x-www-form-urlencoded' \
      "$MWE_ORIGIN/bridges/nanoclaw/claim")" ||
    die "the claim was refused. Open $MWE_ORIGIN/dashboard/bridges/nanoclaw and
       copy a fresh command — a claim is good once and expires."
  _token="$(printf '%s\n' "$_claimed" | sed -n 's/^token=//p')"
  MWE_OPERATOR_USER="$(printf '%s\n' "$_claimed" | sed -n 's/^operator_user_id=//p')"
  [ -n "$_token" ] && [ -n "$MWE_OPERATOR_USER" ] ||
    die "the claim answer was not what this installer expects"
  env_set MWE_TOKEN "$_token"
  _token=""
  _claimed=""
  say "Written to .env as MWE_TOKEN. It is never printed here."
else
  if env_has MWE_TOKEN; then
    say "Using the consumer token already in .env."
  else
    say "No install claim in the command, so the token stays yours to mint."
  fi
  if [ -z "${MWE_OPERATOR_USER:-}" ]; then
    ask MWE_OPERATOR_USER "Your user id in this memory (lowercase letters and digits)"
  fi
fi
case "$MWE_OPERATOR_USER" in
  '' | *[!a-z0-9]*) die "a memory user id is lowercase letters and digits, nothing else" ;;
esac

# ---------------------------------------------------------------------
# NanoClaw's own setup
# ---------------------------------------------------------------------

step "NanoClaw's setup"

# The zone a reminder is read on. NanoClaw detects it and then asks you to
# confirm; when this host can answer for it, take the question away.
if ! env_has TZ; then
  _tz="$(timedatectl show -p Timezone --value 2>/dev/null)" || _tz=""
  if [ -z "$_tz" ]; then
    _tz="$(readlink /etc/localtime 2>/dev/null | sed -n 's|.*/zoneinfo/||p')" || _tz=""
  fi
  if [ -n "$_tz" ]; then
    env_set TZ "$_tz"
    say "Timezone: $_tz."
  fi
fi
_skip="cli-agent,channel,first-chat"
if env_has TZ; then
  _skip="$_skip,timezone"
fi

say "NanoClaw installs what it needs (Node, pnpm, Docker) and builds its"
say "sandbox image here. It asks you two things this installer cannot answer:"
say "  · \"How would you like to begin?\" — Standard setup, which is the default."
say "  · \"How would you like to connect to Claude?\" — pick the subscription"
say "    sign-in; it opens your browser and keeps the token in its own vault."
say "The channel, the agent, the sandbox image and the runtime are answered."
say ""

# Each of these takes a question away. The image source and the runtime have no
# other silencer; the display name and the agent name are asked once and
# reused; the assist offer appears on any failure path.
NANOCLAW_HARDENED_IMAGE=false \
NANOCLAW_AGENT_PROVIDER=claude \
NANOCLAW_DISPLAY_NAME="$MWE_OPERATOR_USER" \
NANOCLAW_AGENT_NAME="$MWE_AGENT_GROUP" \
NANOCLAW_SKIP="$_skip" \
NANOCLAW_SKIP_CLAUDE_ASSIST=1 \
  bash nanoclaw.sh </dev/tty ||
  die "NanoClaw's setup did not finish. Read the last lines above, fix what they
       name, and run this same command again — every step of it is re-runnable."

# ---------------------------------------------------------------------
# The agent group
# ---------------------------------------------------------------------

step "The agent"

# The setup stamps the `mwe` template because the first half named it in .env,
# and clears the pick once it has. This is the other case: a fork where that
# did not happen, or one that had NanoClaw in it already.
if ls -d groups/*/plugins/mwe >/dev/null 2>&1; then
  say "An agent already carries the mwe plugin."
else
  "$NCL" groups create --template mwe --name "$MWE_AGENT_GROUP" --new ||
    die "could not stamp the mwe template. \`$NCL groups list\` says what is there."
  say "Stamped the mwe template as agent group \"$MWE_AGENT_GROUP\"."
fi

# ---------------------------------------------------------------------
# The memory
# ---------------------------------------------------------------------

step "The memory"

NC_INPUT_MWE_SERVER_URL="$MWE_ORIGIN/mcp" \
NC_INPUT_MWE_ADMIN_SENDER="telegram:$MWE_TELEGRAM_OPERATOR_ID" \
NC_INPUT_MWE_ADMIN_USER="$MWE_OPERATOR_USER" \
NC_SKIP_EFFECTS=restart \
  pnpm exec tsx "$MWE_SKILL_DIR/apply-headless.ts" "$MWE_SKILL_DIR" ||
  die "the memory skill did not apply. The lines above name the step that stopped;
       fix it and run this same command again."

# ---------------------------------------------------------------------
# Telegram
# ---------------------------------------------------------------------

step "Telegram"

# NanoClaw's own channel skill, through the same headless driver. Its pairing
# step is the one thing skipped: it exists to learn a chat id and a Telegram
# user id from a code sent by hand, and both are already known. A private
# chat's id is the person's own id, which is what makes that possible.
NC_INPUT_BOT_TOKEN="$MWE_TELEGRAM_BOT_TOKEN" \
NC_INPUT_ADD_ANOTHER=no \
NC_SKIP_EFFECTS=step \
  pnpm exec tsx "$MWE_SKILL_DIR/apply-headless.ts" .claude/skills/add-telegram ||
  die "the Telegram channel did not install. The lines above name the step that
       stopped; fix it and run this same command again."

# What pairing writes when the code matches: the chat, the person, and — on an
# install with no owner yet — the role that lets their messages through.
"$NCL" messaging-groups create \
  --channel-type telegram --platform-id "telegram:$MWE_TELEGRAM_OPERATOR_ID" --is-group 0 ||
  die "could not register your Telegram chat"
"$NCL" users create --id "telegram:$MWE_TELEGRAM_OPERATOR_ID" --kind telegram ||
  die "could not register you as a Telegram user"

if "$NCL" roles list --json 2>/dev/null | grep -Eq '"role"[[:space:]]*:[[:space:]]*"owner"'; then
  say "This install already has an owner, so no role is granted here. If your"
  say "messages come back held for approval, add yourself to the agent:"
  say "  $NCL members add --user telegram:$MWE_TELEGRAM_OPERATOR_ID --group <agent group id>"
else
  "$NCL" roles grant --user "telegram:$MWE_TELEGRAM_OPERATOR_ID" --role owner ||
    warn "could not grant you the owner role — your first message may be held for approval"
fi

"$NCL" wirings create \
  --channel-type telegram --platform-id "telegram:$MWE_TELEGRAM_OPERATOR_ID" \
  --agent-group "$MWE_AGENT_GROUP" ||
  die "could not wire your chat to the agent"

# ---------------------------------------------------------------------
# Restart, both halves
# ---------------------------------------------------------------------

step "Restart"

bash setup/lib/restart.sh || die "the host service did not come back up"
# A session container that keeps running answers from the code it read at boot:
# no ingest, no recall block, and nothing in any log saying so.
pnpm exec tsx "$MWE_SKILL_DIR/restart-mwe-groups.ts" ||
  warn "the agent containers were not replaced — run
       pnpm exec tsx $MWE_SKILL_DIR/restart-mwe-groups.ts once the host is up"

# ---------------------------------------------------------------------
# Does it answer, and does it remember?
# ---------------------------------------------------------------------

step "Say hello"

_wait="${MWE_WAIT_SECONDS:-120}"
_seen=no
if docker ps >/dev/null 2>&1; then
  say "Open your bot in Telegram and send it a message. Watching for ${_wait}s…"
  _deadline=$(( $(date +%s) + _wait ))
  while [ "$(date +%s)" -lt "$_deadline" ]; do
    for _c in $(docker ps -q --filter label=nanoclaw-session 2>/dev/null || true); do
      # The whole log, not a tail: the runner says this at boot, so on a
      # container that has been talking a while the line is at the top.
      if docker logs "$_c" 2>&1 | grep -q '\[mwe\] memory is on'; then
        _seen=yes
      fi
    done
    if [ "$_seen" = yes ]; then
      break
    fi
    sleep 3
  done
else
  warn "docker is not usable from this shell yet — a fresh install adds you to the
       docker group, and that takes a new login. Log out and back in, then check
       by hand with the command below."
fi

step "Done"
if [ "$_seen" = yes ]; then
  say "The agent answered from the patched runner: \"[mwe] memory is on for this"
  say "agent\" is in its container log, so the turn was stored and recalled."
else
  say "No turn seen yet. Send the bot a message, then check:"
  say "  docker logs \"\$(docker ps -q --filter label=nanoclaw-session | head -1)\" 2>&1 | grep '\\[mwe\\]'"
  say "\"[mwe] memory is on for this agent\" is the runner saying it read the"
  say "patched code. No [mwe] line at all means an older container is still"
  say "serving that chat: pnpm exec tsx $MWE_SKILL_DIR/restart-mwe-groups.ts"
fi

say ""
say "Where things are:"
say "  the fork      $NANOCLAW_DIR"
say "  who is who    $NANOCLAW_DIR/mwe.json — senderMap, one line per person"
say "  delegations   $MWE_ORIGIN/dashboard/tokens"
say ""
if [ -n "${MWE_CLAIM:-}" ]; then
  say "One thing is worth doing now: the token this installer redeemed speaks for"
  say "everyone enrolled in this memory, plus guest. Open the Tokens page and"
  say "untick whoever the assistant has no business speaking for."
elif env_has MWE_TOKEN; then
  say "The consumer token in .env is the one this assistant speaks with. Who it"
  say "may speak for is the delegation roster on the Tokens page: every person"
  say "it answers, plus guest for anybody it does not recognise."
else
  say "One step is still yours, and it is the token. Issue a standard consumer"
  say "token for \"$MWE_BOT_CONSUMER\" on $MWE_ORIGIN/dashboard/tokens, tick every"
  say "person it speaks for plus guest, put it in $MWE_ENV as MWE_TOKEN, and"
  say "restart with: bash setup/lib/restart.sh. Until then the agent answers and"
  say "remembers nothing, and says so in the host log."
fi
say ""
say "Add somebody else: a line in senderMap (their <channel>:<platform id> to"
say "their memory user id), a tick in the delegations, and a restart of both"
say "halves. Anyone not listed speaks as a guest."
