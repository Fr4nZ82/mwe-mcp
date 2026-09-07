# Skills

**Nav: Skills.** `/dashboard/skills`

A read-only catalog of the instructions this server hands to the consumers
that use it. A consumer fetches them itself — over MCP with `skill_list` /
`skill_fetch`, or over HTTP at `/skills` — so nothing here has to be
copy-pasted into anybody's prompt.

The table lists **Name**, **Version**, **Description** and **ETag**. Clicking
a name shows the skill's body exactly as a consumer receives it, frontmatter
included.

The skills ship inside the binary, so this page changes when the server is
upgraded and at no other time. It is worth opening once, to see what your
consumers have been told: the always-loaded `core` skill carries the wire
identity, the token lifetimes and the failure codes, and the rest describe how
each class of consumer is expected to behave.
