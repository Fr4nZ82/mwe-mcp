# Health

**Nav: Health.** `/dashboard/admin/health`

Live diagnostics of the running server, read against the live database and the
live model handles. Reload the page to run them again.

**Engine database** — the number of application tables and migrations applied,
then two counters that should be zero on a healthy deployment (stale proposal
operations and stale REM operations awaiting recovery), and the size of the
token blacklist, which is simply how many tokens you have revoked.

**Workdir permissions** — every path in the workdir that another account on
this machine can read. This is not cosmetic: the memory on disk is cleartext,
so a readable path means the per-reader access control can be bypassed by
reading the files directly. Each finding is listed with its severity and its
mode.

**LLM slots** — a spinner while the page dials every slot, then the table. It
arrives on its own a moment after the rest of the page, because a slot whose
backend is not answering takes a while to give up and the rest of the page is
not made to wait for it. This is where an unreachable provider or a wrong
endpoint shows up, and a slot with no model at all reads **NO MODEL**.

Dialling a slot means calling it, so on a read-only deployment this table is
not offered at all: the section says so in place of the spinner, and the
address behind it refuses. Everything else on the page is read from the
database and answers there as it does anywhere.

This page is the subset of `mwe-mcp doctor` that can be answered while the
server is running. When the server will not start at all, the offline CLI is
the one to use: it also checks the workdir lockfile, the token secret and a
signing self-test.

## What a machine reads: `GET /health`

The page above is for you. A watchdog, a reverse proxy or a container
orchestrator needs one line it can parse instead, and that is `/health`, at the
root of the server's address:

```
$ curl -i https://memory.example/health
HTTP/1.1 200 OK
content-type: application/json

{"status":"ok"}
```

`200` and `{"status":"ok"}` mean the server answered and its database is
reachable. When the database is not, the answer is `503` with
`{"status":"degraded","detail":"database unreachable"}`, which is the status
code a watchdog restarts a service on.

It answers those two things and nothing else, on purpose. It asks for no
credential, so anybody who can reach the server reads it, and that is why it
names no version, no file and no model slot. What *does* describe your
deployment is gated: this page, behind the admin login, and the `/metrics`
scrape, behind an admin token — see [Watching it from
outside](observability.md).

Nothing about the answer is cached, so it is always about the server as it is
now, and the check itself is one small database query: polling it every few
seconds costs nothing.
