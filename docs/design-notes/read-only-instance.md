---
title: Read-only instance — showing a deployment without letting it be operated
area: design-notes
status: implemented
last_review: "2026-07-28"
---

# Read-only instance — `instance.read_only`

An instance you show to people who are not you needs a posture the
product did not have: **everything readable, nothing changeable**, and a
front door a stranger will actually walk through. This page describes
that posture — what it refuses, what it deliberately does not refuse,
why the second list is not a compromise, and
[the passwordless entrance](#the-demo-entrance) that makes the frozen
instance worth showing.

Two keys
([config-schema.md § `instance`](../protocol/config-schema.md#instance)):

```yaml
instance:
  read_only: true
  demo_identities: [bob, alice, zoe]   # optional; requires read_only
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
control that works, so the dashboard renders what it refuses and then
takes the handle off it. That is a **second** job, not the same one: on
its own it would be a curtain, since every route would still be routed.
The order matters and the tests follow it — `read_only.rs` asserts the
refusals **by path** before it ever looks at any HTML.

Three mechanisms, in decreasing order of strength:

- **Mounted, and inert.** The consoles that exist only to change things —
  users, groups, tokens, prompts, the LLM / embedding / recall / REM /
  spool / email / server / backup editors, the Dream console, the profile
  wizard — *are* merged into the router
  ([`routes::build`](../../crates/mwe-dashboard/src/routes/mod.rs)) on
  every deployment, frozen or not, and the top nav links them.
  A memory server is an operator's tool as much as a reader's, and an
  instance that hid them would be showing the half of the product that
  answers "what is this thing" least well.
  Their controls are then rendered and disabled by
  [`read-only.js`](../../crates/mwe-dashboard/assets/read-only.js), which
  is handed the server's own `ALLOWED_WRITES` (`read_only::live_writes_js`)
  rather than a copy of it, so what stays clickable is exactly what the
  guard still accepts. **This is chrome, not the boundary**: re-enabling a
  control from a browser console gets you a `403` from the guard, which
  is the intended order — shut the door, then take the handle off the
  inside.
- **Frame.** [`layout::Chrome`](../../crates/mwe-dashboard/src/ui/layout.rs)
  carries the deployment posture into the page shell, next to but
  distinct from `SessionUser`: the session answers *who is looking*, the
  chrome answers *what kind of instance they are looking at*. On a frozen
  deployment the shell drops the chat panel (it captures memory on every
  turn), its reopen FAB, the Help overlay (which is about operating the
  memory through that chat), the in-flight badge and the dream indicator,
  and adds a standing read-only notice.
- **Per-control.** A few read surfaces drop their own write affordances
  outright rather than showing them greyed, because they sit inline in a
  page somebody is *reading* and a disabled box there is noise: the wiki
  page's comment and describe controls, the wiki list's delete column,
  the facts table's edit/delete cell and the fact record's action forms,
  the smart wiki's sharing form and op-log revert cell. The Settings page
  is the counter-example and the deliberate one — it renders every
  section it always had, inert, because it is the page an operator would
  go to first to understand what the product manages.

Where a control's absence would otherwise read as a bug, `read_only::notice()`
puts one line in its place. Where the control sat among others, it is
simply gone — a sentence per missing button is worse than the missing
buttons.

One deliberate rewording: a wiki page normally explains a missing comment
box with "you don't have write access to it". On a frozen deployment
nobody has write access, so the page says *that* instead. A wrong
explanation of a correct refusal is still a wrong explanation.

## The demo entrance

A frozen instance is safe to show. It is not yet *showable*: a memory
product is only legible when you read the same page through two people's
eyes, and asking a stranger to type an email and a password before they
may see that loses most of them at the door — asking them to do it
twice, to compare, loses the rest.

So `instance.demo_identities` turns the sign-in screen into a row of
buttons — *Enter as Bob · Enter as Alice · Enter as Zoe* — and puts the
same row, compact, in the **panel frame**, on every page. That second
placement is the load-bearing one: the comparison is opening one page
and changing whose eyes you are using without leaving it. A switcher
that lived only on the sign-in screen would make the visitor navigate
back each time, and the demonstration would die of friction. The switch
returns to the page it was made from (`Referer`, reduced to a local
`/dashboard/` path), and the button for whoever is already signed in is
not rendered.

The one path that rule must **not** honour is the sign-in screen itself.
The buttons on the door post the same form as the switcher in the frame,
so they arrive with `/dashboard/login` as their `Referer` — and a visitor
who clicks *Enter as Bob* and is sent back to the door sees the same
three buttons and nothing that says they are now signed in, so they click
again. `destination` is the two-step rule: `safe_local` answers whether
the browser may be sent there at all, and the `SIGN_IN` filter answers
whether there is anything to see when it arrives. Everything else — a
missing header, a foreign origin, a non-dashboard path — lands on the
panel as before.

Implementation: [`routes/demo.rs`](../../crates/mwe-dashboard/src/routes/demo.rs).

**It exists only under the demo configuration**, and that is enforced in
two places rather than one:

- The config **refuses to load** when `demo_identities` is non-empty and
  `read_only` is false (`ConfigError::DemoIdentitiesNeedReadOnly`). A
  passwordless door on a writable deployment must not be reachable by
  any combination of settings, and a misconfiguration that quietly
  disabled itself would be worse than one that stops the server — the
  operator would believe the demo works and find out from a visitor.
- The router **does not mount** `demo::router()` unless both halves
  hold, so the path is not a route that refuses; it is a route that does
  not exist. The read-only guard has the one special case that keeps
  that true (`read_only::DEMO_ENTER`): without it, `/demo/enter` on a
  frozen instance with no demo cast would answer `303` where an unknown
  path answers `403`, and the difference is exactly the tell that "the
  route is mounted and merely refusing".

A demo session is the smallest thing that works: the id must be on the
configured list (the form field is checked against it, never trusted)
and must exist in `enrollment_users` (so a config typo mints nothing).
The session carries **that person's own role**, admin included.
Everything downstream — ACL projection, recall, redaction — then behaves
exactly as it does for that person on any deployment. The visitor is not
shown a mock-up of Bob; they are shown Bob.

The entrance once downgraded every session to non-admin, reasoning that
a door with no password should not hand out the panel. That contradicted
the sentence above — an admin shown as a non-admin *is* a mock-up — and
it hid most of the product. What makes the door safe is the freeze, not
the role: on a frozen instance nothing an admin can do changes anything,
and the guard refuses by path whatever the session says. **The role
decides what you may see; the freeze decides what you may change, and
only the second is load-bearing for safety.**

The consequence for whoever puts such an instance on the public
internet: every operator console is then readable by anybody who clicks
a button, so it is worth walking those pages and looking at what they
print. Host paths, private endpoints and real addresses are properties
of the deployment's own configuration, not of this mode.

The operator still needs a way in, so the password form survives behind
a `Sign in with a password` disclosure. Folded away rather than beside
the buttons: offering a stranger two ways in makes them choose instead
of click.

### The screen itself

The sign-in screen is the first thing a stranger sees of the product, so
its layout is part of the feature and was checked in a browser, not
inferred from the markup:

- it uses the centred **hero** shell (`layout::anonymous_hero_page`),
  not the 30 rem single-form column — at that width the third button
  wrapped onto a line of its own and read as a mistake;
- the centring is on `<body>` so the shell's `<h1>` inherits it; a title
  flush left above a centred row of buttons reads as two pages stacked;
- below the `sm` breakpoint the buttons stack full width instead of
  wrapping 2 + 1;
- and the "You are &lt;id&gt;" badge, normally hidden on mobile to keep
  the top bar to one row, stays visible on a demo instance: whose eyes
  you are using is the single most important thing on the screen, and
  the switcher beside it is useless without it.

## Tests

- **Dashboard** — `crates/mwe-dashboard/tests/read_only.rs`, one test per
  property. `a_frozen_instance_refuses_memory_and_configuration_writes`
  walks the write surface by path, including the public half reached with
  no cookie at all.
  `the_same_writes_are_not_forbidden_when_the_instance_is_open` is the
  baseline that keeps it honest — a `403` from the guard is not the same
  thing as a `404` or a validation bounce.
  `identity_still_works_on_a_frozen_instance` signs out and back in.
  `a_frozen_instance_shows_every_console_and_arms_none_of_them` is the
  inverted one: the consoles must answer `200` and be linked in the nav,
  the frame must ship `read-only.js` with the server's live-write list,
  and the same paths must still answer `403` to a `POST`. Both halves in
  one test on purpose — a change that satisfies either by breaking the
  other fails here.
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
- **Demo entrance** — `crates/mwe-dashboard/tests/demo_entrance.rs`. The
  gate is asserted as a **comparison**, not a status code:
  `without_the_demo_configuration_the_entrance_route_does_not_exist`
  walks the three ways to be "almost" a demo and requires
  `POST /demo/enter` to be answered *exactly* as
  `POST /no-such-route-was-ever-defined`. No fixed code would do — this
  router bounces an unmatched request to the sign-in page rather than
  answering `404`, and a frozen deployment refuses an unknown write with
  `403` before routing gets a say, so the expected code differs per
  posture while the property does not. The rest pin the entrance
  (`a_visitor_enters_with_one_click_and_no_credentials`, which posts the
  `Referer` a browser really sends and asserts the landing — passing
  `None` there is what let the door-to-door redirect ship), the frame
  switcher and its return path
  (`the_switcher_is_on_every_page_and_returns_to_the_same_page`), the
  destination rule itself (`demo.rs`'s
  `entering_from_the_door_lands_in_the_panel_and_not_back_on_the_door`),
  `a_demo_session_carries_the_role_of_the_person_it_signs_in_as` (both
  directions: the admin identity gets the admin nav, the ordinary one
  does not), `a_frozen_instance_shows_the_operator_consoles_and_still_refuses_them`,
  the off-list refusal, and the configured-but-absent
  typo. `config.rs` pins the load-time refusal
  (`demo_identities_without_read_only_refuse_to_load`).

## See also

- [config-schema.md § `instance`](../protocol/config-schema.md#instance)
  — the section, and the sibling `admin_reveal_locked` switch.
- [redaction-policy.md § The machine operator can lock reveal](redaction-policy.md#the-machine-operator-can-lock-reveal)
  — the other half of the same idea: the panel admin is not the machine
  operator.
- [dashboard.md](dashboard.md) — the surfaces this mode reshapes.
