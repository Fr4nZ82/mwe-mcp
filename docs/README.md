# The mwe-mcp guide

This is the guide for **people**: the operator who installs and runs the
server, and everybody whose memory it holds.

## Where to read it

Here, and in the dashboard. The server carries these pages inside its binary
and serves them under **Guide** in the top bar, so what you read there is this
folder as your server was built from it. The operator's half needs the admin
account there; everything else opens for anybody signed in.

## The rule that keeps it true

**Before every release, every page here is opened again against the running
dashboard, and re-checked screen by screen. A page nobody can verify is
deleted, not kept.**

A page that has stopped being true is worse than no page, because it has the
air of knowing something. So this guide describes only what a person **sees
and does**: the screens, their labels, the buttons, and what happens after you
press one. It never explains how the engine works inside — that answer comes
from the code, and repeating it here would be a second description to keep in
step with the first.

Where a number appears (a token's lifetime, how long something is kept, when
the budget warns), it is the number the server actually uses, taken from its
configuration and stated once, here, next to the screen that shows it.

## Who this is for

**The operator** installs the binary, runs the server and administers the
dashboard. Everything under [`operator/`](operator/) is theirs, and most of it
needs the admin account: there is exactly one admin per deployment.

**The people whose memory it is** talk to an assistant and open the dashboard
to read and correct their own memory. Everything under [`user/`](user/) is
theirs, and needs no admin rights.

Throughout, a **consumer** is the bot or assistant that talks to this memory —
the ready-made assistant, a chat bot, a coding agent. It is the dashboard's own
word for it, and it is the word used here.

## The operator's pages

Start here, in this order:

1. [First start](operator/first-start.md) — create the admin account and see
   what the dashboard asks for next.
2. [The six model slots](operator/model-slots.md) — the memory does not work
   until all six have a model.
3. [The embedder](operator/embedder.md) — what backs recall and dedup.
4. [Server settings](operator/settings.md) — the public address, mail, the
   time zone, the two scheduled runs, logging.

Then the people and the programs that talk to the memory:

- [Users](operator/users.md) — add a person, invite them, edit their account.
- [Groups](operator/groups.md) — who shares what with whom.
- [Tokens](operator/tokens.md) — issue a consumer's credential, say who it may
  speak for, revoke it.
- [Bridges](operator/bridges.md) — wire a consumer to this memory, starting
  with the ready-made assistant.
- [Wikis, from the admin's side](operator/wikis.md) — the raw editor, exporting
  a wiki, deleting one, and the consoles of a consumer's own wiki.
- [Skills](operator/skills.md) — what a consumer is taught, served by this
  server.

Then running it day to day:

- [Usage and spend](operator/usage-and-spend.md) — what the models cost, and
  the daily budget that stops them.
- [Backup and what is kept](operator/backup.md) — snapshots, restoring one,
  wiping the memory, and how long each thing is retained.
- [The Dream console](operator/dream.md) — run a cycle by hand and read the
  history.
- [REM settings](operator/rem-settings.md) — how much one nightly cycle may
  change.
- [Recall settings](operator/recall-settings.md) — how much memory each turn
  is allowed to pull.
- [Prompts](operator/prompts.md) — override the operational prompts.
- [The training spool](operator/training-spool.md) — record the model
  exchanges.
- [Health](operator/health.md) — live diagnostics of the running server, and
  the open one-line answer to "is it up?".
- [Watching it from outside](operator/observability.md) — the numbers your own
  monitoring reads, and what each of them means.
- [Export and forget a person](operator/export-and-forget.md) — the two things
  somebody can ask you for.
- [Before you expose it](operator/security.md) — the checklist for a
  deployment reachable from outside the machine.

## The pages for whoever the memory is about

- [What this memory is](user/what-this-is.md) — what it remembers, and what it
  does not.
- [Your first sign-in](user/first-sign-in.md) — the invitation link and the
  three-step welcome.
- [Your home page](user/your-home.md) — what each link leads to.
- [Your facts](user/your-facts.md) — read them, correct them, close them,
  forget them.
- [Who can read what](user/who-can-read-what.md) — the three questions every
  fact answers.
- [Wikis and pages](user/wikis-and-pages.md) — browsing the memory as text.
- [Search](user/search.md) — finding the pages a word appears on.
- [Comments](user/comments.md) — the way to change a page.
- [The chat](user/the-chat.md) — asking the memory to do something.
- [What was recalled for you](user/traces.md) — the record of every answer.
- [Reminders and notices](user/reminders-and-notices.md) — what reaches you
  through your assistant.
- [Your account](user/your-account.md) — password, two-factor, signing out
  everywhere.
- [A copy of your memory, or its removal](user/copy-and-removal.md).

## The documents that are not this guide

- [`INSTALL.md`](../INSTALL.md) — getting the binary, running it as a
  service, deployment topologies, the hardening checklist.
- [`INTEGRATING.md`](../INTEGRATING.md) — wiring your own consumer: the
  per-turn contract, tokens, transports.
- [`AGENT_INSTRUCTIONS.md`](../AGENT_INSTRUCTIONS.md) — what an *agent*
  reads to connect itself.
- [`CHANGELOG.md`](../CHANGELOG.md) — what shipped, release by release.
