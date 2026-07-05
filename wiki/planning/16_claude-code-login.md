---
title: Claude Code subscription login — dashboard OAuth (test/personal tooling)
status: in-progress
---

# 16. Claude Code subscription login

Test/personal-only tooling that lets the **operator** sign `anthropic` LLM
slots with their *own* Claude subscription instead of a Console API key — so
local dogfooding does not incur a second pay-per-token bill. **Never a
production auth mode**: a deployed product brings its own Console keys (the
buyer's keys, not the maintainer's subscription). Same stance as the
[config schema](../protocol/config-schema.md#anthropic-claude-code--oauth-auth)
records.

## What already ships (current state — see the per-area page)

The engine (`mwe_core::oauth` — PKCE, code↔token exchange, transparent refresh,
the `0600` workdir store, the `api_key_env: claude-code` sentinel), the
`AnthropicBackend` OAuth path (Bearer + Claude Code fingerprint headers + the
mandatory system block), the startup store install + boot-health leniency for a
not-yet-logged-in slot (the chicken-and-egg), and the dashboard **"Log in with
Claude Code"** panel (out-of-band paste **active**; the seamless loopback channel
**dormant**, see 16a) are **built and documented as current state** in
[config-schema.md](../protocol/config-schema.md#anthropic-claude-code--oauth-auth).
This group tracks only the residue.

## Remaining work

- [ ] 16a — **Wake the dormant seamless loopback channel.** **Confirmed
  2026-06-23:** Anthropic's OAuth client **rejects** the custom loopback
  `redirect_uri` `http://<loopback>/dashboard/admin/claude-login/callback`
  (*"Redirect URI … is not supported by client"*). The dashboard now defaults to
  the **out-of-band paste** channel — the button posts `manual=1` and the
  seamless code stays intact but unreached. To revive seamless, find a
  `redirect_uri` the client accepts (likely the CLI's own
  `http://localhost:<port>/callback` — root path, `localhost` host) and verify
  against the live endpoint.
- [ ] 16b — **Refresh-failure re-login UX.** When the refresh token is revoked
  or expires, slots start failing at *runtime* with an auth error rather than a
  "log in again" prompt. Decide whether the dashboard panel should surface a
  stale/expired login distinctly and nudge re-login.
- [ ] 16c — **Authenticated route integration test.** The unit tests cover
  loopback detection + the oauth exchange/store; an admin-session test that
  drives `/admin/claude-login/start` (seamless 302 vs manual page) and the
  callback/paste completion would lock the wiring. (Needs the dashboard's
  authenticated-request harness.)

## Open decisions

- **Keep it gated to test/personal, or expose a supported "bring your Claude
  subscription" mode?** Current answer: **gated** — the product sells software,
  not the maintainer's tokens; buyers wire their own keys. Revisit only if a
  real consumer asks for subscription auth as a product feature (it also does
  not fit the remote-host topology, since the token lives on the server host).
