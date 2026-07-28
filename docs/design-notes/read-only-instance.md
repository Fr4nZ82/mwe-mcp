---
title: Read-only instance — showing a deployment without letting it be operated
area: design-notes
status: implemented
last_review: "2026-07-28"
---

# Read-only instance — `instance.read_only`

An instance you show to people who are not you needs a posture the
product did not have: **everything readable, nothing changeable**. This
page describes that posture — what it refuses, what it deliberately does
not refuse, and why the second list is not a compromise.

Turned on by one key
([config-schema.md § `instance`](../protocol/config-schema.md#instance)):

```yaml
instance:
  read_only: true
```

Like the rest of the `instance:` section it has **no dashboard editor**.
It is the machine operator's switch, so a panel admin can neither set it
nor lift it, and it is read once at boot rather than hot-swapped.

## It is not "refuse POST"

The obvious implementation — reject every mutating HTTP request — takes
the demonstration with it. Half of what makes the memory model legible is
opening the *same page* as one person and then as another: the fragments
that were `[redacted]` fill in, the ones that were visible go away.
Changing identity means signing out and signing in, and both write
session state.

So the line is not the HTTP verb, it is the **substance**:

| Refused | Kept |
|---|---|
| Memory: facts, wiki pages, inline comments, briefing items, proposals, dreams, media uploads, document ingest | Reading, searching, navigating, the recall trace of a read |
| Configuration: users, groups, tokens, prompts, every YAML editor, backups, staged recovery, restart | Identity: sign in, sign out, the 2FA challenge between them, the session keepalive |
| Credential issuing: `/setup`, invitations, password reset, the whole `webagentoauth` register/token surface | The per-browser admin-reveal cookie, which changes no server state |

## Three surfaces, three enforcement points

The instance is reachable three ways, so the refusal exists three times.
None of them is the "real" one with the others as decoration.

1. **The dashboard HTTP tree** —
   [`mwe_dashboard::read_only::guard`](../../crates/mwe-dashboard/src/read_only.rs),
   one middleware layered over the whole router (public **and**
   authenticated halves; the public half — `/setup`, `/accept-invite`,
   `/reset-password`, the OAuth consent — sits outside the session layer
   and would otherwise be the way in). It matches the request path
   against `ALLOWED_WRITES`, an allow-list, so a route added tomorrow in
   a module nobody remembers is refused by default. Two mutating `GET`s
   are handled by name: `/auth/link` passes (it redeems a magic link into
   a session — identity), `/admin/claude-login/callback` does not (it
   stores provider credentials).
2. **The MCP dispatcher** — one guard at the top of
   [`mcp::dispatch`](../../crates/mwe-mcp-server/src/mcp/mod.rs), the
   single choke point every tool call passes through, keyed off
   `READ_ONLY_TOOLS` (again an allow-list). It runs **before** the tool's
   own argument parsing, so a refused call never reaches its own parser.
   The wire class is `403 instance_read_only`
   ([tool-reference.md § errors](../protocol/tool-reference.md)) and it
   names the *instance*: no token, role or consumer class lifts it.
3. **`POST /media`** — the byte-upload endpoint sits behind the same
   bearer JWT as `/mcp` but not behind the dispatcher, so it carries the
   check itself
   ([`http_media.rs`](../../crates/mwe-mcp-server/src/http_media.rs)).

### And the loops that answer to nobody

A request-level refusal says nothing to a background scheduler. On a
frozen deployment the REM cycle, the light dream, the document worker and
the automatic backup **do not start** — a dream that recompiles pages
overnight would falsify the mode's name while nobody was watching. The
boot-time refresh of the operator's `wikis/index.md` collector is skipped
for the same reason: "nothing changes" has to include the bytes the
server would write itself.

The reindex watcher stays armed. It derives the index from files that no
longer change, so it is a no-op — and if one ever does change, the safety
net should still be honest.

## Two deliberate exceptions, and why they are not cheating

Two allow-listed tools do write a row:

- **`wiki_navigate`** appends to the recall-trace journal, which is
  capped at the last ten runs. It is also the tool that makes the
  navigator demonstrable at all.
- **`events_poll`** stamps `consumers.last_seen_at`, a heartbeat column.

Both write **telemetry about the reader**, not content. The test is "does
it change memory or configuration", not "does it touch the disk", and
saying so out loud is cheaper than a mode that quietly means something
narrower than its name.

`dashboard_link` is refused even though it writes nothing at all: it
mints a signed dashboard session. Handing out a credential that outlives
the call is not a write — it is what a write would need.

## Hide the handle, but shut the door first

A control that returns an error in front of a stranger is worse than a
control that is not there, so the dashboard hides what it refuses. That
is a **second** job, not the same one: hiding alone would be a curtain,
since every route would still be routed. The order matters and the tests
follow it — `read_only.rs` asserts the refusals **by path** before it
ever looks at any HTML.

Three mechanisms, in decreasing order of strength:

- **Not mounted.** The consoles that exist only to change things — users,
  groups, tokens, prompts, the LLM / embedding / recall / REM / spool /
  email / server / backup editors, the Dream console, the profile wizard
  — are not merged into the router
  ([`routes::build`](../../crates/mwe-dashboard/src/routes/mod.rs)). A
  page whose whole content is dead controls invites a visitor to try; a
  route that does not exist cannot be found by anybody. Keep this list in
  step with the top-nav admin block, which hides the same entries.
- **Frame.** [`layout::Chrome`](../../crates/mwe-dashboard/src/ui/layout.rs)
  carries the deployment posture into the page shell, next to but
  distinct from `SessionUser`: the session answers *who is looking*, the
  chrome answers *what kind of instance they are looking at*. On a frozen
  deployment the shell drops the chat panel (it captures memory on every
  turn), its reopen FAB, the Help overlay (which is about operating the
  memory through that chat), the in-flight badge and the dream indicator,
  and adds a standing read-only notice.
- **Per-control.** The read surfaces that stay mounted drop their own
  write affordances: the wiki page's comment and describe controls, the
  wiki list's delete column, the facts table's edit/delete cell and the
  fact record's action forms, the smart wiki's sharing form and op-log
  revert cell, the Settings password and 2FA forms.

Where a control's absence would otherwise read as a bug, `read_only::notice()`
puts one line in its place. Where the control sat among others, it is
simply gone — a sentence per missing button is worse than the missing
buttons.

One deliberate rewording: a wiki page normally explains a missing comment
box with "you don't have write access to it". On a frozen deployment
nobody has write access, so the page says *that* instead. A wrong
explanation of a correct refusal is still a wrong explanation.

## Tests

- **Dashboard** — `crates/mwe-dashboard/tests/read_only.rs`, one test per
  property. `a_frozen_instance_refuses_memory_and_configuration_writes`
  walks the write surface by path, including the public half reached with
  no cookie at all.
  `the_same_writes_are_not_forbidden_when_the_instance_is_open` is the
  baseline that keeps it honest — a `403` from the guard is not the same
  thing as a `404` or a validation bounce.
  `identity_still_works_on_a_frozen_instance` signs out and back in.
  `a_frozen_instance_hides_the_controls_it_refuses` checks the frame and
  then that the consoles are `404`, not merely unlinked.
  `an_open_instance_keeps_its_consoles_and_its_chat_panel` pins that the
  default install is untouched.
- **Unit** — `read_only.rs`'s own module tests pin the path predicate,
  including that an unknown route is refused by default and that the two
  mutating `GET`s are classified in opposite directions.
- **MCP** — `crates/mwe-mcp-server/tests/dispatcher.rs`.
  `read_only_classifies_every_advertised_tool` iterates
  `schemas::all_tools()` and asserts each one is either allow-listed or
  refused with `instance_read_only` — there is no third answer, so a tool
  added without being classified fails this test rather than quietly
  shipping a write. Plus the open-instance baseline, the reads that must
  still work, and `dashboard_link`'s refusal.

## See also

- [config-schema.md § `instance`](../protocol/config-schema.md#instance)
  — the section, and the sibling `admin_reveal_locked` switch.
- [redaction-policy.md § The machine operator can lock reveal](redaction-policy.md#the-machine-operator-can-lock-reveal)
  — the other half of the same idea: the panel admin is not the machine
  operator.
- [dashboard.md](dashboard.md) — the surfaces this mode reshapes.
