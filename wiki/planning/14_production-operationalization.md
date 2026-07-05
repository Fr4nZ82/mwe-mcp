---
title: Production operationalization
status: planned
---

# 14. Production operationalization — cross-platform forms

The Linux production story is complete: dev/prod split, the dedicated-user startup gate with
interactive systemd provisioning (`network-online` ordering included), the `mwe-mcp.service`
unit, the optional `mwe-mcp-tray` KDE tray, REM headroom (resolved by LLM-slot config), and the
runtime housekeeping sweeps (boot + post-wiki-delete). Current state:
[runtime-topology.md §10](../architecture/runtime-topology.md#10-the-trust-boundary-is-the-host-not-the-protocol)
· [build-run.md](../development/build-run.md)
· [web-agent-oauth.md §housekeeping](../design-notes/web-agent-oauth.md#housekeeping).
Headless-first is the rule: the daemon runs unchanged on a remote server with no desktop; the
tray exists only where a desktop session does. This group should land before the public-release
items it underpins — **5b** (Docker is another "no dedicated user / bypass" host) and **5h**
(the dedicated-user gate is the strongest single hardening for the co-location threat).

## Remaining work

- [ ] 14e — **Cross-platform tray for macOS and Windows — required for v1.0.** `ksni` is Linux-only;
  a cross-platform path (e.g. `tray-icon`, which pulls GTK on Linux) is reviewed when this lands.
- [ ] 14f — **Windows/macOS production equivalents** of the dedicated-user gate, the dev/prod split,
  and service supervision (launchd on macOS, a Windows service). **Prod only** — dev stays Linux.

## Open decision

- **Cross-platform tray library (14e).** `ksni` (Linux SNI) vs a cross-platform crate that drags a
  GUI toolkit onto Linux. Revisit when 14e is scheduled.
