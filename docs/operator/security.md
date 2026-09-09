# Before you expose it

A deployment reachable only from the machine it runs on is a different thing
from one reachable from outside. This is the dashboard half of the move; the
machine half — TLS, the service account, the file permissions — is the
hardening checklist in [`INSTALL.md`](../../INSTALL.md).

**Set the public address.** [Settings](settings.md) → *Public address of this
server*, `https://`. Without it the password-reset link, the invitation email
and the link a consumer mints for somebody's own memory do not work.

**Put TLS in front, and mark the cookies.** Once the dashboard is reached
through a proxy or a tunnel, set `instance.cookie_secure: true` in
`mwe-mcp.config.yaml` so the browser only ever sends the session cookie over
HTTPS. It is off by default because the documented first run is plain
`http://127.0.0.1:8742`, where a secure cookie would never come back.

**Require two-factor for everyone.** [Settings](settings.md), one checkbox,
enforced at each person's next sign-in.

**Prefer the short token lifetime.** On [Tokens](tokens.md), *exposed — 30
days* is the profile for anything on the public internet; *internal — 1 year*
is for a device on your own machine. There is no self-service refresh, which is
the point: a lapsed token comes back through you.

**Know the call ceilings.** They apply whether or not you wrote a profile: 120
calls a minute and 3 000 an hour per token, of which 30 a minute and 600 an
hour may spend a model. Write a named profile in `mwe-mcp.config.yaml` and put
its name in a token's **Rate limit id** to give one consumer different numbers.

**Set a daily budget.** [Usage and spend](usage-and-spend.md). A budget is the
only thing that stops a runaway consumer from spending, and it warns you on the
way there.

**Check the workdir permissions.** [Health](health.md) lists every path another
account on the machine can read. The memory on disk is cleartext, so a readable
path bypasses everything this product enforces above it.

**Know the two addresses a machine reads.** `/health` answers anybody who can
reach the server, which is what makes it usable as a proxy or orchestrator
health check; it is one line and names nothing about your deployment.
`/metrics` is the opposite — it describes the deployment, so it takes an admin
token and is never open. Both are described in [Watching it from
outside](observability.md). If your proxy runs bot or browser-integrity
filters, exempt them along with `/mcp`: they are spoken by programs, and such a
filter refuses a non-browser client on sight.

**Decide who may read what.** If administering the panel and owning the memory
are not the same person, set `instance.admin_reveal_locked: true` — the admin
then cannot switch the reveal lens on, and no page can undo it.

**Know how long a sign-in lasts.** A dashboard session is 60 minutes, sliding:
every request you make pushes it out again, and the page pings quietly while
you are on a long form, so it does not lapse mid-fill. An invitation link is
good for 24 hours, a password-reset link for 30 minutes, and a password must be
at least 12 characters.

**Know how access ends.** For a person: *Log out everywhere* ends every session
they have open, on every device, and changing their password does the same —
both are theirs to press, on their own [account page](../user/your-account.md)
and in their own top bar. Removing them altogether is
[Forget](export-and-forget.md). For a consumer: revoke its token on
[Tokens](tokens.md), and the revocation takes effect within a minute.

## Showing the product without opening it

Two settings in `mwe-mcp.config.yaml`, deliberately with no dashboard editor —
a switch a panel admin can flip is not a switch that constrains a panel admin:

- `instance.read_only: true` freezes the deployment. Signing in, reading and
  navigating keep working; facts, pages, comments, proposals, dreams, users,
  tokens, prompts and every configuration editor refuse. The background runs
  that would rewrite memory on their own do not start. **No model is called,
  by anybody, for any reason**: the few pages that would have asked one
  something — the chat, the per-slot reachability probe on **Health**, the
  model picker on **LLM** — say so instead of asking, and the addresses
  behind them refuse even if a visitor types one by hand. A frozen
  deployment on the public internet cannot be made to spend.
- `instance.demo_identities: [alice, bob, carol]` puts a passwordless door on
  it, offering *Enter as…* for exactly those people. It **requires**
  `read_only`, and the server refuses to start otherwise. A session minted
  that way carries that person's own role, **admin included** — the visitor is
  shown that person, not a reduced stand-in. What makes the door safe is the
  freeze, not a smaller session.

Empty is the only value a normal installation ever has for the second one: with
no identities listed, the passwordless routes are not mounted at all.

⚠️ **List an admin and every operator console becomes readable by whoever
clicks the button.** The freeze refuses changes, not reading: Backup, Spool,
Health and Wikis print paths on this machine, LLM config prints the last four
characters of each API key and the Ollama endpoint in full, Prompts prints the
system prompts, the Dream journal prints a night's whole log, and Usage prints
what the deployment spends. Two links hand over more than a page —
**Export everything about this person** on a user and **Export subtree (.tar)**
on a wiki — and the only switch that closes them, along with *Reveal*, is
`instance.admin_reveal_locked: true`. Read those pages on the deployment you
are about to show before you show it.
