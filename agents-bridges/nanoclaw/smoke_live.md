# Live smoke — nanoclaw bridge

Operator-run, against a real mwe-mcp server and a real model. It costs model
calls on both sides (the memory's `ingest` and `navigator` slots, and the
agent's own provider), so it is a deliberate act, not part of CI. A bridge is
not *functional* until this has passed once.

Twenty minutes, one throwaway workdir, one throwaway Telegram bot.

## Set up

1. A server on a **throwaway workdir**, not your own memory:

   ```bash
   mwe-mcp init --workdir /tmp/mwe-live && mwe-mcp serve --workdir /tmp/mwe-live
   ```

   Finish onboarding in the browser: all six model slots, then two users —
   `alice` and `bob` — and a **standard consumer** for the bot (`samnano`).
   Delegate `alice`, `bob` and **`guest`** to it, and mint its token.

2. A nanoclaw fork with the bridge installed (the README's steps), pointed at
   that server, with `senderMap` mapping your own Telegram id to `alice` and a
   second account's to `bob`.

3. A Telegram bot both accounts have written to at least once — a bot cannot
   cold-message somebody who has never written to it, so the reverse channel
   has nowhere to deliver until they do.

## The conversation

Everything below is typed into Telegram as `alice`, unless it says otherwise.
After each step, check what the memory did on the dashboard
(`/dashboard/wiki/alice`).

1. **It remembers.**
   - *"Il cane si chiama Frodo."*
   - Then, in a new message: *"Come si chiama il cane?"*
   - It must answer *Frodo* without being told again. Look at the dashboard:
     one fact, subject `alice`.

2. **It remembers what it said.**
   - *"Ricordami di chiamare il veterinario venerdì."*
   - The reply should confirm the commitment. On the dashboard, a fact whose
     author is the agent, not alice — that is the assistant-pass ingest.

3. **It knows who is speaking.** From the second account, as `bob`:
   - *"Io invece ho perso le chiavi."*
   - The fact lands in **bob's** wiki, not alice's. Then, as alice again,
     *"cosa ha perso Bob?"* — whether she is told depends on the ACL, and
     either answer is correct; what must not happen is the fact landing in the
     wrong wiki.

4. **A stranger is a guest.** From a third account, not in `senderMap`:
   - *"Ciao, chi sei?"*
   - It answers, helpfully and reservedly. It must **not** name alice or bob,
     and must **not** promise to remember. Nothing appears on the dashboard.
   - It must not offer a dashboard link.

5. **The window is real, in both directions.** As alice, three short messages
   in a row, then a question that only makes sense given the first one (*"e
   quello di prima?"*). It must follow — that is the recent window, not a
   session. Then a question that only makes sense given the agent's **own**
   last answer (*"e perché proprio quella?"*, right after it has recommended
   something). It must follow that too, and it must not ask which answer you
   mean: the window carries both halves of every turn, and a window holding
   your questions with none of its answers is exactly what that looks like from
   the chat.

   Then the same thing **without giving it a pause**: ask something, and the
   second the answer lands send the follow-up that depends on it (*"e quanti
   abitanti ha quella città?"*). It must follow that too. The memory is still
   working on the turn before it at that moment — a deep recall takes seconds —
   and the window is written when the reply is delivered, not when the memory
   has finished with it.

6. **A photo becomes memory.** Send a photo with a caption. The dashboard shows
   a described media fact; the described text should match the photo.

7. **The memory speaks first.** As alice: *"Dì a Bob che la visita è alle
   sei."* Within a minute or so, **bob's** chat receives a message from the
   agent carrying that content, saying it comes through alice. It must not read
   as though bob was in the conversation.

8. **A disambiguation.** Have two people of the same first name in memory, then
   mention that name ambiguously. The agent should ask which one; answer, and
   the fact must then land against the one you picked.

9. **Nothing was written to disk.** In the agent's group folder:

   ```bash
   ls groups/<folder>            # no memory/ , no conversations/
   ```

10. **A restart keeps the thread.** Restart the service mid-conversation and
    ask a follow-up that depends on the previous message. The window is on
    disk, so it must still follow.

## What counts as a pass

Every step above, plus: no error in the host log, and
`source setup/lib/install-slug.sh && journalctl --user -u $(systemd_unit) |
grep 'mwe_request failed'` empty for the whole run.

## Tear down

Revoke the consumer token in the dashboard, stop the server, delete
`/tmp/mwe-live`, and delete the bot.
