# Security policy

## Reporting a vulnerability

Please do **not** open a public issue for a security problem. Write to
**fr4nz82@gmail.com** with the subject `mwe-mcp security` and include:

- the version (`mwe-mcp --version`) or commit you tested;
- what an attacker needs (a dashboard login, an MCP token, network access to
  the port, a shell on the host);
- the steps to reproduce, and what the impact is (data another user can
  read, a write they can make, a way to take the server down).

You will get an acknowledgement within a few days. Fixes ship as a patch
release and are called out in `CHANGELOG.md`; you will be credited there
unless you prefer not to be.

## Scope

mwe-mcp is a self-hosted server. The threat model it defends against is
described in `INSTALL.md` (hardening checklist) and `INTEGRATING.md`
(deployment security): an enrolled user or a consumer holding a token who
tries to read or change memory beyond what the per-fact permissions grant,
and an unauthenticated party reaching the listener. Reports about the
Markdown being cleartext on the host's disk are out of scope: that is the
documented design, and the workdir permission rules exist for it.

## Supported versions

Only the latest release receives security fixes.
