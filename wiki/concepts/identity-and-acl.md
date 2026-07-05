---
title: Identity & ACL model
area: concepts
status: implemented
last_review: "2026-07-02"
---

# Identity & ACL model

This is the canonical concept page for **who exists** in an mwe-mcp
deployment and **who can read what**. It ties together three layers
that are easy to conflate but are deliberately kept orthogonal:

1. The **identity model** — users, groups, the single-admin invariant,
   and the split between a *login account* and a *memory identity*.
2. The **block-level ACL** — the region-by-region read filter that runs
   on every read, plus the declarative default-private/group-shared
   policy and the per-sender redaction that happens *before* any text
   reaches a consumer agent.
3. The **operator privilege levels** — what a consumer agent, a
   signed-in dashboard user, and the admin each get to see and do.

One narrower, wiki-scoped mechanism sits on top of all three and is
described at the end: **per-wiki sharing** (`shared_with`, §4). Above the
block-level ACL there is **no wiki-level access gate**: a whole wiki (or
page) is reachable by a reader iff it holds at least one fact that reader
can read — visibility is **derived** from the per-fragment ACL, never
declared on the wiki (§5).

The deep render/evaluator mechanics (the redaction pseudocode, the
marker grammar) are not duplicated here — they live in
[`../design-notes/redaction-policy.md`](../design-notes/redaction-policy.md)
and [`../design-notes/marker-grammar.md`](../design-notes/marker-grammar.md).
This page is the *model*; those pages are the *implementation*.

---

## 1. Identity model

### Users and groups

The single source of truth for "who exists" is two database tables in
`engine.db`: `enrollment_users` and `enrollment_groups`. They are
managed through the built-in dashboard's CRUD, and the in-memory shapes
that back the form submissions live in
[`crates/mwe-core/src/enrollment.rs`](../../crates/mwe-core/src/enrollment.rs)
as `EnrollmentFile { version, users[], groups[] }` /
`UserEntry` / `GroupEntry`.

- A **user** is a person (or a bot's "system user") that can be a
  sender, a fact owner, and the owner of a personal memory wiki.
- A **group** is a named set of users with a free-prose `scope`. A
  group can own a shared memory wiki and can appear as a principal in a
  region's ACL (`group:famiglia`). Group membership is what the ACL
  evaluator matches `Principal::Group(_)` against.

There is no junction table: a group stores its member list as a JSON
array in a `TEXT` column (see §1.6 for the rationale). A user stores
its aliases the same way.

**The builtin `global` group.** One group always exists: `global`, the
universal everyone-group. It is the public/world principal — a region
naming it (in `owner`, `sender`, or `allow`) is readable by anyone, because
every user is implicitly a member (enforced in
[`acl::principal_matches`](../../crates/mwe-core/src/acl.rs), not stored in
the member list). It is a real `enrollment_groups` row so the admin can edit
its **`scope`** from the dashboard — the prose the `ingest` classifier reads
to recognise a genuine *world* fact (e.g. weather, common knowledge) and
route it to `owner=global`, as opposed to a *public personal* fact (which is
`owner=<subject>` + `allow=global`). It is special-cased throughout: never
hand-membered or deletable, excluded from the collaborative-group list that
seeds `group_theme` hubs, and seeded by migration `0047` /
[`enrollment::ensure_global_group`](../../crates/mwe-core/src/enrollment.rs).

**The builtin `guest` pseudo-identity.** The second builtin is the inverse
of `global`: not "everyone" but "someone we cannot identify" — an
unrecognized voice on a voice satellite, an unknown sender in a family group
chat, a walk-up kiosk user, a house guest. Consumers that meet such a human
act as **`guest`** instead of falling back to a wrong real identity. Unlike
`global`, guest is **not** an enrollment row: it has no wiki, no login, no
aliases, and can never hold a token
([`enrollment::validate_token_identity`](../../crates/mwe-core/src/enrollment.rs));
the id is **reserved** (`enrollment::GUEST_USER_ID`, enforced by `validate`
and every dashboard create form), so no real user, group, or bot can ever
claim it. It exists only as an **effective sender**, reached per turn via
`X-MWE-Act-As: guest` under the same per-consumer delegation roster as any
real user — **granting `guest` to a consumer from the dashboard's
delegation editor IS the enable switch** (off by default: no roster
contains it). The contract, per surface:

- **Ephemeral memory.** A guest `wiki_ingest_message` turn files nothing —
  no capture, no closures, no behaviour rules — and skips the classifier
  entirely; the `rules` channel carries a fixed reserved-behaviour
  directive instead (see
  [ingest-pipeline.md](../design-notes/ingest-pipeline.md#the-guest-short-circuit--ephemeral-turns-for-the-unidentified-human)).
  There is one shared `guest`, not episodic per-visitor identities (that
  would mint dangling principals); a recurring guest who should be
  remembered is promoted by **enrolling them normally** and delegating the
  new identity — with nothing stored under guest, there is nothing to
  migrate by construction.
- **Reads are the public slice, by derivation.** Guest goes through the
  same `can_read` as everyone (§2) with no special case: `user:guest`
  matches no region (the id is reserved, so no fact ever names it),
  guest belongs to no group, and the `global` arm matches anyone — so a
  guest reader sees exactly the regions where `global` appears in
  `owner ∪ allow ∪ sender`, on every read surface (recall, `wiki_read` /
  `wiki_search` / `wiki_navigate`, the reader-relative cards).
- **The permanent-write / operator surface refuses it.**
  `wiki_ingest_external`, `wiki_admin_notify`, `consumer_register`,
  `tool_log_search`, `dashboard_link` (it mints a signed dashboard session
  token), and `POST /media` all reject the guest sender
  (`forbid_guest` in
  [`tools.rs`](../../crates/mwe-mcp-server/src/mcp/tools.rs)); the
  governance blocks (`pending_attention` / `pending_votes`) stay off guest
  turns. Everything else is already closed by the authority model
  (guest owns no wiki, authored no facts).

### The dashboard owns identity

mwe-mcp reads no identity config file at boot. Bootstrap
(`mwe-mcp init`) only creates the workdir, runs the migrations against
an empty `engine.db`, and generates the `MWE_TOKEN_SECRET`. Everything
else — the first admin, every subsequent user, every group — is created
through the dashboard.

The `EnrollmentFile` / `validate` / `mirror_to_db` trio in
`enrollment.rs` is the validation-and-write machinery the dashboard CRUD
handlers call. It would also back an eventual one-shot
`import-enrollment` admin action, if one ever ships — but there is no
canonical YAML on disk.

### Who creates whom

| Actor | How it is created |
|---|---|
| **First admin** | The dashboard **first-run wizard** at `/dashboard/setup`. This route is served *only while `enrollment_users` is empty*; after the first user it disappears and redirects to `/dashboard/login`. The wizard asks for an email, a `user_id` slug, and a password, then inserts one `enrollment_users` row with `is_admin = 1` plus an Argon2id hash in `user_credentials`. |
| **Subsequent users** | Dashboard CRUD at `/dashboard/users`. The admin clicks "Add user", fills `id` / `email` / `aliases`, and mwe-mcp issues an **invitation link**. The invited person opens the URL out-of-band, chooses their own password, and is enrolled. The admin never sees the new user's password. **No form carries an `is_admin` toggle** — every invited user is a regular user. |
| **Groups** | Dashboard CRUD at `/dashboard/groups`. Groups are pure metadata (id, members, scope) — no login, no invitation. |

### The single-admin invariant

**Exactly one admin per deployment.** The setup wizard is the only
producer of a row with `is_admin = 1`; every later flow creates regular
users. The invariant is enforced at the database level by a partial
unique index (`idx_single_admin` on `enrollment_users(is_admin) WHERE
is_admin = 1`, migration 0015), so even a hand-crafted `INSERT` that
tried to add a second admin row fails at the SQL layer. The
application code deliberately does **not** re-check this — the DB is the
last line of defense. (The
`db_rejects_two_admins_via_partial_unique_index` test in
`enrollment.rs` pins this.)

Role transfer is not exposed in the dashboard. If the admin disappears
or wants to hand off, the operator acts from the host shell
(`mwe-mcp admin-reset --user <id>` to re-invite the same admin, or a
direct `UPDATE` to change the identity).

### Account vs. identity — the load-bearing split

An **account** and an **identity** are separate things that map
one-to-one but live on different axes. This split is documented in
[`../design-notes/setup-and-identity.md`](../design-notes/setup-and-identity.md);
the model in one table:

| Concept | What it is | Where it lives | Cost to change |
|---|---|---|---|
| **Account** | The credential pair used to sign in (`email + password`). | `user_credentials` table | Cheap — a single `UPDATE` on `user_credentials.email`. |
| **Identity** | The canonical principal of the domain: the `user_id` slug. | `enrollment_users` row + `<workdir>/wikis/<user_id>/` directory | Expensive — the slug is wired into the filesystem and every fact. |

The slug appears in **every `fact_index` row's `owner_id`**, in every
exported marker (`{{owner=user:franz}}…{{/}}` — the full form a
portable archive carries), in every JWT's `sender` claim, and as the
directory name of the user's personal memory wiki. Changing it would ripple through the whole
filesystem, so it is chosen once at creation and never rewritten. The
email is just the login handle and a future SSO/recovery key — it can
change freely.

A third axis sits above both at request time, resolved by the HTTP
middleware (not the dashboard):

- **Token holder** = the `sender` claim of the Bearer JWT — *who owns
  the token*.
- **Effective sender** = *who is logically making the call*. Equal to
  the token holder for single-user clients; equal to the
  `X-MWE-Act-As` header for delegated multi-user bot ("consumer")
  tokens.
- **Fact owner** = the application-level `owner_id` argument on write
  paths — *whose datum it is*. Defaults to the effective sender; when
  it differs, the fact records `owner=user:X` + `sender=user:Y` in its
  `fact_index` columns (spelled inline only in the export form).

The token/session model is documented in full in
[`../design-notes/jwt-and-session-model.md`](../design-notes/jwt-and-session-model.md).

### Connection pattern follows `consumer_class` — the diagonal

The three request-time axes above are *resolved* per call; **which**
pattern a consumer uses is not a free per-deployment choice but a
function of its `consumer_class` claim:

| `consumer_class` | Identity pattern | `sender_id` is | Acts as | Typical |
|---|---|---|---|---|
| **`smart`** | Pattern A (mono-user) | the **human owner** — an account with login credentials | itself; never delegates | a coding agent maintaining a project / smart wiki on one developer's machine |
| **`standard`** | Pattern B (multi-user) | a **system user** — the bot's own credential-less identity, with its own wiki | the human it is serving, via `X-MWE-Act-As` per call | a conversational assistant (Telegram, home automation, mail) serving several people |

The model in one line: **a smart consumer *is* its human owner; a
standard consumer *is itself a user* (a "team member") and reaches the
real humans by acting on their behalf.** mwe-mcp is fundamentally a
memory, and a standard bot attributes each datum to the right human by
delegating, never by writing as them.

This is enforced, not merely conventional, because the failure mode is a
confidentiality leak: if a standard consumer authenticated *as a human*
(Pattern A) while serving several people, every read would be redacted
for the wrong effective sender (`render_for_sender`, §2) and one user
would receive another's private regions. Pinning the pattern to the
class closes that footgun by construction. Three places enforce it:

1. **Token issue.** `mwe-mcp token-issue` and the dashboard token form
   share one validator (`enrollment::validate_token_identity`): a
   `standard` token is rejected unless its `sender_id` is a
   credential-less **system user** *and* it carries a `consumer_id`.
   This is the security-critical guard — it makes an onboarded human
   (one *with* a `user_credentials` account) un-bindable as a standard
   sender. The `smart` side stays a convention (a smart token is
   mono-user, harmless regardless of sender). The "bot vs not-yet-onboarded
   human" ambiguity that credentials alone could not settle is now closed
   by the explicit **`is_agent`** marker (`enrollment_users.is_agent`,
   migration 0050): set when a standard token connects (point 3), it is
   **mutually exclusive** with a `user_credentials` login —
   `enrollment::reject_if_agent` refuses a dashboard login to an `is_agent`
   identity (the mirror of this token-side guard), so an identity is EITHER
   a human with a login OR a bot's credential-less system user, never both.
2. **The act-as gate.** The MCP middleware permits `X-MWE-Act-As` only
   for `standard` tokens (`403 act_as_requires_standard` otherwise) —
   see [`../design-notes/jwt-and-session-model.md`](../design-notes/jwt-and-session-model.md).
3. **The binding.** A standard consumer's identity (`sender_id`, a
   system user), its deployment (`consumer_id`, a row in the `consumers`
   registry), and its delegations (`consumer_delegations`) are tied
   together by `consumers.system_user_id`. This binding is established
   **from the token itself**: the MCP auth middleware
   (`consumers::ensure_agent_identity`) records it — and stamps `is_agent`
   — the moment a standard token connects, taking the bot's own pre-act-as
   `sender_id` as the system user. The `consumer_register` tool still
   records the same thing explicitly, but the connection-time path means a
   conversational bridge that never calls it (e.g. a per-turn
   `wiki_ingest_message` consumer) is wired anyway, not left silently
   unbound. The link consumer ↔ system-user ↔ wiki is thus explicit, never
   inferred from "this user happens to have no password".

A **system user** is an `enrollment_users` row with no `user_credentials`
row — a pure memory identity that cannot log into the dashboard. The
operator creates one from the dashboard user CRUD, discards the invitation
link, and sets its delegations; the `consumers` row and the `is_agent`
marker are then materialised automatically the first time the bot's token
connects (point 3), so the only manual step is the operator-side identity
+ delegation setup.

### 1.6 Id rules

The id rules are enforced in pure Rust (no `regex` dependency) by
`is_valid_user_id`, `is_valid_group_id`, and `is_filesystem_safe` in
[`enrollment.rs`](../../crates/mwe-core/src/enrollment.rs), and the same
rules run on both the bulk mirror writer and the dashboard CRUD forms —
a single-user insert can never produce an id the bulk validator would
later reject.

#### User id — `^[a-z][a-z0-9°]*$`

Start with a lowercase ASCII letter, then lowercase letters, digits, or
the degree sign `°`. The `°` is the **collision-suffix allowance**: when
a second person named "bob" must be enrolled, the operator disambiguates
with `bob°2` (the Samvise consumer convention). The degree sign is
*intentionally* permitted inside user ids so this suffix is expressible.

The charset is by construction a **subset of the `WikiId` charset**
(`[a-z0-9°-]`): every enrollable id can always be created as an identity
wiki. Underscores are rejected for exactly that reason — `WikiId::parse`
refuses `_`, so an underscore id would enroll and then silently fail to
get its identity wiki (maintainer decision 2026-07-02; the inclusion is
pinned by the `every_valid_id_is_a_valid_wiki_id` test in
[`enrollment.rs`](../../crates/mwe-core/src/enrollment.rs)).

#### Group id — `^[a-z][a-z0-9]*$`

Same shape, but **the `°` is dropped**. Groups deliberately disallow
the degree sign so the collision-suffix convention stays unambiguously
user-scoped: if a group could carry `°`, a group named `a°b` could be
mistaken for the collision suffix of a same-named user. Underscores are
out for the same identity-wiki reason as user ids. Groups must
also never collide with a user id, because both share the single flat
`wiki/<id>` namespace — a group whose id equals an existing user id is
a **hard error**.

#### Filesystem-safety

On top of the id regex, `is_filesystem_safe` rejects any id containing
`/`, `\`, `..`, or whitespace — applied *before* any DB key or
filesystem path is formed from the id, so a slug can never escape the
workdir or land in an unsafe path.

#### Hard-abort vs. soft-warn

`validate(&EnrollmentFile)` distinguishes two validation outcomes:

| Outcome | Triggers | Surfaced as |
|---|---|---|
| **Hard abort** | unsupported `version`; invalid user/group id regex; duplicate user id; duplicate group id; group↔user id collision; dangling group member (a group lists a user not in `users[]`); unsafe filesystem slug | `Result::Err(EnrollmentError::…)` — short-circuits, nothing is written |
| **Soft warn** | an alias shared between two users; an alias that matches an existing user id | accumulated in `ValidationReport::warnings: Vec<String>` — the operation still proceeds |

A hard error stops the whole submission (the mirror write never runs).
A soft warning is collected and returned; the dashboard handler is
expected to render it back to the operator ("saved with N warnings")
and proceed. Aliases stay usable even when ambiguous — the warning just
flags that resolution may be uncertain.

The mirror write itself (`mirror_to_db`) runs inside a single
`sqlx::Transaction` that `DELETE`s both tables and re-`INSERT`s every
row, so read-only consumers (REM, the ACL paths) never observe a
half-applied identity state.

#### 1.6.1 JSON columns for aliases/members — the `groups_for` trade-off

`aliases` (on `enrollment_users`) and `members` (on `enrollment_groups`)
are stored as JSON arrays in `TEXT` columns rather than in junction
tables. This is a deliberate trade-off:

- **Pro:** the schema stays flat — no `enrollment_user_aliases` /
  `enrollment_group_members` side tables to keep in sync.
- **Pro:** reading a full row gets the aliases/members in one fetch,
  no JOIN.
- **Con:** the inverse query — *"which groups does user X belong
  to?"* — cannot use a conventional JOIN.

That inverse query is exactly what the ACL evaluator needs (to fill in
the sender's group list), and it is answered by
[`groups_for(pool, user_id) -> Vec<String>`](../../crates/mwe-core/src/enrollment.rs).
Rather than restructure the schema, `groups_for` queries the JSON
column directly:

```sql
SELECT group_id FROM enrollment_groups
 WHERE EXISTS (SELECT 1 FROM json_each(members) WHERE json_each.value = ?)
 ORDER BY group_id ASC
```

The `EXISTS (… json_each …)` pattern is the same JSON1 idiom
`fact_index` already uses for its `topics_any` filter, so the cost is
real but only paid on demand. Every production construction site of the
`SenderContext` that the ACL matcher consumes fills its
`sender_groups` via `groups_for` — the bare `SenderContext::user`
constructor leaves the list empty and is therefore only safe for tests
and the anonymous/bootstrap path. A sibling, `groups_with_scope_for`,
carries the group `scope` prose along for the ingest classifier, which
uses it to route a capture into a group's shared memory.

(If a hot "all users in group X" SQL query is ever needed, the answer
is a generated column or a junction-table migration — but with the
dashboard tables as the source of truth, that cost is incurred only
when it actually arrives.)

---

## 2. Block-level ACL model

mwe-mcp's access control is **not scope-level**. There is no "this wiki
belongs only to X" gate on reads — every wiki is navigable by every
authenticated user. The filter is **region-by-region**, applied on
every read path, and it hands each reader a *declassified document*.
This is "the single law": there is one rule, evaluated per region.

### Marker fields

A region is a span of text delimited by an inline marker:
`{{owner=… sender=… allow=… f=…}}…{{/}}`. Three of those fields carry
the ACL; the fourth (`f`) is the fact id. The ACL data model lives in
[`crates/mwe-core/src/types.rs`](../../crates/mwe-core/src/types.rs) as
`Principal` (`user:<id>` / `group:<id>` — every principal is a user or a
group; the builtin **`global` group** is the universal everyone-group, see
[§1 Users and groups](#users-and-groups)) and
`Acl { owner: Option<Principal>, allow: Vec<Principal> }`.

The three ACL fields are **three independent axes** — provenance, subject,
and visibility — and any of them may name a user or a group (`global`
included):

| Field | Axis | Meaning |
|---|---|---|
| **`owner=`** | *subject* — who the fact is **about** | `owner=user:alice` ⇒ a fact about alice; `owner=group:team` ⇒ about the team; `owner=global` ⇒ a **world** fact about no one in particular ("ieri ha piovuto"). Owner is **not** a visibility flag. **`owner` absent ⇒ the region's owner-of-last-resort is its `sender`** (resolved before the check; unreadable if it has no sender — never the wiki principal). |
| **`sender=`** | *provenance* — who **captured** it | A full principal — `user:Y` (Galadriel wrote a fact about Gollum), `group:Y` (an ambient "family microphone"), or the `global` group (a public capture device). The sender is *always* allowed to reread their own capture. Omitted when it would equal `owner=user:X`. |
| **`allow=`** | *visibility* — who **else** may read | Extra principals beyond owner and sender — comma-separated, each prefixed (`allow=group:team,user:bob`). **A fact is public when the `global` group is in `allow`** (`allow=global`), with `owner` left on the subject — *about me, visible to all*, not *owned by everyone*. Purely additive. |
| **`f=`** | — | The region's `fact_id` (canonical `UUIDv7`). Identity, not access — listed here only because it shares the marker. |

At runtime the engine writes the **bare** marker (`{{f=<uuid>}}…{{/}}`)
and stores the ACL in the `fact_index` columns only — the **DB is the
authoritative source**: redaction resolves a region's ACL by its `f=`
key from `fact_index`, falling back to the inline attributes only for
regions the DB does not know (legacy pages, imported archives — the
attributed form above remains valid input and is the export format).
See [`../design-notes/redaction-policy.md`](../design-notes/redaction-policy.md)
and [marker grammar §0](../design-notes/marker-grammar.md).

### `owner` is always an existing principal — non-enrolled subjects

`owner` is the **subject**, but the system has only one vocabulary for a
subject: a `Principal` (`user:<id>` / `group:<id>` / `global`). It therefore
never invents one. A subject the deployment does **not** enrol — a relative
who does not use the system (Bruno, Franz's father), a pet, a third party —
is **never** minted as a `user:<them>`: that would be a dangling principal
no reader matches and no enrolment backs. The ingest/document classifiers
resolve such a subject to an **existing** principal instead:

- the **group whose `scope` the fact falls inside** (the same scope signal
  that drives `allow`) — the collective that holds responsibility for that
  subject. Bruno's health and care facts → `owner=group:famiglia`,
  `sender=user:franz`, readable by every `famiglia` member.
- else `user:<sender>` — "a note the sender holds about someone".

The non-enrolled individual's name lives in the region's **prose**, not in a
principal. Read the pillar as *owner = the principal that **governs** the
subject*: an enrolled subject governs themselves; a non-participating subject
is governed by the collective (or the capturer) responsible for them — so a
group `owner` here is not "the subject is the collective", it is "the
collective governs facts about this member". This keeps `owner` stable
across re-ingests (no per-run minting), which is what lets the subject's
facts share **one** home wiki and be deduplicated. The rule lives in the
classifier prompts
([`ingest.md`](../../crates/mwe-core/prompts/ingest.md),
[`document-extract.md`](../../crates/mwe-core/prompts/document-extract.md));
the document path injects the `known_users` roster so it can tell enrolled
from non-enrolled.

**Who may *act* on a fact keys on these same axes** (the write-authority
model), and the line is **destroy vs update**: the
**`sender`** (author) may **`delete`** their own contribution directly —
[`acl::can_delete`](../../crates/mwe-core/src/acl.rs) = `is_admin ∨ sender == caller` — while the
**`owner`** (subject) may **update** the fact directly: edit its content (`supersede`), shift or close
its validity, and change its visibility (`acl_change`) —
[`acl::sender_owns`](../../crates/mwe-core/src/acl.rs) ‖ admin. So *reading* resolves on
`owner ∪ allow ∪ sender`, *destroying* keys on `sender`, and *updating / re-sharing* on `owner`. An
update is the subject keeping a fact about themselves current (never a vote); only destruction is
governed. A non-sender who wants a fact gone — even its owner — has no direct path: instead they open a
request, **from the dashboard**, that its audience votes on
([`votes::open_forget_request`](../../crates/mwe-core/src/votes.rs), silence = consent). A vote is
only ever opened (and cast) when a user acts from the dashboard — never started in the background by
a consumer agent. A `sender` may also clear **all** their own facts in bulk
([`fact_index::mark_forgotten_by_sender`](../../crates/mwe-core/src/fact_index.rs)). Restructuring
(deleting a page or wiki, `move`) is the admin's.

> **Naming caveat — the region `owner` means *subject*, and that is load-bearing.**
> Coming from Unix/IAM, "owner" suggests the *creator* or the *access
> controller*. The per-fragment ACL `owner` of *this* section — the
> `owner=` marker field, `fact_index.owner_id`, [`Acl::owner`](../../crates/mwe-core/src/types.rs) —
> is neither: the creator is `sender` (provenance), the visibility is
> `allow` (audience), and `owner` is the fact's **subject** — who or what
> it is *about* (a user, or a group when the subject *is* the collective).
> The name is kept on purpose: the data subject **governs who may read**
> the fact about them (an `acl_change` is owner-or-admin), so the subject
> genuinely *owns* the datum on themselves — per-fragment governance seen
> from the subject's side.
>
> Do **not** conflate this with the **wiki-level owner** — the principal a
> whole memory wiki belongs to (`WikiMeta.owner_user`: a user for a
> personal wiki, the **group** for a `wiki-group`; see §1 and §4). *That*
> owner is a genuine **proprietor/master** — the access controller in the
> classic sense, the authority for wiki-level acts — and is a **separate
> axis** from the per-fragment subject. The two are independent: a fact
> `owner=user:franz` (subject) can live in a wiki owned by `group:famiglia`
> (proprietor). A full rename `owner → subject` was considered and
> **declined** near release precisely because the word spans both senses
> (plus `owner_id` doubling as the token holder), so a blind rename would
> corrupt unrelated code for little gain. The concept is reinforced
> instead; the name stays. **Within this section**, read `owner` /
> `owner_id` / `owner=` as *subject* — never "creator" or "visibility".

### The single rule

The evaluator `acl::can_read` builds the **effective principal set** —
`owner ∪ allow ∪ {sender_of_region}` — and grants read if *any*
principal in it matches the current reader (the reader's `sender_id`
for `User`, the reader's group list for `Group` — and the builtin
`global` group, which every user belongs to, matches everyone). The
`sender_of_region` element is what makes the
"family microphone" case work: a region captured by `group:famiglia`
is readable by every family member even when `famiglia` is not in
`allow=`. The matcher is monotonic in `allow` — adding a principal can
only ever grant access, never revoke it.

Crucially, **`isAdmin` does not bypass the evaluator.** It has no
`is_admin` parameter by construction: the API — and every MCP tool that
honours the ACL — cannot grant blanket access. The `isAdmin` JWT claim is
a *UI gating hint* for the dashboard's admin-only pages, never a
server-side ACL override on the tool surface. (The `admin_does_not_bypass`
test documents this invariant.) The dashboard does offer the admin an
**ACL-reveal** lens over redacted fragments (and over the facts table) —
gated server-side on the admin role, dashboard-only, and built on top of
`can_read` rather than around it; see
[redaction-policy.md](../design-notes/redaction-policy.md#dashboard-admin-reveal).

### Default-private vs. group-shared — the declarative policy

The ACL is **default-private**. A region with no ACL of its own — no
DB record and no inline `owner=` — falls back to its own `sender` as the
owner of last resort (e.g. `user:alice`, the user who captured it;
unreadable to anyone else, and to no one when there is no sender). To
expose a region
beyond its owner requires a *deliberate* declarative act at capture
(recorded in the region's `fact_index` columns; spelled inline only in
the export form):

- `allow=…` — extend reading to extra users or groups, or
- `allow=global` — make it public (the builtin `global` group is everyone),
  with `owner` left on the subject, or
- `owner=group:team` — file it as a fact *about* a group (every member then
  reads it).

(`owner=global` is **not** in this list: it marks a *world* fact about no one
in particular, not "a public fact" — public is the `allow` axis above.)
There is no implicit widening. This is what "declarative sharing
policy" means: visibility beyond the default is declared per region,
at capture, never inferred. Lista-style pages are the typical place
where the sender fallback fires — a hand-written list item with
no DB record and no inline owner leans on its capturing `sender`.

#### The ACL card boundary — what card metadata may carry

The recall-navigation **cards** — a wiki's `_meta` `keywords`/`summary` and
each page's testata `keywords` — are wiki-level metadata. They live at two
tiers, with two boundaries:

- **The compile-time `.md` card** (the operator's Obsidian view) carries every
  topic at the wiki's **default visibility** — the boundary the keyword syncs
  enforce
  ([`meta_annotate::fact_at_default_visibility`](../../crates/mwe-core/src/meta_annotate.rs)):
  **a fact contributes its topic words only when its owner is `global` or the
  wiki's resolved `scope` principal** (an `allow=` list only *extends*
  readability, so it never disqualifies a default-owned fact). A cross-user
  region, a group-owned region on a user wiki, or any other off-default fact is
  special-cased content: its topics never reach the `.md` card.
- **The served card is reader-relative.** The consumer-facing surfaces that
  expose card metadata — the recall navigator's topic seeds, its candidate
  cards and root index (both the ingest recall path and the `wiki_navigate`
  tool) — never hand out that `.md` card. They recompute it **per reader** from
  `fact_index` ([`meta_annotate::build_reader_card`](../../crates/mwe-core/src/meta_annotate.rs)): the topics are the union
  over the facts that reader `can_read`, and the abstract (`summary` / page
  `description`) is served only to a reader whose read-set covers the wiki's
  default visibility (i.e. matches its resolved `scope`). So a reader
  denied a fact never sees its theme on a card — the boundary is the reader's
  own read-set, not the wiki default. (`wiki_read` likewise strips the testata
  before per-sender projection, so a page read carries no card.)

A wiki (or page) in which the reader can read **no fact** drops from that
reader's catalog and navigator entirely — card included — by **derivation**
([`ReaderCard::readable_wikis`](../../crates/mwe-core/src/meta_annotate.rs)),
not a wiki-level gate. The prose-side counterpart is the Cronista's
`DESCRIPTION` contract (prompt v1.7): a page description orients at topic
level and never carries the content — or even the theme — of a
narrower-than-default fact.

#### Where a group-related capture lands — the routing rule

Sharing a fact *with* a group is orthogonal to *whose wiki* the fact
lives in. mwe-mcp deliberately keeps two legitimate forms for "this
fact concerns a group", and the `ingest` classifier chooses between
them at capture time:

- **Under `wiki-group/<scope>/`** — when the capture's `sender` is a
  `Principal::Group(scope)` (a shared device-channel: a family ambient
  microphone sending as `sender=group:famiglia`, a team app sending as
  `group:acmecorp-team`), **or** when an intrinsically-collective,
  no-single-steward entity emerges (a shared shopping list anyone in the
  family appends to and closes, a shared group calendar, collective
  contacts). The fact is *born collective*; no individual is "speaking
  for themselves", so the wiki itself lives under the group.
- **On the user-owned wiki, with a `group:*` ACL** — when a single User
  is the sender and is *materially* the steward: the fact is the user's
  initiative and responsibility, and the group only reads it. Frodo
  announcing a picnic lands in `wiki/frodo/calendario/` with
  `{{allow=group:famiglia}}`; Alice's ACME client docs live under
  `wiki/alice/…`, each fact carrying `{{allow=group:acmecorp-team}}`.

The deciding question is **stewardship**: if a single identifiable user
curates the thing, it lives under them with a `group:*` ACL; if the
collective curates it (no unique custodian) or a scope device-channel
captured it, it lives under `wiki-group/<scope>/`. The REM forge
cluster detector can flag *ex post* a cluster of user-owned captures
that looks collective and propose promotion to `wiki-group/<scope>/`
(opt-in, never automatic).

Either way the fact lives in a **real, existing** wiki — there is no "root"
wiki to fall back to. mwe-mcp's tree is a **forest** of top-level wikis (one
per user / group), so a fact with no natural collective home stays in the
**sender's** wiki with a broader marker — `allow=global` for a public fact (owner
still the subject), `owner=group:*` for a collective one — and the narrative
compiler homes its page in the fact's source wiki, never a root (see
[`narrative-compiler.md`](../design-notes/narrative-compiler.md)).

### Per-sender redaction *before* context injection

The redaction is not a post-filter bolted on at the edge — it is the
read path. Every read (the MCP `wiki_read`/`wiki_search`/`wiki_navigate`
tools *and* the internal `wiki_recall` / `recall_nav` navigator paths that
build the context a consumer agent receives) runs the parser, gates each
region with
`can_read`, and emits a document where invisible regions are replaced
inline by a `[redacted]` marker. **The consumer agent never sees text
it is not authorised for** — the declassification happens before the
snippet is injected into the agent's context, not after.

Two model rules are worth internalising:

- **Free prose always passes through.** Headings, paragraph
  separators, the connective tissue around inline regions are narrative
  scaffolding — both the human reader and the LLM that later
  extracts/supersedes a region need that context. Redaction gates
  *regions*, never prose.
- **Total redaction collapses to one callout.** When a page has at
  least one region, every region was redacted, and there is no
  meaningful prose to anchor the output, the whole render collapses to
  the single callout `> [!redacted] This entire page is private.` — so
  the exact count of hidden regions does not leak.
- **Redaction hides content, not the fact that something was
  redacted.** Replacing a region with an inline `[redacted]` marker
  *deliberately* leaks that a hidden region existed and *where* it sat —
  existence and position are intentionally disclosed: the redaction is
  marked so the reader does not silently miss it. What is hidden is the
  region's text, never its presence. The
  total-redaction collapse above only caps the *count* leak (one callout
  instead of N), not the existence leak.

The pseudocode for the inline-marker placement, the prose/embed
pass-through, the total-redaction collapse, and the owner-of-last-resort
resolution all live in
[`../design-notes/redaction-policy.md`](../design-notes/redaction-policy.md);
the marker grammar and parser behaviour live in
[`../design-notes/marker-grammar.md`](../design-notes/marker-grammar.md).
This page intentionally stops at the model boundary.

---

## 3. Operator privilege levels

"Who can do what above the block-level ACL" resolves into three
distinct surfaces. mwe-mcp itself has **no concept of per-tool
permissions** — any valid token may call any exposed MCP tool — so the
distinction is enforced by *which surface* a principal reaches, not by a
permission matrix on the tools.

| Level | Who | Surface | What they see / can do |
|---|---|---|---|
| **(1) Consumer** | A consumer agent (Claude Code, a Telegram bot, …) holding an MCP token | The MCP tool surface only | Reads and writes the memory through the tools. Every read is redacted per-sender (§2); no destructive/structural operations are exposed as the agent's primary path — structural intent is bounced to the dashboard via `dashboard_link`. The agent never sees filesystem paths and never sees text outside its ACL. |
| **(2) Signed-in user** | A regular user logged into the dashboard | The dashboard, filtered to their own session | Sees **only their own redacted view**. The memory explorer renders every file through `render_for_sender` for *that* signed-in user, so a normal user browsing the dashboard sees exactly what the ACL grants them — same redaction as a consumer agent acting on their behalf. Admin-only pages return 404. |
| **(3) Admin** | The single admin (the `isAdmin` JWT claim) | The full dashboard, including the agentic chat panel | Sees everything the admin pages expose (users, groups, tokens, audit, costs) and can **operate on the memory** through the agentic chat — atomic CRUD/structural ops composed from the internal `_internal.*` toolset, with an explicit **confirmation step before every write**. The memory viewer redacts per the admin's own ACL by default, but the admin can flip an **ACL-reveal** toggle (in Settings) to see every fragment — and every user's facts on the facts page — so supervision actions can reach them (see the clarification below). |

A few clarifications that keep these levels from blurring:

- **The admin seeing other users' data is not act-as.** When the admin
  opens an audit view and reads another user's `tool_executions`, the
  effective sender stays the admin; admin-only tools take an explicit
  `target_sender_id` argument gated by `if !is_admin: reject`. The
  audit log records "admin (isAdmin) searched user X's logs", not "user
  X searched their own logs". This is distinct from the multi-user-bot
  `X-MWE-Act-As` delegation, where the effective sender genuinely
  *becomes* the delegated user.
- **The dashboard chat is operational, not conversational.** It is the
  place where structural operations on the memory happen (CRUD,
  forge, move, batch edits, recall-and-correct) — not a Q&A about the
  memory's contents. Its writes go through the hard-rule
  confirmation-before-write flow. See
  [`../design-notes/agentic-chat.md`](../design-notes/agentic-chat.md).
- **`isAdmin` is a UI gate, not a tool-surface ACL bypass** — see §2.
  On the MCP tool surface the region-level read filter applies to the
  admin like everyone else. The one exception is the dashboard's explicit,
  opt-in **ACL-reveal toggle** (a single control on the Settings page): an
  admin can flip a dashboard-wide lens that shows (highlighted) the
  fragments redaction would hide **and** lists every user's facts on
  `/dashboard/facts` so the owner-or-admin fact actions can reach them. It
  is gated server-side on the admin role, dashboard-only, and never
  touches the MCP tool surface; until it is on, the memory viewer still
  redacts and `/dashboard/facts` stays ACL-projected (it does **not** show
  another user's facts by default — that was the gap reveal now covers). It
  is built on top of `can_read`, never around it (see
  [redaction-policy.md](../design-notes/redaction-policy.md#dashboard-admin-reveal)).

This separation is what keeps mwe-mcp minimal and genuinely
agent-agnostic: a consumer with an LDAP, OAuth, or no permission model
at all integrates the same way, because all "who-can-do-what" policy
above the ACL is the consumer's responsibility (or the admin's, via the
dashboard).

---

## 4. Per-wiki sharing (`shared_with`)

`shared_with` is a **per-wiki** sharing roster that lives in a wiki's
`_meta.md` frontmatter — orthogonal to the block-level ACL of §2. It is
modelled in
[`crates/mwe-core/src/wiki.rs`](../../crates/mwe-core/src/wiki.rs) as
`WikiMeta::shared_with: Vec<Principal>` and resolved by
[`resolve_read_access`](../../crates/mwe-core/src/wiki_admin.rs).

When a wiki's `shared_with` list is non-empty, the listed principals
get **read + notify** access to that wiki on top of its owner:

- they can `wiki_search` / `wiki_read` against it, and
- they can append items to its `_briefing.md` via `wiki_admin_notify`.

**Write tools stay owner-only.** `wiki_admin_push` / `wiki_admin_pull`
preserve the invariant `wiki.owner_user == token.owner_user`; the share
only extends the *read/notify* perimeter, never write. There is an
explicit test (`shared_with_does_not_grant_write_access`) pinning this.

Resolution order (first match wins, so the audit view shows the
most-specific grant): **owner** → direct `Principal::User` in
`shared_with` → `Principal::Group` membership in `shared_with`
(one enrollment lookup, only when a group entry is present) →
`Principal::Global` in `shared_with` → otherwise **denied**. The
outcome is a tagged `ReadAccessOutcome` enum (`Owner` / `SharedUser` /
`SharedGroup(id)` / `Global` / `Denied`) rather than a bare boolean, so
the access-via-group path stays distinguishable in the audit log.

`shared_with` is the sharing primitive of the **smart-wiki** family
(a wiki marked `smart: true` in its `_meta.md`). It is
managed from the dashboard's `/wikis/<id>/sharing` route and is empty
for the vast majority of wikis.

---

## 5. Wiki visibility is *derived* — there is no wiki-level access gate

There is **no wiki-level ACL** above the block-level ACL of §2. A reader's
access to a region is decided **only** by `can_read` (§2); a whole wiki or
page is never gated as a unit. Visibility is instead a **derived** property:
a page is reachable by a reader iff it holds at least one fact that reader
`can_read`, and a wiki iff it holds at least one such page.

The derivation is computed once per turn as the **reader-relative card**
([`build_reader_card`](../../crates/mwe-core/src/meta_annotate.rs) →
`ReaderCard`): every `fact_index` row the reader `can_read` adds its wiki to
`ReaderCard::readable_wikis` (independent of whether the fact carries
`topics`), and its topics to the per-wiki / per-page card. The recall
navigator and the sender-scoped catalog (`wiki_catalog_list_for`) seed only
wikis in `readable_wikis` (and descend only into pages with a readable
topic), so a wiki the reader can read nothing in surfaces nowhere — with no
declared flag. The explicit reads enforce the same derivation at the wiki
level: `wiki_read` and the dashboard wiki view / list refuse (404 / filter
out) a wiki that holds facts the caller can read none of
([`fact_index::wiki_visible_to`](../../crates/mwe-core/src/fact_index.rs) — an
empty wiki hides nothing and stays visible; admin reveal bypasses), and
`wiki_search` returns only `can_read` rows — never a wiki-level render of
prose or structure the reader was not granted.

This is the load-bearing consequence of "ACL lives only in the fact": the
wiki and the page are **recall structure** — where a fact is filed, how it is
grouped, the prose around it — never an access boundary.

---

## Where this lives in the code

| Concern | Module |
|---|---|
| Identity validation, id rules, `groups_for`, mirror writer | [`crates/mwe-core/src/enrollment.rs`](../../crates/mwe-core/src/enrollment.rs) |
| `Principal` / `Acl` / marker types | [`crates/mwe-core/src/types.rs`](../../crates/mwe-core/src/types.rs) |
| The single ACL rule (`can_read`) | [`crates/mwe-core/src/acl.rs`](../../crates/mwe-core/src/acl.rs) |
| Per-sender redaction (`render_for_sender`) | [`crates/mwe-core/src/render.rs`](../../crates/mwe-core/src/render.rs) |
| `shared_with` read+notify resolution (`resolve_read_access`) | [`crates/mwe-core/src/wiki_admin.rs`](../../crates/mwe-core/src/wiki_admin.rs) |
| Derived wiki/page visibility (`build_reader_card` → `ReaderCard::readable_wikis`) | [`crates/mwe-core/src/meta_annotate.rs`](../../crates/mwe-core/src/meta_annotate.rs) |
| Setup wizard, account-vs-identity, identity-wiki creation | [`crates/mwe-dashboard/src/routes/setup.rs`](../../crates/mwe-dashboard/src/routes/setup.rs) |

Deeper mechanics, by design, live in the sibling design notes:
[redaction-policy](../design-notes/redaction-policy.md),
[marker-grammar](../design-notes/marker-grammar.md),
[setup-and-identity](../design-notes/setup-and-identity.md),
[jwt-and-session-model](../design-notes/jwt-and-session-model.md),
[agentic-chat](../design-notes/agentic-chat.md).
