---
title: Enrollment validator — design notes for `mwe-core::enrollment`
area: design-notes
status: implemented
last_review: "2026-06-09"
---

# Enrollment validator — `mwe-core::enrollment`

`mwe-core::enrollment` is the identity validator and DB mirror behind the
dashboard's user/group CRUD. The id-rules and the hard-abort vs soft-warn
validation outcomes are canonical in
[`../concepts/identity-and-acl.md`](../concepts/identity-and-acl.md), and the
end-to-end token + session model in
[`jwt-and-session-model.md`](jwt-and-session-model.md); this page documents the
validation/mirror machinery itself.

Identity is owned by the dashboard: there is no enrollment YAML file on disk and
no reload tool. The form handlers build an `EnrollmentFile` from operator input in
memory and hand it straight to `validate` + `mirror_to_db`. The module's
`mod tests` ([`enrollment.rs`](../../crates/mwe-core/src/enrollment.rs)) is the
regression net for the validation rules.

## The validator + mirror flow

[`mwe-core::enrollment`](../../crates/mwe-core/src/enrollment.rs)
exposes:

```
EnrollmentFile { version, users[], groups[] }
   │
   ▼
validate(&file) -> ValidationReport
   │
   ▼
mirror_to_db(pool, &file) -> ()    (single sqlx::Transaction)
```

`validate` applies the id-rules and validation outcomes now documented
in [`../concepts/identity-and-acl.md §1.6`](../concepts/identity-and-acl.md).
`mirror_to_db` opens a single transaction, `DELETE`s both
`enrollment_users` / `enrollment_groups`, re-`INSERT`s every user and
group, and `COMMIT`s. Splitting the two means the dashboard CRUD
handlers can run validation up front and only flip the DB once it
passes, so a malformed admin submission never partially corrupts the
identity tables.

## Hard rules vs soft warnings

The full hard-abort vs. soft-warn outcome table, and the id regexes it
enforces, are **canonical in**
[`../concepts/identity-and-acl.md §1.6`](../concepts/identity-and-acl.md).
The loader-specific note is just how `validate` *carries* the two
outcomes:

- **Hard errors** short-circuit as `Result::Err(EnrollmentError::…)` —
  the first violation aborts and nothing is written.
- **Soft warnings** (alias collisions) accumulate in
  `ValidationReport::warnings: Vec<String>`, which is the success
  return value of `validate(...)`.

The dashboard CRUD handler renders each warning back to the operator
(yellow banner, "saved with N warnings") and proceeds with the mirror
write.

Both id checks run in pure Rust (no `regex` dependency) — see
`is_valid_user_id` and `is_valid_group_id` in the module. The
filesystem-safety check (`is_filesystem_safe`) catches `/`, `\`,
`..`, and whitespace before any DB or filesystem path is formed
from the id.

## DB layout — JSON columns for the variable-length pieces

The two mirror tables' DDL ships in `migrations/0006_enrollment.sql`
(catalogued, with the rest of the schema, in
[`engine-db-and-migrations.md`](engine-db-and-migrations.md)). Both
`aliases` and `members` are stored as JSON arrays in `TEXT` columns.
This was a deliberate trade-off — the same trade-off described
canonically in
[`../concepts/identity-and-acl.md §1.6.1`](../concepts/identity-and-acl.md):

- Pro: schema stays flat — no `enrollment_user_aliases` /
  `enrollment_group_members` junction tables to maintain in sync.
- Pro: queries that read the full row get aliases/members in one
  fetch without a JOIN.
- Con: SQL queries that filter by "users in group X" cannot use a
  conventional JOIN; they have to `json_each` the column or expand
  in application code.

The "con" does not bite the common path — every consumer of the row
reads the full record of a user it has already identified. The inverse
query ("**which groups does user X belong to?**") is answered by
[`groups_for(pool, user_id) -> Vec<String>`](../../crates/mwe-core/src/enrollment.rs):
every production construction site of `SenderContext` calls it so the
ACL matcher `Principal::Group(_)` finally has a non-empty haystack. The
implementation uses `EXISTS (SELECT 1 FROM json_each(members) WHERE
json_each.value = ?)`, the same JSON1 pattern `fact_index` already uses
for `topics_any`. If/when "all users in group X" is ever needed as a hot
SQL query, the answer is either a generated column or a junction-table
migration; with the dashboard tables as the source of truth, the
cost is real but only incurred on demand.

## Test surface

The `mod tests` block in
[`enrollment.rs`](../../crates/mwe-core/src/enrollment.rs) is the
regression net for the rules above. The load-bearing categories:

- **Validate happy path** on a canonical example built in Rust
  (two users, one group, aliases, an admin flag, a locale).
- **Hard rules**: `version != 1`, invalid user id (numeric prefix,
  uppercase), invalid group id (`°` rejected), duplicate user id,
  group↔user collision, dangling member, unsafe slug.
- **Soft rules**: alias collision warning (no error), `°` accepted
  in user ids.
- **DB writer**: round-trip of aliases (as JSON) and group members;
  atomic replace (apply v1 with two users + one group → apply v2 with
  one user → only v2 visible, group dropped); and the single-admin
  invariant — a second `is_admin = 1` row inserted *behind* the mirror
  writer is rejected by the `idx_single_admin` partial unique index
  (migration 0015), pinning that the DB, not the application code, is
  the last line of defense.
- **`groups_for` / `groups_with_scope_for`**: overlapping membership
  resolves to the alphabetical members-of relation, the scope column
  rides along as `Some`/`None`, and unknown / empty `user_id`
  short-circuit to empty without a JOIN-or-error surprise.
- **`locale_for`**: configured locale round-trips through
  `mirror_to_db`; unknown user, empty `user_id`, and a whitespace-only
  column all collapse to `None` so the prompt renderer falls back to
  the mirror clause.

The "atomic replace" test is the load-bearing one for the dashboard
CRUD behaviour — it proves the `DELETE` and the `INSERT`s share a
transaction. If the implementation ever drifts towards separate
statements, that test fails and tells you "saved with warnings" could
leave the mirror inconsistent.
