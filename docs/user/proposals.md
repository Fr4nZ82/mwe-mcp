# Proposals

**Nav: Proposals.** `/dashboard/proposals`

The memory does not only hold what you told it — it keeps rearranging how that
is filed. It makes a page for a subject nobody named, links one page to
another, merges two facts that said the same thing, closes a fact that stopped
being true, changes who may read one. **A proposal is the note it leaves each
time it does one of those**, and this page is the list of them, newest first.

Two of them are not a note but a question, and those wait for an answer. They
are marked **Pending**.

## What you see

Above the table, four tabs: **All** (where the page opens), **Pending**,
**Applied**, **Expired**. The table has six columns.

- **What** — the kind of change, in a couple of words. There are a dozen or so:
  *A page the memory made*, *A link between pages*, *A page of its own*, *A fact
  moved*, *Two pages merged*, *Two facts merged*, *Facts closed*, *Dates
  corrected*, *Sharing changed*, and the two that are questions — *Two answers
  to one question* and *A request to forget*.
- **What it says** — one sentence saying what happened, or what is being asked.
  It is written from the note itself, so the page needs no model to explain it
  to you and never calls one.
- **Who it is for** — the person the note concerns, or *nobody in particular*
  for the ones the nightly run leaves unaddressed.
- **Raised** — when it was written.
- **State** — **Pending** with the date an answer is due, **Applied** with the
  date and who applied it, or **Expired** with the date it was due.
- The last column opens it. A **Pending** row also offers **answer in chat**.

The fifty most recent are listed. Nothing here is ever swept, so the older ones
stay in the table behind the newest fifty.

## Opening one

The same sentence again, then what the note holds: which wiki and which page,
which facts, the reason the memory gave itself, and — for a question — **What
it asks**: the question word for word, the answers you can give, and which one
is marked *what happens if nobody answers*.

Nothing on this page changes anything. It reads.

## Answering one

Only a **Pending** row can be answered, and the answer is given in the chat:
**answer in chat** opens it with the proposal already summarised, and you say
there what you want. See [The chat](the-chat.md).

Two things arrive as a question.

**Two answers to one question.** Your identity card holds a handful of things
there can only be one of — when you were born, where you live, how to reach
you. When somebody states a different value for one of them and is not entitled
to overwrite it, nothing is written and the question comes to you, because the
card is yours whoever happened to state what is on it: the value on record, who
said it and when, the value that was said instead, and who said that. Answer
that the stored value still holds and what they said is dropped; answer that it
is wrong and the stored value stops being asserted and theirs takes its place.
**You have 24 hours.** Say nothing and the card keeps what it had — that is the
answer marked as the one that happens by itself, and it is deliberate: the
stored value was put there by somebody entitled to put it there.

**A request to forget a fact.** When somebody asks for a fact to be forgotten
and they are not the one who said it, everybody who can read that fact is asked
first. **The window is 7 days.** Silence lets the forget through; a majority of
*no* keeps the fact.

**Expired** means the deadline passed and the memory could not carry the answer
out — it keeps trying for a further 24 hours before giving up and marking it so.
Nothing was changed.

## Who sees what

You see the proposals **addressed to you**: the ones about your own facts, and
the ones about something you said. A note carries the material the change was
about — a fact in full, or the first 120 characters of one — so a note about
somebody else's fact is not yours to read, and you are not shown it.

Two cases put somebody else's words in front of you on purpose, because you
cannot answer without them.

- **A forget request you are voting on.** You are one of the people who can
  read the fact, which is why you are being asked, and the request names the
  fact so you know what you are voting about.
- **Two answers to one question.** The page shows both values, the one on
  record and the one that was said instead, with who said each. You are being
  asked which is right, and the question cannot be put without them.

When one turn closes several facts belonging to several people, the memory
writes **one note each**, and each carries only its own reader's facts. The
sentence somebody typed goes only to the person who typed it.

The admin also sees the ones addressed to nobody in particular, which is most
of what the nightly run writes. With **Admin reveal** turned on in Settings,
they see everybody's, and the page says so at the top while it is on.

## On an instance you are only looking at

On a read-only instance there is no chat, so no row offers **answer in chat**.
The list and each proposal still read normally: that is what the page is for
there.
