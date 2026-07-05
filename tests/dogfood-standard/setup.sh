#!/usr/bin/env bash
# Bootstrap the standard-consumer dogfood fixture on a fresh mwe-mcp workdir,
# using the **diagonal identity model** (post-2026-05-31): a *standard* consumer
# is a credential-less **system-user "bot"** that speaks for humans via the
# `X-MWE-Act-As` header — NOT a per-human token (the old model this script used).
# The token-issue endpoint enforces it (`enrollment::validate_token_identity`):
# a standard token bound to a human-with-login is refused.
#
# Fictional cast (the Baggins
# household): admin frodo, humans galadriel/gollum/bilbo, groups famiglia+amici,
# plus the consumer bot `samvise`.
#
# This script runs the first-run wizard (creates the admin), then creates the
# other humans, the bot system-user, the two groups, and issues ONE standard
# token bound to the bot with the humans as allowed act-as senders. The admin
# cookie jar is left in tokens/cookies.txt for driving dreams over HTTP.
#
# Drive the surface AS A HUMAN:
#   MWE_JWT_FILE=tokens/samvise.jwt MWE_ACT_AS=frodo \
#     python3 mcp_client.py wiki_ingest_message '{"text":"...","context_hint":"conversation"}'
#
# Override any of these via env. See instruction.md for the full runbook.
set -uo pipefail
BASE="${MWE_DASH_URL:-http://127.0.0.1:8742/dashboard}"
ADMIN_ID="${ADMIN_ID:-frodo}"               # first-run admin
ADMIN_EMAIL="${ADMIN_EMAIL:-frodo@shire.test}"
ADMIN_PASS="${ADMIN_PASS:-!Brandivino84}"   # shared test password for ALL accounts; internet-compromised but the local test net is closed & self-managed, so reuse is fine (one less to remember); committed on purpose
BOT_ID="${BOT_ID:-samvise}"                 # the standard consumer = a credential-less system user
CONSUMER_ID="${CONSUMER_ID:-samvise-prod}"  # consumer_id stamped on the token + delegation row

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUT="$HERE/tokens"; CJ="$OUT/cookies.txt"; mkdir -p "$OUT"
JWT_RE='eyJ[A-Za-z0-9_-]\+\.eyJ[A-Za-z0-9_-]\+\.[A-Za-z0-9_-]\+'

# --- admin: first-run wizard (no-op once an admin exists), then login -------
echo "==> first-run wizard + login ($ADMIN_ID / $ADMIN_EMAIL)"
curl -s -c "$CJ" -o /dev/null \
  --data-urlencode "email=$ADMIN_EMAIL" --data-urlencode "admin_id=$ADMIN_ID" \
  --data-urlencode "password=$ADMIN_PASS" --data-urlencode "password_confirm=$ADMIN_PASS" \
  "$BASE/setup"
# (Re-)login so we hold a fresh admin session cookie whether or not the wizard
# just ran — the session secret lives in ./work/mwe-mcp.env and survives restarts.
curl -s -c "$CJ" -o /dev/null \
  --data-urlencode "email=$ADMIN_EMAIL" --data-urlencode "password=$ADMIN_PASS" "$BASE/login"
grep -q mwe_session "$CJ" || { echo "!! no admin session (wizard+login failed) — is the server up at $BASE?" >&2; exit 1; }
echo "    admin session ok"

# --- human users (admin creates them; each gets an invite, never a token) ---
newuser() { curl -s -b "$CJ" -o /dev/null -w "    user $1 -> HTTP %{http_code}\n" \
  --data-urlencode "user_id=$1" --data-urlencode "aliases=$2" --data-urlencode "profile=$3" "$BASE/users/new"; }
echo "==> create human users"
newuser galadriel "Morgana, Nina"     "Compagna di Frodo, madre di Gollum."
newuser gollum    "Matteo, Smeagol"    "Figlio di Frodo e Galadriel (2017), bambino."
newuser bilbo     "Bruno, nonno Bruno" "Padre di Frodo, nonno di Gollum; amico di famiglia."

# --- the consumer bot: a credential-less SYSTEM USER (diagonal model) -------
# Created like any user but never given a credential → enrollment::is_system_user
# is true → a standard token may bind to it.
echo "==> create system-user bot '$BOT_ID'"
curl -s -b "$CJ" -o /dev/null -w "    user $BOT_ID -> HTTP %{http_code}\n" \
  --data-urlencode "user_id=$BOT_ID" --data-urlencode "aliases=" \
  --data-urlencode "profile=Consumer bot (Samvise 2.0), standard-conversational." "$BASE/users/new"

# --- groups (membership is resolved server-side from enrollment_groups) -----
FAM_SCOPE="Spesa e lista della spesa; regole della casa (orari, turni); piani condivisi (vacanze, cene, uscite, figli); robe di casa e oggetti nostri; episodi condivisi e dei figli; scuola e compiti dei figli; presenza e impegni (chi e in casa o fuori); tutto cio che e utile che l altro membro della famiglia sappia. NON includere: password personali, fatti personali irrilevanti per gli altri (es. ho corretto un bug, ho finito un libro)."
echo "==> create groups"
curl -s -b "$CJ" -o /dev/null -w "    famiglia -> HTTP %{http_code}\n" \
  --data-urlencode "group_id=famiglia" --data-urlencode "description=Nucleo familiare Baggins" \
  --data-urlencode "members=frodo" --data-urlencode "members=galadriel" --data-urlencode "members=gollum" \
  --data-urlencode "scope=$FAM_SCOPE" "$BASE/groups/new"
curl -s -b "$CJ" -o /dev/null -w "    amici -> HTTP %{http_code}\n" \
  --data-urlencode "group_id=amici" --data-urlencode "description=Amici di famiglia, accesso base" \
  --data-urlencode "members=frodo" --data-urlencode "members=bilbo" \
  --data-urlencode "scope=Informazioni di amici e condivise con amici." "$BASE/groups/new"

# --- one STANDARD consumer token, bound to the bot, act-as the humans -------
# consumer_token=on + consumer_id + allowed_sender_ids[] => the diagonal
# "Pattern B": the bot holds the token, X-MWE-Act-As picks the human per call.
echo "==> issue standard consumer token for '$BOT_ID' (act-as: frodo galadriel gollum bilbo)"
curl -s -b "$CJ" -o "$OUT/_i.html" -w "    issue -> HTTP %{http_code}\n" \
  --data-urlencode "sender_id=$BOT_ID" --data-urlencode "device_label=$CONSUMER_ID" \
  --data-urlencode "ttl_profile=internal" --data-urlencode "consumer_token=on" \
  --data-urlencode "consumer_id=$CONSUMER_ID" \
  --data-urlencode "allowed_sender_ids=frodo" --data-urlencode "allowed_sender_ids=galadriel" \
  --data-urlencode "allowed_sender_ids=gollum" --data-urlencode "allowed_sender_ids=bilbo" \
  "$BASE/tokens/issue"
grep -o "$JWT_RE" "$OUT/_i.html" | head -1 > "$OUT/$BOT_ID.jwt"; rm -f "$OUT/_i.html"
[ -s "$OUT/$BOT_ID.jwt" ] && echo "    -> $OUT/$BOT_ID.jwt ($(wc -c <"$OUT/$BOT_ID.jwt") bytes)" \
  || { echo "    !! no JWT extracted — check the diagonal-model refusal (is $BOT_ID credential-less?)" >&2; exit 1; }

echo
echo "Done. One bot token in $OUT/$BOT_ID.jwt; admin cookie in $CJ."
echo "Drive the surface AS A HUMAN (the bot acts-as):"
echo "  MWE_JWT_FILE=$OUT/$BOT_ID.jwt MWE_ACT_AS=frodo python3 $HERE/mcp_client.py wiki_ingest_message '{\"text\":\"...\",\"context_hint\":\"conversation\"}'"
echo "Run a dream over HTTP (admin cookie):"
echo "  curl -b $CJ -X POST $BASE/dream/light     # promote captures + compile dirty pages"
echo "  curl -b $CJ -X POST $BASE/dream/full      # reorg + apply parked comments (G.8) + compile"
