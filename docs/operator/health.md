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

**LLM slots** — press **Load LLM slot diagnostics** and every slot is probed
for reachability. This is where an unreachable provider or a wrong endpoint
shows up.

This page is the subset of `mwe-mcp doctor` that can be answered while the
server is running. When the server will not start at all, the offline CLI is
the one to use: it also checks the workdir lockfile, the token secret and a
signing self-test.
