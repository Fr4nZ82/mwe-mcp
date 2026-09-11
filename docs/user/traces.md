# What was recalled for you

**Nav: Traces.** `/dashboard/recall-traces`

When your assistant answers you, the memory hands it what it found. This page
is the record of those recalls — the most recent first, and only your own. The
page's own empty state says when a row appears: as soon as a turn or a deep
search runs.

The table says **When**, **What ran**, **Who it was for**, **What was asked**,
how many facts were found and how, how many **steps** the search walked, **why
it stopped**, and how much text was handed over.

## Opening one

A trace opens into the whole recall, in the order the engine ran it. At the
top, the **replay**: an animated route of the same recall, phase by phase, with
transport controls (restart, back, play/pause, forward, speed). Every phase is
drawn from the record below it; nothing in it is invented. Below the replay,
the same recall as text:

- **What was asked**, when, which consumer asked it, and — when the engine
  filled in what the sentence left implicit — **The completed message**, with
  a line saying whether the facts answer that sentence or the words as
  written.
- **Read as** — what the engine took the message to be. **How deep** — whether
  the walk was allowed to run, or the consumer asked for a light recall and it
  was skipped. **Search started from** — the topics and the people it
  resolved.
- **The walk** — how many steps, why it stopped, how much prose it collected
  against its budget. **Time** — the recall alone, inside the whole turn.
- **Handed over whole** — the identity cards the consumer received in full;
  the walk never opens those pages.
- **Facts the search returned** — each with its score, the **seat** it took in
  the block (similarity, a seat kept for its macrotopic, or one fact of its
  kind), its kind, and what happened after the search: handed over, or dropped
  and why.
- **Not yet on a page** — captures still waiting to be filed. **Closing
  soon** — dated items, with their date. **Project notes** — documentation a
  coding assistant keeps, handed over as reference.
- **Doors the walk could start from** — the pages the search offered as a
  starting point, strongest first, with **Found by** saying why each one was
  offered: *similarity* (and then the fact that opened it is shown beside it),
  *a topic of the message*, *the situation the consumer described*, or *the
  page says it is about this* — which is the page's own one-line description
  matching what you asked, and the only reason that does not need a fact.
- **Each step**, with the navigator's reason, which pages were on offer, which
  it asked for, which opened and — for each refusal — why not.
- **Weighed against what was already there** — after the walk, the turn asks
  one question about the facts it has just read: does this message retire,
  replace, re-date or re-share any of them? The panel lists the facts that
  were put to it and shows the answer it gave, word for word. It is the only
  step that can retire something you had stored, so it is shown raw.
- **Changes it asked for and did not get** — replacing a stored fact and
  closing one both take it out of what the memory answers with, so every one is
  checked before it is carried out. This panel lists the ones that did not pass,
  which verb asked and why, and it appears only on a turn that had one. The fact
  that would have been taken away is exactly as it was.
- **Handed to the consumer** — the text that actually reached your assistant
  (a deep search shows **Answered to the caller** instead).
- **Standing rules handed over** — your standing rules as they went with it,
  in the field your assistant receives them in, kept apart from the memory
  above. The panel is there only on a turn that carried any.

## The replay

The stage tells the recall as a scene: the sentence types itself and, when the
engine completed it, rewrites itself with the filled-in words lit; the facts the
search returned rise as cards, each carrying its seat; fresh captures drift
above them because they have no page; dated items tick on a clock; the identity
cards are handed over and barred; the doors line up on four rails (similarity,
the page's own description, topic, situation); the navigator — an orb with an
eye — is shown a pool of cards each step, asks, is refused or reads, while the
prose budget drains on the left; and the block assembles band by band from those
sources, the dropped facts falling away with their reason. On a light turn the
navigator sleeps. Without WebGL the stage disappears and the text is the whole
record; with reduced motion it starts paused.

## Why this exists

It is the answer to *why did it say that*. If your assistant told you something
odd, the trace shows whether the memory handed it the wrong thing, handed it
nothing, or handed it the right thing and the assistant did the rest.

Traces are kept for 90 days and then swept, because they hold recalled memory
word for word.
