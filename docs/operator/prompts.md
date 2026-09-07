# Prompts

**Nav: Prompts.** `/dashboard/prompts`

Every system prompt the runtime uses is listed here, with its **Status vs
bundled default** and an **edit** link.

A prompt is loaded from `<workdir>/prompts/<name>.md` when that file exists,
and from the copy built into the binary otherwise. Editing one here writes the
workdir file atomically and keeps the previous content as `<name>.md.bak` — a
single slot, overwritten on every save. **Reset** drops the override and goes
back to the bundled text.

Some rows are not whole prompts but pieces appended to one in a named
situation, and the list says which: the nightly variant of the page compiler,
the ingest additions for a turn with attachments or one written by the
assistant.

Overriding a prompt is a real change to how the memory behaves, and it is the
change with the widest blast radius available from this dashboard. Two habits
make it survivable: read the bundled text before replacing it, and check the
[Dream console](dream.md) history afterwards to see what the next cycle made
of it.
