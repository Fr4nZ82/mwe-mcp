# Groups

**Nav: Groups.** `/dashboard/groups`

A group is how several people legitimately share memory: a fact readable by
`group:famiglia` is readable by everyone in it, and stops being readable by
somebody you take out.

The table lists **Group id**, **Members**, **Scope** and **Actions**.

## Add a group

**+ Add group** asks for:

- **Group id** — lowercase letters and digits, starting with a letter; no
  underscore, no hyphen. It becomes the name of this group's memory wiki.
- **Scope** — a line of prose telling the classifier what this group is about
  (*"The household"*). It is a hint, not a rule.
- **Members** — tick every person in it.

**Save**, and the group exists. **edit** changes the scope and the membership.

**delete** asks for a confirmation and removes the group together with its
member list. Two things it does not do, and both are worth knowing first: the
group's own wiki stays behind, as a wiki nobody owns — [the Wikis
page](wikis.md) is where that is deleted — and a fact whose *also readable by*
names the group keeps naming it, so with the membership gone it is read by
nobody through that entry. Facts the group itself was recorded as having said
are handed to the wiki they sit in; in a topic wiki — one that grows around a
subject and belongs to nobody — there is no owner to hand them to, so they are
signed with the same anonymous author a forgotten person's facts get.

## The one group you did not create

`global (builtin)` is always there and holds everyone. The page states exactly
what it is for: general, public-domain facts that are true for everyone and
belong to no single user or group. It is **not** the way to make a personal
fact public — that is visibility (putting `global` in a fact's allow-list),
which is a different question from ownership. See
[Who can read what](../user/who-can-read-what.md).
