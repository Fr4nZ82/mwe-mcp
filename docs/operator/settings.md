# Server settings

**Nav: Settings.** `/dashboard/settings/me`

Everybody's account lives on this page — password and two-factor, see
[Your account](../user/your-account.md). An admin gets seven more sections
below them, and they are the deployment's own.

## Require two-factor for everyone

With it on, every person must set up two-factor authentication before they can
use the dashboard, enforced at their next sign-in. Consumer identities have no
password and are exempt. Turn it on **before** exposing the dashboard to
anybody outside the room.

## Admin reveal

A supervision lens. With it on, the whole dashboard stops projecting per-reader
access control: wiki pages show every fragment (highlighted), the facts table
lists everybody's facts so the per-fact actions reach them, and the recall
journal widens from your own recalls to everybody's. It never affects what a
consumer receives over MCP, and it is honoured for admins alone. Leave it off
unless you are actively supervising.

It can also be taken away from you: `instance.admin_reveal_locked` in
`mwe-mcp.config.yaml` turns reveal off for good, and no page here can undo it —
only somebody with access to the machine can. That is the setting for a
deployment where administering the panel and owning the memory are not the same
person.

## Email (SMTP)

The outgoing mail server that sends password-recovery links. With it off — the
default — the sign-in page hides *Forgot your password?* and the recovery route
does nothing.

Set **SMTP host**, **SMTP port** (587 STARTTLS · 465 implicit TLS · 25
plaintext), **TLS mode**, **From address** and optionally a display name and a
username. The password is never stored here: name the environment variable
holding it (`MWE_SMTP_PASSWORD` by default) and set that in `mwe-mcp.env`.

**Send a test email** uses the saved settings — save first. It is how a wrong
host, a wrong TLS mode or a missing password surfaces, because the recovery
path itself is silent by design.

## Public address of this server

The address people reach this server at from outside. **Three things are built
on it and none of them works without it**: the password-reset link, the
invitation email, and the link a consumer mints so somebody can open their own
memory. The server never guesses it from the request, because an address taken
from the browser's own header is one whoever sent the request chose, and these
links carry credentials.

It must start with `https://`; plain `http://` is accepted only for a loopback
host such as `http://127.0.0.1:8742`. Left blank, the two emails are not sent
at all — the dashboard still shows you the link to hand over — and a consumer's
link comes back as a path for it to complete. Applies immediately.

## Ingest timezone

The deployment-wide default zone. Times people speak — *"tomorrow at 9"* — are
stamped in it; unset, they are read as UTC. A person's own timezone, set on
their account or by the welcome wizard, always wins over this. Applies from the
next turn.

## Dream cadence

When the two scheduled runs fire: **Full REM** (interval and initial delay) and
**Light** (interval, initial delay, and a backlog trigger that fires a Light
run early once that many captures are waiting). One **Mode** switch turns both
off. What the runs do is [the Dream console](dream.md); what they may change is
[REM settings](rem-settings.md).

This section is read once at boot, so it applies at the next server restart —
there is a **Restart now** button on [Backup](backup.md).

## Logging

**Level** (`info` — boundary events, or `debug` — plus internal step detail),
**File rotation** (daily, hourly, never, or disabled for stderr only) and the
**File path**, relative to the workdir. `debug` is for diagnosis, not steady
state. Read once at boot, like the cadence above.

## Document pipeline

The resource knobs for ingesting a whole document: how often the worker checks
the queue, the segment sizes, the caps on segments and facts per segment, the
sample the classifier reads, the maximum document size, and the similarity
above which two extracted facts merge. Empty keeps the built-in default. Read
once at boot.
