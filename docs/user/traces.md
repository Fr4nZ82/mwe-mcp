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

A trace opens into the whole walk:

- **What was asked**, when, and which consumer asked it.
- **Read as** — what the engine took the message to be.
- **Search started from** — what it read the message to be about: the topics
  and the people it resolved.
- **The walk** — how many steps, why it stopped, how much text it collected
  against its budget, and how long it took.
- **Found by similarity** — the fragments the search matched directly, each
  with a score and the page it came from.
- **Doors the walk could start from** — the pages the search offered as a
  starting point, strongest first.
- **Each step**, with the reason given for opening a page, which pages were on
  offer, which were opened and which were not.
- **Handed to the consumer** — the text that actually reached your assistant.

## Why this exists

It is the answer to *why did it say that*. If your assistant told you something
odd, the trace shows whether the memory handed it the wrong thing, handed it
nothing, or handed it the right thing and the assistant did the rest.

Traces are kept for 90 days and then swept, because they hold recalled memory
word for word.
