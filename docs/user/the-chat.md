# The chat

A panel on the right of every page, and its own page at `/dashboard/chat`.

It is an **operative surface**: it acts on the memory through the same tools an
assistant uses, and it does **not** capture or recall what you type into it.
Talking to the panel does not teach the memory anything about you. The
scrollback stays in your browser, and each message is a fresh turn.

Say what you want in plain language. Before anything is written, the chat asks
you to confirm.

## What people ask it

The **Help** button in the panel lists the five it is built for, with an
example each:

| You say | It does |
|---|---|
| *"what do you know about the car?"* | Searches the memory you are allowed to read. |
| *"that is wrong — I moved to Bologna in March"* | Replaces a fact with a corrected version. |
| *"forget everything I said about the old job"* | Lists the matching facts, then forgets the ones you confirm. |
| *"move this fact onto the health page"* | Moves one fact to another page or wiki (admin only). |
| *"read aloud every reply you send to Bob"* | Sets a standing rule for one person (admin only). |
| *"what have I got pending?"* | Reviews what is still waiting on you, and casts your vote on a forget request. |

### Standing rules for somebody else

A standing rule is how you tell an assistant to behave — *keep it short with
me*, *call me Franz*. You set your own simply by saying them in an ordinary
conversation, and they stick.

A rule about **somebody else** is different, and this chat is the only place it
can be set. Said in an ordinary conversation — *«read aloud every reply you send
to Bob»* — it is refused and nothing is stored, whoever says it, an
administrator included. Two things follow: Bob can always set his own by telling
an assistant himself, and an administrator can set one for him here.

When you set one here you say three things: **who** it is about, **how far it
reaches**, and **the rule itself**, written as an instruction naming the person
— *"Read aloud every reply you send to Bob."*, not *"read aloud my replies"*,
because it is read back later by an assistant that was never in this
conversation. How far it reaches is either **every assistant** Bob talks to,
which travels with him, or **one assistant**, which you name — this chat is not
one of his assistants, so there is no *this one* for it to assume. Setting a
rule for every assistant that Bob already has on one of them **moves** it: the
copy on that assistant is retired, so it never ends up reading the same
instruction twice.

**Taking one back.** Bob can drop a rule about himself the way he set it: by
telling an assistant so. You cannot, because a rule you set for Bob is filed as
his and is never shown to your own assistant — ask it in an ordinary
conversation and it answers that no rule in force matches. Remove it from this
chat instead: ask for Bob's rules, then forget the one you mean. There is no
edit — to change a rule, set the new one and forget the old.

## Things waiting on you

When something needs your answer, a badge lights up in the top bar. Clicking it
opens the chat on exactly those items — it is the same door as *Review pending
changes in the chat* on [your home page](your-home.md). To read them first,
with their deadlines and what happens if you say nothing, open
[Proposals](proposals.md); **answer in chat** there opens this same door on one
of them.

Two kinds of thing arrive there. A **change somebody made to a fact of yours**
comes with a summary of what happened. A **request to forget a fact you can
read, made by somebody who did not say it**, comes as a vote: you are one of
the people it was readable by, so you are asked.

## The one exception

The chat's own page warns about it: with JavaScript off, the box posts to the
page instead of the panel, and **that path behaves differently** — it runs your
message through the ordinary capture and recall, exactly as a message to an
assistant would be, and prints below what the engine made of it. So on that
path what you type *can* become a fact.

## When the memory is out of budget

If the deployment has spent its daily budget, the chat says so instead of
answering: the amount, the limit, and that paid model calls resume at midnight
UTC or when the operator raises the budget. Nothing is lost; it is a pause.
