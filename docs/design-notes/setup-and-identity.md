---
title: Setup wizard and identity model
area: design-notes
status: implemented
last_review: "2026-07-20"
---

# Setup wizard and identity model

This page documents how the dashboard bootstraps the first user and what
the "identity" of a person or group means on disk and in the database.
The identity/account model it builds on is the
[identity & ACL model](../concepts/identity-and-acl.md). Identity wikis
are materialized explicitly at user/group creation, not implicitly at
first message.

## The two binaries: account vs identity

In mwe-mcp an account and an identity are **separate things** that map
one-to-one but live on different axes:

| Concept | What it is | Where it lives |
|---|---|---|
| **Account** | The credential pair used to sign in. `email + password`. | `enrollment_users.email` (the login key) + `user_credentials` (Argon2id hash) |
| **Identity** | The canonical principal of dominio. `user_id` slug. | `enrollment_users` row + `<workdir>/wikis/<user_id>/` directory |

The email lives on `enrollment_users` — the row that exists from the
moment a user is **invited** — because that is when the admin sets it
(the "Add user" form), well before the `user_credentials` row is born at
accept-invite. The password hash lands on `user_credentials` at accept.

The slug appears in **every** marker on disk
(`{{owner=user:franz}}…{{/}}`), every `fact_index` row's `owner_id`
column, every JWT's `sender` claim, and as the directory name of the
user's personal memory wiki. Changing it would ripple through the
filesystem — so it's chosen once at creation time and never rewritten.

The email is what the operator types at the login form, what the
SMTP-based recovery flow targets (roadmap 28), and what an SSO
integration would key on. It is **admin-managed**: the admin sets it when inviting a
user and is the only one who can change it afterwards (the edit-user
page) — a user never edits their own login email. Changing it is cheap:
a single UPDATE on `enrollment_users.email`. The slug stays put.

## First-run sequence

```
operator opens https://localhost:8742/dashboard
        │
        ▼
GET /                      ─── redirect ───▶  /dashboard/login
GET /dashboard/login       ─── no admin ──▶  /dashboard/setup
GET /dashboard/setup       ─── render wizard form
                                 ▼
                          operator fills:
                          - email           (e.g. franz@example.com)
                          - user_id slug    (pre-filled from local part: "franz")
                          - password + confirm
                                 ▼
POST /dashboard/setup
                                 ▼
                          transaction (engine.db):
                          1. INSERT enrollment_users     (email, is_admin=1)
                          2. INSERT user_credentials     (Argon2id hash)
                          ── commit ──
                                 ▼
                          filesystem (post-commit):
                          3. create <workdir>/wikis/<user_id>/_meta.md
                          4. create <workdir>/wikis/<user_id>/index.md
                          5. create <workdir>/wikis/<user_id>/rules.md
                             (default user-policy page)
                                 ▼
                          session cookie minted
                          303 → /dashboard/home
                                 ▼
                          operator lands on the dashboard,
                          /dashboard/wiki already lists their wiki
```

The `_meta.md` written at step 3 has the canonical frontmatter:

```yaml
---
wiki_id: franz
wiki_type: wiki-user
parent_wiki_id: null
slug: franz
title: franz
scope: 'user:franz'
created: 2026-05-19T18:24:15.7Z
updated: 2026-05-19T18:24:15.7Z
---
```

The `title` defaults to the `user_id` because the setup form does not
yet ask for a display name. The operator can edit `_meta.md` (or use a
future dashboard form) to set a friendlier title. The
[`mwe_core::wiki::create_identity_wiki`](../../crates/mwe-core/src/wiki.rs)
helper is the canonical writer — it produces the same frontmatter
shape every caller relies on, for both user and group actor-wikis.

Step 5 seeds [`rules.md`](../../crates/mwe-core/src/wiki.rs) (`RULES_FILENAME`),
the user-facing **engine-policy page**: the user's
standing **governance** rules in natural language — privacy/sharing + do-not-store
(per-agent behaviour rules belong to the consumer's own wiki; the user's
**user-global** behaviour rules do live on this page, as `{{f=…}}` fact
regions the governance read strips — roadmap 42). The seeded
default is **neutral** — no conservative ACL override is baked in (decided posture:
"the agent decides, as now"); the user tightens it by writing rules, which the
ingest **reads** (`sender_rules`) and **writes** (an `engine_rule` extraction is
appended here, not filed as a fact). The governance half is *all prose, no
metadata*: nothing is materialised onto `scope`. The file is written only
at creation, so the idempotent re-run preserves a user-edited file. See
[ingest-pipeline.md](ingest-pipeline.md) "User policy".

## Why filesystem after DB

The setup transaction commits the SQL rows first, then writes the
filesystem. The rationale is asymmetric error recoverability:

- **DB committed, FS write fails**: the admin has a working account but
  no personal wiki on disk. They can still log in; the dashboard's
  wiki list is empty; the operator can either retry creation through a
  future "create wiki" form or run `wiki_capture` once to trigger
  re-creation. Loud `tracing::error!` flags the failure so it's not
  silent.
- **DB commit fails**: nothing happens, the wizard renders the form
  again with the error. The filesystem is never touched.

The reverse (filesystem first, then DB) would leave orphan `_meta.md`
files behind on every failed setup attempt, contaminating the workdir.

## The login resolution

The form asks for **email + password** — email is the only login
identifier. The handler resolves the email to the canonical `user_id`
via SQL:

```sql
SELECT c.user_id, c.password_hash, u.is_admin
  FROM enrollment_users u
  JOIN user_credentials c ON c.user_id = u.user_id
 WHERE u.email = ?
 LIMIT 1
```

There is **no username fallback**: a user whose `email` is unset cannot
sign in until the admin sets one. Because the admin makes the email
mandatory at invite, that is only ever a transitional state (e.g. a row
that predates the email becoming required). The login field is
`type="email"`, so the browser will not even submit a non-email — which
is why a slug typed into it is rejected outright.

The cookie minted on successful login carries the `user_id` (as
before), so every downstream handler (session middleware, audit
logging, dashboard render) sees the canonical principal.

## CRUD also auto-creates identity wikis

The dashboard's user CRUD (`POST /users/new`) and group CRUD
(`POST /groups/new`) materialize the personal/shared identity wiki at
the same moment the row is inserted in `enrollment_users` /
`enrollment_groups`. Symmetric to the setup wizard:

- For users, the `title` is the `user_id`; `scope = user:<id>`,
  `wiki_type = wiki-user`.
- For groups, the `title` is the `group_id`; `scope = group:<id>`,
  `wiki_type = wiki-group` (the bundled group template;
  `IdentityKind::Group → wiki-group` via `IdentityKind::wiki_type`). A
  group wiki is stamped `wiki-group`, distinct from a user wiki in both
  its `wiki_type` and its ACL.

Same defensive pattern: filesystem failures are non-fatal and surface
as a tracing error, the DB row stays. The on-disk scaffold is what
the user/group sees the very first time they sign in.

## First-login profile wizard

The very first time a user signs in we still want some content in the
wiki — otherwise the user lands on an empty page and the system feels
empty. The wizard at `/dashboard/welcome` fills that gap.

**The freshly-created admin reaches the model first.** The profile primer
calls `wiki_ingest_message`, which degrades to an empty `skip` turn without
a usable `ingest` model — so the `/dashboard/setup` POST lands the new admin
on the LLM config page (`/dashboard/admin/llm-config`), not on
`/dashboard/welcome`. While the admin's `profile_initialized` is still `0`,
that page renders a **step-1 onboarding banner** framing it as the first of
two steps, with a *Continue to profile setup →* link that goes live only once
the `ingest` role has a usable provider (the local backend, or a cloud
provider whose key/login is present — a config-level check, no network
probe). The primer keeps its own guard regardless: a `Save` without
`llm.ingest` hard-refuses (422) behind a no-LLM banner.

Trigger: every login (including the freshly-created admin out of the
setup wizard) checks `user_credentials.profile_initialized`. When the
flag is `0`, the auth flow redirects to `/dashboard/welcome` instead
of `/dashboard/home`. The wizard is presented exactly once: both the
"Save" and "Skip for now" actions flip the flag to `1`, so subsequent
logins go straight to `/home`.

### Three steps → three destinations

The form is one page with **three client-side steps**, as described in the
[memory model](../concepts/memory-model.md),
mapping to the three universal ingest destinations. The routing itself
is **not** the wizard's job — it lives in the ingest prompt and is
universal across every turn. The wizard is *just the
collection UI*: it organises the fields into three steps and adds
reinforcing **section markers** to one composed message.

| Step | Fields | Destination | How the engine routes it |
|---|---|---|---|
| **1 · Chi sei** | `email`, `display_name`, `nickname`, `presentati`, `birthday`, `address`, `language`, `timezone`, `pronouns`, `phone`, `occupation`, `health_safety` | the owner's `index.md` always-on base context | the ingest LLM marks the identity/always-on facts `salience: high`; the engine routes the high-salience core onto `index.md` |
| **2 · Le tue regole** | `sharing_default` (radio: private / group / always-private), `sharing_exclusions`, `private_topics`, `do_not_store` | the sender's `rules.md` engine-policy page | the ingest LLM marks each directive `engine_rule: true`; the engine appends it as policy prose to `rules.md` — never a row in `fact_index` |
| **3 · Il resto** | `favorite_color`, `hobbies`, `food_preferences` | the normal pipeline | no special tag — the LLM files them wherever it sees fit (`salience` stays normal/low) |

All fields are optional. A "Salta tutto" submit repeats on every step,
and a "← Indietro" / "Avanti →" pair (both `type=button`) navigates the
stepper without submitting. With JS off, nothing is hidden: the page
degrades to one long form the user fills top-to-bottom and submits from
the final "Salva e vai".

The free-form `presentati` textarea is the catch-all for everything the
structured fields don't cover, so the wizard deliberately keeps a single
self-description slot (no separate `bio`) and a single likes/interests
slot (`hobbies` covers hobbies *and* interests). `health_safety` is the
one always-on field that is **not** part of the identity card: allergies,
chronic conditions, hard standing constraints ("only ever write to me in
Italian") that an assistant must hold in mind in every interaction.

A **behaviour** rule under step 2 (a name & preferred tone) is
deliberately **out of scope** for the wizard. A tone directive is for
the *consumer* agent, not the memory engine — the engine routes it from a
chat turn via the `behaviour_rule` destination (see
[ingest-pipeline.md](ingest-pipeline.md#agent-behaviour-rules--routed-by-scope-outside-fact-memory)):
into the consumer's own wiki when it is agent-local (keyed by the caller's
consumer identity threaded through `wiki_ingest_message`), or onto this same
`rules.md` as a fact region when the user sets it for every assistant
(`user-global`, roadmap 42).

### Composing the message

On `Save`, the wizard turns the filled-in fields into **first-person
Italian prose** — one clause per field, shaped like the user would say
it in chat — and assembles **one message with up to three marked
sections** (empty sections are dropped), then pushes it through the chat
chokepoint as a single ingest call.

| Field | Clause |
|---|---|
| `email` | "la mia email è X" |
| `display_name` | "mi chiamo X" |
| `nickname` | "mi chiamano anche X" |
| `birthday` | "sono nato il X" |
| `address` | "vivo a X" |
| `language` | "la mia lingua principale è X" |
| `timezone` | "il mio fuso orario è X" — **and** the value lands in `enrollment_users.timezone` (light shape check), the column that drives per-sender reference-time stamping at ingest; the admin can edit it later from the users page |
| `pronouns` | "i miei pronomi sono X" |
| `phone` | "il mio numero di telefono è X" |
| `occupation` | "lavoro come X" |
| `favorite_color` | "il mio colore preferito è il X" |
| `hobbies` | "i miei hobby e interessi sono X" |
| `food_preferences` | "per quanto riguarda il cibo: X" |

**Step 1 (identity).** The identity clauses are joined with `. ` and the
free-form `presentati` is appended as a separate paragraph, the whole
block opened by an explicit Italian **public-consent line** that frames
the content as public. The form header tells the user upfront that
everything they enter here is **public**; the consent line mirrors that in
natural language (no JSON jargon — quoting an ACL keyword verbatim makes
Qwen 3.5 9B copy it into `target_wiki_id` and crash the pipeline). "Public"
is the **visibility** axis: the `ingest` slot emits each `wiki_capture`
with `allow_ids: ["global"]` while `owner_id` stays the **subject** (the
user filling the primer) — the facts are *about me* and *visible to all*,
not *owned by everyone*. (Keeping `owner` on the subject keeps the
[same-owner supersede guard](../concepts/identity-and-acl.md) effective:
two users' public primer facts can never supersede each other, which an
`owner=global` framing would allow.) **The primer is the public-profile channel:** the
identity clauses, `presentati`, **and** the always-on `health_safety` line
all sit inside that public block, so a health/safety entry is captured
public too (the LLM still marks it `salience: high`). The form is explicit
about this and offers the opt-out in plain terms — leave a field blank and
add it later through normal chat, which is private by default. (There is
still no hardcoded "health = private" gate; the primer simply frames *all*
of its content public, and private facts arrive through the ordinary,
private-by-default path.)

**Step 2 (rules).** The preset answers become imperative policy
sentences under a section marker that frames them as governance rules,
not facts ("Le indicazioni che seguono NON sono fatti su di me, ma le mie
REGOLE …"). The `sharing_default` radio maps to one sentence (private /
group-decides / always-private); `sharing_exclusions`, `private_topics`,
`do_not_store` each add a bulleted directive. The ingest LLM recognises
each as an `engine_rule` and the engine appends it to `rules.md`.

**Step 3 (the rest).** The leftover low-weight preferences as plain
first-person prose under a "niente di importante" marker; no routing tag.

Per the single-chokepoint rule, the composed
message is **not** passed to `wiki_ingest_message` directly. The wizard
hands it off to the chat panel — the single chokepoint for every LLM
call originating from the dashboard — by calling
[`mwe_dashboard::routes::chat::process_submission`](../../crates/mwe-dashboard/src/routes/chat.rs).
That function is what the chat panel's submit form also uses; the
wizard primer enters the system in the exact same shape a turn typed
by the user would. `sender_id` is the logged-in user.

After `process_submission` returns the [`ChatTurn`], the wizard:

1. Marks `user_credentials.profile_initialized = 1` so the next login
   skips this page.
2. Renders a small landing page ("Benvenuto, tutto pronto") with a
   `<script>` inline that publishes `window.__mweChatPrimer = {
   user_text, response_html, ts }`.
3. The persistent right-side chat panel — rendered on every
   authenticated page by `ui::layout` — boots `chat.js` with `defer`.
   On its first run, `chat.js` checks for that global, splices the
   primer into the localStorage history, renders it in the message
   list, and scrolls to bottom.

The user lands at `/dashboard/home` post-redirect (no, at the welcome
landing — the panel is what shows the primer turn). The primer is now
in their wiki (via the autocapture done by `process_submission`) and
also in the panel's localStorage history (purely for scrollback).

**Why through the chat, not direct.** Routing every dashboard-side LLM
call through `process_submission` keeps a single entry point to the
engine, rather than two with slightly different setups (context hint,
recent messages handling). The wizard is "a UI that composes a primer
message and pushes it through the chat": if the chat changes (logging,
rate limiting, prompt adjustments), the wizard inherits the change for
free.

The composed primer reaches
[`mwe_core::ingest::wiki_ingest_message`](../../crates/mwe-core/src/ingest.rs)
via `process_submission` with `ContextHint::DashboardCommand`. The
`ingest` LLM classifies the message as `capture`, emits N separate
`wiki_capture` calls (one per piece of information), each with the
appropriate `fact_type` / `topics` chosen by the LLM. The user ends up
with a granular memory of typed facts — indistinguishable from what
they would build by chatting in the panel later.

**No fallback** (the [internal-LLM no-degrade rule](../design-notes/llm-functions.md)).
If `llm.ingest` is not configured in `mwe-mcp.config.yaml`, the Save
action returns 422 with an explicit message naming the missing slot.
The wizard's GET form also shows a red banner so the operator knows
upfront that Save will refuse. `Skip` always works — it's the escape
valve when the operator hasn't configured `ingest` yet.

On `Skip`, the LLM call is bypassed entirely; only the flag flip +
redirect to `/home` happen. The user lands on `/home` with an empty
personal wiki and can fill it in later via the chat panel (the same
ingest path, so the same three destinations apply).

Migration 0018 added `profile_initialized INTEGER NOT NULL DEFAULT 0`
to `user_credentials`. Default is "wizard pending" so every freshly
created user (including the admin) lands on `/welcome` on their first
session. Existing rows that predate the email column (none on fresh
deploys) get the same treatment automatically. The flag only flips on
explicit completion
(Save) or explicit skip (Skip) — a Save that 422s because `llm.ingest`
is missing leaves the flag at 0, so the operator can configure the
slot and retry.

## Current limitations

- **Email-based recovery via SMTP** now ships (roadmap 28). When the admin
  configures the `email:` backend (the Email section of
  `/dashboard/settings/me`), the login
  page offers *Forgot your password?* → a one-shot `password_resets` link
  is emailed and the user sets a new password. The CLI break-glass
  (`mwe-mcp admin-reset --user <slug>`, prints a single-use invitation URL)
  stays as the no-SMTP / admin-locked-out fallback. **TOTP two-factor
  authentication** ships alongside it. Full mechanism:
  [JWT & session model §recovery and 2FA](jwt-and-session-model.md#password-recovery-and-two-factor-authentication).
- **No profile-edit form post-wizard**. The wizard fires once;
  subsequent edits to the profile fact happen by editing
  `wikis/<user_id>/index.md` directly (or via the `/dashboard/wiki/:id`
  edit flow).
- **No typed per-user profile schema** (display_name, avatar, …). The
  enrollment row carries no free-prose blurb: the wizard captures
  identity data into the wiki **as facts**, not into structured columns
  promoted to notifications, SSO, and the like. The two exceptions are
  the columns the *engine plumbing* needs deterministically — `locale`
  (prompt LANGUAGE directive) and `timezone` (per-sender reference-time
  stamping, migration 0061) — which the wizard and the users page also
  populate. A group's only operator prose is its `scope` (what the
  ingest classifier routes on).

These gaps are planned future work — see the
roadmap.

## Where to look in the code

- [`migrations/0045_user_email_on_enrollment.sql`](../../migrations/0045_user_email_on_enrollment.sql)
  — moves the login `email` onto `enrollment_users` (the row born at
  invite) and retires the `user_credentials.email` column from
  [`0017`](../../migrations/0017_user_credentials_email.sql).
- [`crates/mwe-core/src/wiki.rs`](../../crates/mwe-core/src/wiki.rs)
  — `create_identity_wiki` helper + `IdentityKind` enum + tests.
- [`crates/mwe-dashboard/src/routes/setup.rs`](../../crates/mwe-dashboard/src/routes/setup.rs)
  — `SetupSubmission` with `email + admin_id + password + password_confirm`,
  the three-field form, and the post-commit identity-wiki call.
- [`crates/mwe-dashboard/src/routes/users.rs`](../../crates/mwe-dashboard/src/routes/users.rs)
  — admin "Add user" / "Edit user": collects the mandatory email at
  invite, enforces uniqueness, and is the only place it changes.
- [`crates/mwe-dashboard/src/routes/login.rs`](../../crates/mwe-dashboard/src/routes/login.rs)
  — `LoginSubmission` with `email + password`, the single email-only resolver.
