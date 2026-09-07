# Users

**Nav: Users.** `/dashboard/users`

One account per person. The table lists **User id**, **Email**, **Role**,
**Status** and **Actions**; the admin's own row carries no actions beyond
*edit* and says so.

## Add a person

**+ Add user** asks for:

- **User id** — lowercase letters and digits, starting with a letter; no
  underscore, no hyphen. It becomes the name of this person's memory wiki, so
  it is worth agreeing on before you type it.
- **Email** — required. The person signs in with it, and only you can change
  it later.
- **Aliases** — the other names this person answers to, separated by commas.
  The memory recognises somebody by their user id and by these names, and by
  nothing else: it holds no surnames and no display names, and it matches a
  name exactly. So a nickname, a full first name, whatever the household
  actually calls them all belong here — leave the field empty and any of those
  names, in a message or in a document somebody uploads, reaches nobody. The
  same exactness is what keeps somebody outside the memory from being taken for
  the person inside it: a name that merely resembles a user id is a different
  person, and what is said about them is kept under their own name instead.

  You are not the only one who fills this in. At their first sign-in the person
  is asked their full name and their nickname, and both are added to this list
  as they typed them — this field is where you correct them and add the rest. A
  name of several words counts as one name and is matched whole: "Frodo
  Baggins" reaches him, "Baggins" on its own does not. A name another enrolled
  person already answers to — their user id, or one of their aliases — is
  refused when you save, naming whose it is: a name reaches one person.
- **Timezone** (IANA, e.g. `Europe/Rome`) — times they speak, like *"tomorrow
  at 9"*, are read in this zone. Empty falls back to the deployment default on
  the [Settings](settings.md) page.
- **Language** (BCP-47, e.g. `en-GB` or `it`) — not only the language a
  consumer answers them in: **every page the engine compiles for them is
  written in it**. Empty falls back to English.

Press **Create user + invitation link**. The list comes back with a green line
carrying a **single-use link** — by default it expires in 24 hours. Hand it
over; the person opens it, picks their own password (minimum 12 characters)
and is signed in. You never see the password. Their row reads *invited* until
they do, and **reinvite** mints a fresh link if the first one lapses.

There is exactly one admin per deployment, so everyone created here is a
regular user. If you lose the admin account, the break-glass is the CLI:
`mwe-mcp admin-reset`.

## This form is for people

A **consumer** — the bot or assistant that talks to this memory — is not made
here. Issuing a standard consumer token on the [Tokens](tokens.md) page
creates its identity and its wiki, with no email and no login.

## Edit a person

**edit** opens their account: email, aliases (worth revisiting whenever the
household starts calling somebody something new), timezone, language, and
**Require two-factor authentication** for that one person. The user id and the
admin role cannot be changed — to give somebody a different id you forget them
and enrol them again.

The same page carries the two things a person can ask you for: **Export
everything about this person** and **Forget this person**. Both are on their
own page, [Export and forget](export-and-forget.md), because both deserve
reading before you press them.
