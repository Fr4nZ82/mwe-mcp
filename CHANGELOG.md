# Changelog

All notable changes to **mwe-mcp** will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

From 1.0, the public interface — the MCP tool surface, family by family, as
the dispatcher in `crates/mwe-mcp-server/src/mcp/` declares it — is a stable,
semver-governed surface: breaking changes are called out explicitly.

## Unreleased

### Fixed

- **A note about a move quotes only the comment its reader left.** A page can
  carry pending comments from several people, and one round of them can move
  several facts belonging to several owners. Every note written about those
  moves repeated the same line, built from **all** the comments on the page at
  once — so each owner read what the others had asked for. Each note now quotes
  its own reader's comments, and says only that a comment asked for the move
  when they wrote none.
- **A receipt goes to one person, and carries only that person's facts.** When
  one turn closed, re-dated or re-shared several facts at once — which happens
  whenever somebody tidies up things that were shared with them — the memory
  wrote a single note, addressed it to whoever the first fact belonged to, and
  put a hundred-and-twenty-character preview of **every** fact in it, plus the
  sentence that had been typed. So one person could read the opening of another
  person's fact, and what a third had said, in a note about their own. Now
  there is one note per person, each carrying only their own facts, and the
  sentence somebody typed reaches only them. The same held for the nightly
  passes, and holds no longer.
- **The people asked to vote on a forget request can now see it.** A request to
  forget somebody's fact is put to everybody who can read that fact, and the
  request itself was addressed only to whoever asked for the forget — so the
  voters were told to go and vote and found nothing on the **Proposals** page,
  nothing in the chat and nothing in the badge. Silence lets a forget through
  after seven days, so this was a vote nobody could cast. They see it now, and
  nobody else does.
- **The chat and the Proposals page answer "what may I see" the same way.** The
  chat handed any signed-in person the notes the nightly run addresses to
  nobody, which the page keeps for the admin, and the badge counted rows the
  page then withheld. All three read by one rule now.

- **A read-only deployment can no longer be made to call a model.** The freeze
  refused anything that would *change* something and let every read through on
  the strength of its method, and four reads were spending real model calls
  behind that rule: the two that open a proposal in the chat, the per-slot
  reachability probe on the **Health** page — six calls, which that page starts
  by itself as it paints, before anyone clicks anything — and the model listing
  the **LLM** page fetches to fill its picker. On an instance shown to
  strangers, with a passwordless door, that was somebody else's bill. Now every
  route that can reach a model is refused there whatever its method, and says
  which of the two reasons it was refused for. The pages that used to ask say
  so plainly instead of showing a spinner that turns into an error. **Nothing
  changes on an ordinary deployment.**

### Added

- **A card value nobody else may read can still be corrected, by the person it
  belongs to.** Your identity card holds a handful of things there can only be
  one of — when you were born, where you live, how to reach you — and when a
  conversation states a different value for one of them the memory stops and
  asks which is right. It could only ask somebody who was allowed to READ the
  value already on record, because asking quotes it back to them. So a value
  you had kept to yourself was invisible to that question, and a second one
  filed quietly beside it: two numbers on one card, with neither you nor the
  person who said it told anything at all.

  The memory now compares the two itself, without a model and without reading
  either value out. Nothing is written; **you** are asked, with both values,
  because you are the one person entitled to see them both; and the person who
  spoke is told only that what they said was not saved and that it has been
  passed to you — not what is on record, and not which detail it was. If one
  message does this about two people, both are named to them.

  It compares the **values** and not the sentences around them, so «Zoe can be
  reached on 07700 900314» and «Zoe's mobile number is 07700 900314.» are one
  number said twice and nobody is asked anything. Which detail a fact fills is
  named from a fixed list, so *birth date* and *birthday* cannot be two details
  that never meet — in a memory kept in two languages they would have been
  three. A card fact whose detail is not on that list records none and behaves
  as every fact behaved before the list existed.

  **Some of those details can honestly have more than one value** — a work
  email beside a personal one, a mobile beside a landline, a second job, a
  second nationality, two mother tongues — and a second value in one of them is
  not a disagreement. Where the person speaking can see what is on the card,
  nothing stops them and the assistant decides in the conversation. Where they
  cannot, the question still goes to you, with a **third answer**: keep yours,
  take theirs, or keep **both** — and on those details **both** is also what
  happens if you never answer, because throwing away something true costs more
  than carrying a line you can remove. On the details there can only be one of
  — when you were born, where you live, who your mother is — nothing changes,
  and silence still keeps what your card had.

  Where the speaker may read the value the question reaches them as before —
  and now it reaches them even when the classifier read the card and failed to
  say the two disagreed, on those details there can only be one of and where
  both values were written out plainly. Elsewhere the assistant decides, as it
  did, because it has both values in front of it and the memory does not.

  **Two people who disagree with the same card value are two questions**, each
  carrying its own value; one person repeating themselves is still one. And
  when a conversation is already asking you something else, a detail the
  memory notices by itself does not displace that question: it goes to the
  person whose card it is — and when that person is you, your assistant says
  so and points you at the page where the question is waiting, instead of
  telling you somebody else has been asked about your own record.

  Facts written before this release do not record which detail they fill, or
  what they put in it, so they take no part in the comparison until they are
  stated again.

- **A page for the proposals, so you can read them without asking the chat.**
  The memory keeps rearranging how your facts are filed — it makes a page for a
  subject nobody named, links one page to another, merges two facts that said
  the same thing, closes one that stopped being true, changes who may read one
  — and it writes a note each time. Until now those notes could only be reached
  by talking to the chat, which meant that on a **read-only instance**, where
  the chat is not there at all, the top bar showed a count with nothing behind
  it. There is now a **Proposals** page: the notes listed newest first, each
  with the kind of change in a couple of words, one sentence saying what
  happened, who it concerns, when, and its state — **Pending**, **Applied** or
  **Expired**. Tabs narrow it to one state, and opening a row shows what the
  note holds field by field, plus, when it is a question, the question word for
  word with the answer that happens if nobody gives one. **No model is called
  to render any of it**: every sentence is built from the stored note, which is
  what lets the page work on an instance that must never spend a call. Nothing
  on the page changes anything — a row that is still waiting carries a link
  that opens the chat on it, and that link is absent on a frozen instance,
  where nothing can be answered. **You see the proposals about your own facts
  and about what you said**, and nobody else's, because a note carries the
  material the change was about; the admin also sees the ones addressed to
  nobody in particular, and Admin reveal shows every recipient's. Reachable
  from the top bar, from your home page, from the count on the admin's home,
  and with its own page in the guide behind the **?** beside its title.
- **A search box, in the top bar, for the pages a word appears on.** There was
  no way to ask *where does this word appear* — you could read a page you had
  found some other way, or filter the facts table by who and when, but not by a
  word. Now every page carries a box: type a word, and back come the pages that
  carry it, grouped by wiki, each with the line the word appears in and a link
  that opens the page. A word finds the words that begin with it, so *pediatr*
  finds *pediatra*; capitals and accents make no difference, so *citta* finds
  *città*; give two words and a page comes back only if it carries both. It is
  a search for words and not a question: no model runs, and nothing is ranked
  or guessed. **What it searches is what you are allowed to read** — on a
  standard wiki the facts you may read, one of which has to carry the words for
  its page to appear; on a smart wiki, the pages of a wiki you may open at all.
  Two people searching the same word get two different answers, and neither
  learns that the other's page exists. The screen has its own page in the
  guide, reachable from the **?** beside its title.

### Changed

- **A second birth date is a question now, not a second line on the card.**
  An identity card holds a handful of things there can only be one of: when
  somebody was born, where they live, how to reach them. When a conversation
  stated a *different* value for one of them, the memory wrote it down beside
  the one already there and nothing downstream noticed — a child's card ended
  up carrying two birth dates, five years apart, both live. Now nothing is
  written: the assistant asks which of the two is right, in the same turn,
  quoting the value on record with who said it and when, and stores only after
  the answer. Keep it and nothing changes; replace it and the old value is
  retired, pointing at the new one. If the person asking is not entitled to
  rewrite that value — it is somebody else's, and they neither said it nor are
  it — the question goes to **the person whose card it is**, whoever happened
  to state what is on it, as a proposal waiting in the dashboard. What they
  said is held aside meanwhile, read by nothing: the owner saying *it is wrong*
  retires the stored value and releases those words onto the card, recorded as
  theirs; the owner keeping their own value drops them, and so does the
  deadline passing. Until then the card keeps what it had. The same detail is
  asked about once, however many times somebody restates it. No model ever
  chooses between the two, and no value is lost in silence. The engine also
  stops turning an age into a birth date: *«he is about 14 in July»* is stored
  as an age, not converted into a day nobody said.
- **Traces shows what the last step of a turn was asked, and what it answered.**
  One call at the end of every turn decides whether the message retires,
  replaces, re-dates or re-shares something already stored — the only call that
  can take a fact away — and it was the one step of the route that left no
  record. The **Traces** page now shows the facts it was given and its answer,
  word for word.
- **Traces shows only the recalls a person was actually given.** When an
  assistant answers, its reply is sent back to the memory so that the memory
  keeps the assistant's half of the conversation as well as yours. That second
  call was treated like any other turn: it searched, it put together a block of
  recalled memory, and it left a row on the **Traces** page beside your own —
  two rows per exchange, the second of them a recall nobody ever read, since
  the reply had already reached you and there was nothing left to hand it to.
  A turn the assistant feeds back is now read for what is worth storing and
  nothing else: no block is put together for it, no row is written, and the
  page-by-page search it used to walk for no reader does not run. What gets
  remembered out of the assistant's replies is exactly what it was, and a turn
  of your own is untouched.

### Fixed

- **A rule you set the assistant cannot be wiped out by an unrelated remark.**
  A standing instruction — *«keep it short with me»*, *«never bring up my
  mother's health»* — lives apart from ordinary facts, and every part of the
  engine leaves it alone except the one step that decides what a message
  retires. That step was being shown the rules alongside everything else, so a
  passing sentence about dinner, two days later and from the same person, could
  be recorded as having replaced the instruction: the rule stopped applying,
  the page it lived on emptied that night, and the later sentence meant to
  widen it had nothing left to widen.

  **What you can do to an instruction, from now on, you do by talking about
  the instruction.** Take one back by saying so — *«forget the rule about
  keeping it short»* — and it stops applying and its page says when it stopped,
  so what is written there is still the list of what binds the assistant. It
  has to be yours to take back: an instruction belongs to whoever gave it, and
  one that applies to everybody on an assistant belongs to the administrator,
  who is the only person who can lift it. Where it cannot be done you are told
  which of the two it is, and told plainly when nothing matched what you named,
  instead of hearing "done" and finding the assistant still obeying it. Change
  one by giving another in its place, as before. Nothing else reaches it: a
  sentence about something else cannot end it, cannot re-date it, and cannot
  change who it applies to, whatever else that sentence is about. Who an
  instruction binds was settled when you gave it — this assistant, everyone on
  it, or every assistant you talk to — and that is not something a later
  message adjusts by accident.

- **Telling the assistant how you want to be treated is a rule now, not a note
  on your card.** *«I'd like the summaries as a voice message»*, *«answers for
  me have to be short»* — phrased that way, those were being filed as a taste
  of yours, a line on your identity card next to your phone number and what you
  do not eat. To a person the two read alike; they are not alike. Nobody can
  act on a taste, so an instruction filed as one was never handed to the
  assistant as an instruction, could not be changed by giving another in its
  place, and did not go away when you asked it to stop. The dividing line is
  whether the sentence only comes true if the assistant behaves differently. If
  it does, it is an instruction. If it stays true with no assistant in the room
  — *«I prefer tea»*, *«I avoid the oven in hot weather»* — it is a fact about
  you, and nothing about it changes.

  **And how you phrase it decides who it binds.** Say it to the assistant in
  front of you — *«answer me in audio»*, *«call me Franz»* — and it stays with
  that assistant, the one you were speaking to. Say it about yourself, without
  telling anyone in particular to do it — *«I'd like the answers in audio»* —
  and it is yours: it travels with you, and every assistant you talk to applies
  it. Until now an instruction stayed with whichever assistant happened to hear
  it unless you spelled out «every assistant», which is not how anybody speaks.
  Drawing the line yourself still works and still wins: *«qui le risposte le
  voglio brevi»* stays with the assistant you said it to. Widening one **moves**
  it rather than adding a second: the narrower copy sitting on the assistant
  you first said it to is retired, so no assistant ends up reading the same
  instruction twice. That holds whichever road the widening came by — your own
  words, or an administrator setting it for you from the operator chat.

  **You cannot set a rule about how somebody else is treated by talking to the
  assistant, and now you are told so.** *«Leggi a voce alta tutte le risposte
  che mandi a Bob»* is a rule about Bob, and no conversation sets one —
  instructions are filed under whoever gives them, so this would have ended up
  applying to you and never to Bob. It used to be read as a rule for everybody
  and refused with *«only the administrator can do that»*, which was an answer
  about a rule nobody had asked for. Now the answer is the true one: a rule
  about another person is not yours to set, Bob saying it about himself works
  as it always did, and an administrator can set one for him from the
  dashboard's operator chat.

  **And that chat can now do it.** *"Read aloud every reply you send to Bob"*,
  said to the operator chat by an administrator, sets a standing rule for one
  person: you say who it is about, whether it reaches every assistant they talk
  to or one you name, and the rule itself. It is the only road there is, which
  is why it exists. Written down in the guide, under **The chat**.

- **A message that goes wrong in two ways is now reported in both.** One
  message can lose an item off a list *and* say something about somebody
  else's card, and only the first of those reached you: the other went
  unmentioned, so an item you believed was on your list was not there. Every
  one of these notices is now said, one sentence each.
- **Four kinds of sentence the memory was letting go past it.** A rule you lay
  down for the assistant (*«keep it short with me»*), a preference or a
  position you take in a discussion that is still open (*«I'd take the hybrid
  instead»*), taking back something you said yourself in favour of somebody
  else's version (*«you were right, close mine»*), and a plain statement about
  somebody the memory has never heard of (*«Sam is taking over the
  releases»*) were all read as small talk, or — the last one — as a question
  about a name, and nothing was kept. Each of the four is now named for what
  it is, so the rule is filed where the assistant reads it, the two opposing
  positions stand side by side with who said each, the withdrawn claim is
  closed, and the person outside the address book gets their fact. The
  sentences that must still pass without a trace pass unchanged: a thank-you,
  an instruction meant for this reply only, and a question about a name.

- **A fact about a pet, a tradesman or anyone else without an account.** When
  the memory was told something about somebody it has no account for, it
  occasionally garbled which field held the person's name and dropped the
  whole sentence. It is now shown the two fields filled in side by side, so
  the name of the thing goes where names go and the fact is kept. When a
  message does carry a claim the memory cannot read, only that one claim is
  dropped and the rest of the message is still saved — and the line in the log
  now says which sentence went, instead of only that one did.

- **A fact the conversation overtook stays open no longer.** When a message
  replaced something already on file and the memory had no new sentence to
  point at as the replacement, it described what happened with a word it then
  refused to accept from itself, and the old fact was left standing as though
  nothing had happened. It is now read as what it means — overtaken by what
  was said — and closed, with the date it stopped holding.

- **The keywords on a page are written in the language of the memory.** Every
  fact is filed under two words, and a page prints them among its keywords.
  The instruction that says which language to write in named the replies and
  the machinery and left the short labels between the two, so a stray word in
  the wrong language could slip in — and then stay, because the next fact about
  the same thing is deliberately filed under a word already in use. One memory
  ended up with the same idea under two words in two languages, which is
  exactly what the second word is meant to prevent. The instruction now covers
  every word written for a person to read, labels included. Nothing changes for
  a memory whose language was already being followed: the language is still the
  one on the person's account, set where it always was.

- **A page no longer looks as though it repeats its own opening.** When the
  writer leaves a fact out of the prose, the engine adds it back at the end so
  that no fact loses the marker that governs who may read it. Added with no
  separation it read as the closing paragraph the writer chose — and where the
  writer had already said the same thing in its own words, the page appeared
  to say it twice on purpose. What the engine adds now sits below a horizontal
  rule, plainly not part of the narrative, and the writer is told what leaving
  a fact untagged costs.

- **A new release shows its new dashboard at once.** The stylesheet and the
  scripts the dashboard embeds are asked for at an address that carries a
  fingerprint of the file's own content, so a build that changes one of them
  changes its address too: the browser has no copy of it and fetches the new
  file at once, while a file that did not change keeps the address it had and
  is read from the copy already on the machine. The dashboard now also tells
  browsers they may keep any of these files for a year, which is safe exactly
  because a changed file arrives under a different address.

- **The ready-made assistant remembers what it just answered.** On the nanoclaw
  bridge the agent's own reply was read back out of the text the provider hands
  over when a turn finishes, and there are two everyday turns where that text
  does not carry it: one where the reply goes to the person while the agent is
  still working and is not said again at the end, and one where the first
  answer comes back without the wrapper that sends it, so the bridge asks for
  it again and the real answer arrives after the turn had been counted as over.
  In both, the person got their answer and nothing else did: it was not stored
  as the agent's half of the exchange and it never entered the recent window,
  so the next message found the question on file with no answer beside it and
  the assistant asked again what it had just been told. The reply is now taken
  from the delivery itself, however the turn sent it, and a turn cut short by a
  follow-up before it formally ends keeps what it had already said.

- **And it keeps the thread when the memory is slow or unreachable.** Two
  messages eight seconds apart — *"what is the capital of Norway?"*, then *"and
  how many people live there?"* — still came back as *"I'm not sure which city
  you mean"*. Two things on the nanoclaw bridge were waiting on the memory that
  had no business waiting on it. The reply the person had just been handed
  entered the recent conversation only once the memory had been told about it,
  and a memory that takes seconds over a turn is a memory the next message
  arrives ahead of, with the answer not yet there. And a turn the memory did
  not answer in time went to the model with no recent conversation at all,
  although the bridge keeps that conversation on its own side and had it. Now
  the reply enters the recent conversation the moment the person receives it,
  and the conversation goes in front of every turn whether the memory answered
  or not — the memory is told afterwards and may take as long as it needs.

- **A link to a page takes you to that page, even when you have to sign in
  first.** Following a link into a page of a memory you are not signed in for
  put you on the sign-in screen, which is right, and then — whichever way you
  signed in — on the panel home, which is not: the page you had asked for was
  forgotten between the two, and nothing on the screen said so. It now travels
  with you, so signing in finishes the journey the link started. On a shown
  instance, where signing in is one click on *Enter as…*, the same is true of
  the buttons: they carry the page, and the visitor lands on it as the person
  they picked. This is what makes a link into a demonstration — from a tour, a
  README, a post — a link to what it promised rather than to a panel that
  explains nothing.

## 2.1.0 — 2026-09-08

**Traces** now tells the recall the way the engine runs it: the question the
memory actually answered, the seat every fact took in the block and why some
were dropped, the fact that opened each door, the reason for every page the
walk was refused, and the recall's own time inside the turn — with the replay
above the text following the same order, phase by phase.

The server also answers a machine. `GET /health` gives a watchdog one parsable
line and asks for no credential, `GET /metrics` gives your own monitoring a
Prometheus scrape behind an admin token, and **Format: json** in the Logging
block turns every log line into one JSON object a shipper can index.

And four things whoever writes a consumer will notice: a note is left where a
fact is read, so the topic wikis nobody owns take one at last; a deleted
group's name stops sitting on the facts it said in one; naming a thing the
memory already knows — the greenhouse, the car — files the fact under
whoever answers for it, on the conversation road and the uploaded-document
road alike; and `metadata.recall: "light"` lets a single turn on a channel
that is waiting to speak skip the navigation step.

### Added

- **Logs a shipper can ingest.** `logging.format` in `mwe-mcp.config.yaml`, and
  a **Format** choice in the Logging block of **Server settings**, switches
  every log line from prose to one JSON object per line: the timestamp, the
  level, the module and each structured field as a field, instead of a sentence
  to match a regex against. It applies to **both** places the logs come out, the
  file under `logs/` and standard error — which on a systemd host is what
  `journalctl` shows — so the two cannot disagree about their own shape. The
  default is `text` and nothing changes for an installation that leaves it
  alone; a misspelt value is refused at boot by name rather than read as the
  default.

- **The numbers your own monitoring can graph.** `GET /metrics` serves a
  Prometheus scrape of what this deployment is doing: turns and tool calls and
  failures per credential, model calls and errors and latency and tokens per
  slot, today's spend against the budget, how long recall is taking, when each
  of the Dream console's runs last finished and whether it worked, the database
  size and the uptime.
  It is **never public** — it takes an admin token, the same credential family
  `/mcp` uses, issued and revoked from **Tokens**. Every number is read from a
  table at scrape time, so no model is called and a scrape can never spend
  money. The volume figures are scoped to the UTC day and say so in their
  names, because the tables they come from are pruned and a total that silently
  drops is worse than no total.
  Documented metric by metric in the guide, under **Watching it from outside**.

- **A one-line answer to "is it up?", for whatever is watching.** `GET /health`
  at the root of the server's address answers `200 {"status":"ok"}` when the
  process is serving and its database is reachable, and `503
  {"status":"degraded","detail":"database unreachable"}` when it is not — so a
  reverse proxy, a container orchestrator or a cron `curl` can decide without
  a credential. It is deliberately mute about the deployment: no version, no
  path, no model slot, nothing cached. The Health console at **Admin →
  Health** is unchanged and remains where a person looks.

- **The Traces page tells the recall the way the engine runs it.** A trace now
  opens with the completed message the engine answered (and says whether the
  facts answer that sentence or the words as written), how deep the turn was
  allowed to look, the identity cards handed over whole, the seat every fact
  took in the block and why some were dropped, the fact that opened each door,
  the reason for every page the walk was refused, the date of each item
  closing soon, the project notes, and the recall's own time inside the turn.
  The replay above the text follows the same order — the sentence rewrites
  itself, the facts rise with their seats, the fresh captures drift without a
  page, the clock ticks, the cards are barred, the doors line up on three
  rails, the navigator is shown its pool each step and is refused with a
  reason, the prose budget drains as it reads, and the block assembles band by
  band while the dropped facts fall away — and sleeps through a light turn.

- **The release archive carries the guide.** Every published archive
  (`mwe-mcp-<tag>-<target>.tar.gz`, `.zip` on Windows) unpacks with `docs/`
  beside the binary, so an operator who downloads a release has the same
  pages the dashboard serves under **Guide** without a network connection.

- **A turn can ask for a shallower recall, for a channel that is waiting to
  speak.** Every turn walks the memory pages the conversation points at, and
  that walk is a second model call and the seconds it takes. A consumer on a
  latency-bound surface — a voice satellite in a room — now sends
  `metadata.recall: "light"` on `wiki_ingest_message` and the server skips
  exactly that step: no walk, no **NAVIGATED PAGES** in the recall block, and
  nothing else about the turn different. It is a per-turn choice, so the same
  consumer keeps the deep answer on its text channel; the default, and every
  turn that says nothing, is the full depth. An unrecognised value is
  refused rather than read as the default. The ready-made bridges do not send
  it — a consumer opts in.

### Changed

- **A note can reach a wiki nobody owns.** `wiki_admin_notify` decided who
  may leave an item by asking who owns the target wiki. A **topic wiki** —
  the kind the nightly grouping raises around a subject — is owned by
  nobody, so the question refused everybody and no consumer could leave a
  note in one. The gate is now the memory itself: **a note is left where a
  fact is read**, asked of each family in its own words. On a standard wiki
  that is whoever can read at least one fact in it — a wiki with nothing in
  it yet has no such fact and takes no note, even though it still *reads* as
  visible to everyone, which is what keeps a brand-new wiki from vanishing
  for its own subject. On a smart wiki, which holds no facts at all, it
  stays the wiki's owner, the members of its owning group, and whoever
  `shared_with` names. Who may reach which wiki at all is unchanged: an
  ordinary conversational consumer still records into a standard wiki
  through `wiki_ingest_message` and is refused here. So the knock-on effect
  is a smart consumer's: it may now leave a note in another person's wiki
  when it can read a fact there.

- **Deleting a group no longer leaves its name on the facts it said in a
  wiki nobody owns.** When a group is deleted, the facts recorded as said by
  it are handed to whoever the wiki they sit in belongs to. A **topic wiki**
  belongs to nobody, so there was nobody to hand them to and they kept the
  name of a group that no longer exists. Those facts are now signed with the
  same anonymous author a forgotten person's facts carry: the fact still says
  it was said by somebody, and that somebody is nobody. Facts in a person's
  or a group's wiki are unaffected.

### Fixed

- **Naming a thing the memory already knows names whoever answers for it.**
  The memory keeps a list of the named things it holds facts about — a
  greenhouse, a car, a relative who does not use the product — and who
  answers for each, so that the answer is given once and reused. The guard
  that stops a fact landing on the wrong person's card did not read that
  list: it asked only whether the words spelled somebody's own name, so a
  fact about the greenhouse, correctly filed under the person who answers
  for it, was handed back to whoever spoke or uploaded the file. Writing the
  thing's name now counts as naming that person, on both roads — a
  conversation and an uploaded document — and the fact stays where the rest
  of that thing's history is.

## 2.0.0 — 2026-09-07

The dashboard is rewritten, in what it says and in what it does. A person can
ask for everything the memory holds about them, and for its removal. Spending
has a daily ceiling that stops it, and **Admin → Usage** is where the day is
read against that ceiling and where the price list is edited. Linux, macOS and
Windows are all the gate, each with a way to run the server under an account
nobody logs in with. And a memory now ships with an assistant to talk to,
installed with one command.

Underneath, the memory learned to say what a fact is *about* when that is not
a person, to link its pages on purpose, and to open the pages of the topic
wikis its own nightly grouping raises, which every gate had been refusing. Ten
migrations, `0068` through `0077`, and three breaking changes on the tool
surface — plus a fourth that behaves like one: an unknown argument is refused
instead of dropped.

Everything a person sees on the dashboard is written down in the guide,
[`docs/`](docs/): one half for the operator who installs and runs the server,
one for whoever the memory is about.

### Added

- **A daily budget that warns you, then stops the spending.** Set
  `budget.daily_limit` — in the currency of your price list, from **Admin →
  Usage** or in `mwe-mcp.config.yaml` — and the deployment tells you once, on
  the dashboard and on the reverse channel, when the day passes
  `budget.warn_at_percent` of it (80% unless you say otherwise). At the budget
  itself, **paid model calls stop** until 00:00 UTC, or until you raise the
  budget or press *Unlock for today*. Omit the key and there is no budget, which
  is what every existing deployment gets on upgrade.

  A stop is a spending decision, not a way of running without a model: all six
  model roles stay required. User turns keep answering — they degrade to
  `intent=skip` with the canned seed and say nothing was saved, exactly as they
  do for an unreachable model — and the nightly cycle skips its round and says
  why. The budget covers metered calls only: a role on a flat subscription or on
  a model running on your own machine never moves it and is never stopped.
  Health probes are never refused either, so a stopped deployment does not read
  as a broken one.

  New event kind on `events_poll`: **`budget_threshold_reached`**, an operator
  notice at most once per threshold per UTC day, carrying `threshold` (`warn` |
  `stop`), `stopped` (whether paid calls are actually being refused right now),
  the `day`, `spent`, `limit`, `percent`, `currency`, the count of
  `unpriced_calls` the estimate leaves out, and a `dashboard_path`.

- **The Usage page is now the spend page, and the price list is edited on it.**
  **Admin → Usage** opens with today against the budget — the figure, the
  budget line, the percentage — and then the history it already showed. The
  price list is a form on that page instead of a block of YAML to copy: rates
  per 1M tokens, your own currency, `prefix*` wildcards, saved into
  `mwe-mcp.config.yaml` and in force on the next model call without a restart.
  Tables are headed in words rather than in field names, and every slot is named
  as the LLM config editor names it.

- **Embedding calls are counted.** An embedder reached over the wire (the
  `ollama` backend) now writes one row per request into the usage ledger, under
  a slot of its own, and the page shows it beside the six model roles. Its token
  columns read *not reported* rather than zero, because an embedding endpoint
  reports no token counts and a summed zero would read as "this was free". The
  bundled embedder runs inside the process on your own machine and is not
  recorded: there is no request to count and no bill to explain.

- **A person can ask for everything the memory holds about them, and for its
  removal.** Two admin actions on the user's page. **Export** builds a tar
  archive with their wiki (self-describing full markers), a markdown file of
  the facts **other people's wikis** hold about them — each with where it is
  filed, who said it and when — the files they uploaded, and their card.
  **Forget** erases them, and it is now the only way the dashboard removes a
  person.

  Forgetting is not deleting every sentence with their name in it. What they
  said about themselves goes with their wiki, and unlike every other
  retirement it leaves no tombstone — a retired row keeps its claim text, and
  the claim text is the thing being erased. What **somebody else** said
  about them is that person's memory and survives: the fact changes hands —
  the speaker becomes the principal that answers for it — and the forgotten
  person stays on it as a plain name in `subject_external`, which grants
  nothing and addresses nobody; a fact filed in their wiki moves into the
  speaker's, onto a page named after them. What **they** said about other
  people stays exactly where it is, with the author replaced by
  `user:_removed`, an id no account can ever hold. Behaviour rules about them
  are the exception and are destroyed whoever wrote them: a rule is an
  instruction, and handed to its author it would become an instruction about
  the author. The audit trail keeps what happened and loses who did it. No
  copy is kept — the wiki is erased where it stands rather than moved to the
  trash, and any subtree of theirs an earlier delete left in the trash is
  erased with it.

  **Two copies live outside the memory, and the erasure treats them
  differently.** The training spool, when it is switched on, records the
  whole prompt of every model call, and a prompt carries the recalled memory
  word for word — so it holds the person, with no column saying whose. There
  is no honest way to take out their lines and leave the rest (a prompt can
  describe somebody without ever spelling their id), so the spool is emptied
  whole, everybody's training pairs included, and the erasure reports how
  many files it emptied. A **snapshot** is the copy nothing can reach:
  restoring one brings the person back with everything else. The confirmation
  page says so before you press and links the backup console, because
  deleting an old snapshot or keeping it knowingly is the operator's call.

  **The id is spent afterwards.** What survives the erasure still names the
  person — a fact handed to another speaker keeps their name, and so does a
  page somebody else wrote — so an account created under the same id would
  inherit all of it, and the erasure would have moved the material instead of
  removing it. Every path that creates a user refuses an erased id and says
  why; the same human coming back gets a different one. Migration `0077`
  holds that list, and it holds nothing but the id and the date.

- **A fact can say what it is *about* when that is not a person.** The three
  governance axes are all principals, so none of them could hold the dog, the
  car, or a relative who does not use the product. A fact now carries the
  plain name of its subject beside the principal that answers for it
  (`subject_external`, migration `0075`), which makes the second fact about a
  name a lookup instead of a fresh judgement and makes a name matchable
  against a turn's text literally. `subject_id` keeps every job it had,
  existing rows are `NULL`, and the export marker gains `external=`.

- **Pages are linked on purpose, and the sentence a link sits in finds the
  facts beside it** (migration `0074`). The link graph was built from a field
  nothing ever wrote, so it was empty on every build; it is harvested from
  each page's own prose now, and **directed** — the page a link points at owes
  nothing back. The night nominates the links the corpus is missing, six a
  page, and may retire one, while the hourly build defends every link the
  prose carries, a hand-written one included. The clause each `[[wikilink]]`
  sits in is indexed and stands in for the facts beside it, reaching a fact
  through words it does not contain.

- **Recall asks the question the turn was actually asking, and an identity
  card says who a person is *now*.** "his kidneys have got worse" was searched
  for as written — no name in it, so the search looked for facts about kidneys
  belonging to nobody. The classifier has read the previous exchange, so it
  rewrites the message with the implicit filled in, inside the reply it was
  already composing, and the engine searches again on that at no extra call. A
  card, meanwhile, admits only what the classifier reserved for it, sheds onto
  ordinary pages rather than being cut at the read path, and can no longer be
  split away from the turn.

- **The night can move a page to another wiki, and it ends with an empty
  queue.** A page joined a wiki when it was born and nothing could undo that.
  Once a cycle the strong model is shown the whole forest — every standard
  wiki, whose it is, and per page its card, fact count and the principals its
  facts are about — and asked where each belongs; a smart wiki is never a
  destination. New `rem:` key `structure_review_cap` (default 3). And
  **`mwe-mcp reindex`** runs the safety-net sweep once, on demand, for a
  workdir restored from a backup or edited with the server down.

- **A memory now ships with an assistant to talk to, and the server installs
  it.** Every consumer the catalog covered was one the operator had to bring —
  a Hermes they already ran, or a Claude Code subscription. The **NanoClaw**
  bridge is an assistant the server installs itself: an `mwe` agent template
  whose persona teaches that recall and capture are mechanical and that there
  is no file on disk to keep a second copy in, plus an `add-mwe-memory` fork
  skill that makes it true — one ingest per conversational turn, the recall
  block in front of the turn, act-as per speaker and `guest` for anyone
  unmapped, media out of band, and a reverse channel that delivers a notice to
  its own recipient's chat. NanoClaw's built-in memory stands down for a group
  carrying the plugin and no session is carried between turns, so the memory,
  not a transcript, decides what the agent remembers.

  The dashboard serves it at **`/bridges/nanoclaw`** as the first consumer it
  recommends, and it is **one command**: `curl … | sh` clones NanoClaw at the
  tested ref when the operator has none, places the template and the skill,
  drives NanoClaw's own setup with everything it can answer already answered,
  stamps the agent, applies the memory, installs Telegram, wires the operator's
  chat without the pairing code, restarts both halves and then watches for the
  first message to confirm the turn was stored and recalled. What is left to
  the person is what no machine can do for them: **the bot token @BotFather
  gave them, their own Telegram id**, and NanoClaw's own two questions —
  “Standard setup”, and the Claude subscription sign-in that opens their
  browser. It needs a terminal, and it says so instead of hanging when there
  is none. Re-running the whole command is how an install is updated.

  **The token comes with it, if the admin wants.** *Bridges → NanoClaw* mints
  an **install claim**: a code that is good once and for fifteen minutes, that
  rides the command as `?claim=…`, and that the installer trades in its first
  seconds — at `POST /bridges/nanoclaw/claim` — for a standard consumer token
  it writes straight into the fork's `.env`. The token is never shown on the
  page, never printed, never on a command line. The claim is not the token: it
  expires, it is burned through the same `jti` blacklist as a single-use
  dashboard link, and a replay is refused. The consumer starts delegated to
  everybody enrolled plus `guest`, which the installer and the page both say
  to narrow on the Tokens page. Run the same command without a claim and
  everything else still happens; the token is the one step it hands back.

  There is no PowerShell installer, because NanoClaw runs on Windows inside
  WSL2 and the Windows path is the same command in a WSL2 shell.

- **The per-turn contract names the two governance blocks a turn can carry.**
  `INTEGRATING.md` documents `pending_votes` (the speaker owes a vote on a
  request to forget a fact they are part of — the requests, the deadline past
  which silence is consent, and the dashboard path where the vote is cast) and
  `document_promoted` (a paste long enough to be a document was archived
  verbatim on the media rail and queued for reading). Both are injected beside
  `rules`, both are absent rather than null when they do not apply, and neither
  is ever carried by a guest turn.

- **The memory can be asked what it holds about one person, and that is what
  the fact browser opens on.** The browser could narrow by the wiki a fact is
  filed in and by nothing that identifies a person — different sets, because a
  fact about somebody lives wherever it was filed, often on a group's page,
  while their own wiki holds facts about other people. There is an **About**
  field beside Wiki now, and for a person opening Facts from the top bar it is
  filled in with them: the question people arrive with — what does this thing
  know about me — is answered before they ask it, with **Everything I can
  read** one click away and carried through the pager. An admin opens on the
  deployment, as the console always did.

- **The guide, in [`docs/`](docs/).** Every screen of the dashboard written
  down for the person in front of it, in two halves. `operator/` is for
  whoever installs and runs the server: first start, the six model slots, the
  embedder, server settings, users, groups, tokens, skills, bridges, wikis,
  usage and spend, the Dream console, recall and REM settings, prompts,
  health, backups, the training spool, exporting and forgetting a person, and
  what to settle before the server is exposed. `user/` is for whoever the
  memory is about: what this memory is, their first sign-in, their home page,
  their facts, who can read what, wikis and pages, the chat, comments,
  reminders and notices, what was recalled for them, their account, and how to
  ask for a copy of their memory or for its removal. It says what a person
  **sees and does** and never how the engine works inside; the rule at the top
  of it is that every page is opened again against the running dashboard
  before a release ships, and a page nobody can verify is deleted rather than
  kept.

- **The guide is in the dashboard too, under Guide.** The pages of
  [`docs/`](docs/) travel inside the binary, so a server serves the guide it
  was built from and there is one copy to keep true. The top bar has a
  **Guide** entry for everybody, and a screen the guide covers with one page
  of its own carries a small **?** beside its title that opens it. Each half
  goes to whoever it is for: the operator's pages need the admin account, the
  rest opens for anybody signed in, and the map shows each reader the half
  that is theirs.

### Changed

- **The dashboard says what the engine does.** Its prose had been left behind
  by the engine under it, and the cost was borne by whoever believed it. The
  Help panel taught four things to say to the chat, and three named
  capabilities no tool has ever had — moving a wiki into another scope,
  retuning an item's time-to-live, changing an item schema — so the operator
  argued with a model that could only refuse. Three sign-up forms offered an
  underscore in an id and the validator on the same page refused it. The fact
  browser said a validity correction belonged to the fact's author when the
  handler asks for its **subject**, sending the one person who could make the
  change away to find somebody who could not. The page-description form
  promised the line was never overwritten, when the writer composes a fresh
  one every time it rewrites that page. Setup said email recovery was coming
  in a later milestone, with the recovery flow already mounted. The post-signup
  page told a new user the chat panel captured and recalled what they typed,
  which is the one thing that panel does not do. Beside them: an offer to
  disambiguate by appending a query parameter no handler reads, a page footer
  denying the raw editor this same file mounts, and five Italian strings on an
  English surface.

  **A model slot with no model is not a way to run.** Four places sold one as a
  supported mode — a Health row reading "unconfigured · feature off", a Dream
  trigger advertising "promotion without an LLM" as the cheap option, and two
  slot descriptions explaining what still works without them. All six slots are
  mandatory; the pages say that now, and the empty Health row reads NO MODEL.
  The editor no longer offers it either: the provider menu has lost its
  *— not set —* entry, and saving with a slot short of a provider or a model
  is refused whole, naming the slot, rather than quietly writing five slots
  and dropping the sixth. In the same pass the same thing stopped being called
  three things: **slot** everywhere, where the LLM page said role, function
  and slot in one screen.

- **The dashboard is usable by the people it is for, and on the phone they
  open it on.** The previous pass fixed what the pages *said*; this one fixed
  what they *do*. Somebody who is not the admin arrived on a home page of eight
  counters, five of them counting rows on consoles that answer them with a 403,
  and an "Issue a token" link into one of those consoles — with nothing about
  their own memory anywhere on it. The home page **opens** on that now (their
  own wiki, the facts about them, their standing rules, what was recalled for
  them), and the operator's view is the operator's: all eight counters and the
  server address a consumer is pointed at are read and rendered for an admin
  alone, because they are taken across everybody's memory, past the
  per-fragment ACL — a reader could not reproduce them and they are not about
  them. The top bar follows: **Skills** and **Bridges** are what a consumer is
  taught and how it is wired in, which is the operator's job, so they are
  offered to the operator — both pages stay mounted for everyone, because they
  are somebody's job and not a secret. A test walks every page a reader passes
  through and fails on any link that would refuse them.

  Columns and knobs stopped being database fields read aloud. The fact browser
  headed its columns `fact_id`, `wiki_id`, `salience`, `allow_ids`,
  `decay_reason`; they now say what they hold — About, Said by, Also readable
  by, Holds from / Holds until. A recall trace said `Hits`, `Hops`, `Stop` and
  printed `done`; it says how many facts were found, how many steps the walk
  took, and that it stopped because it had collected enough. A REM knob asked
  about "min page mass" and "husk-page GC"; it asks how many facts a page must
  hold before it is split, and how many emptied pages to remove per cycle.

  The same thing had four names — an agent on the bridges catalog, a bot on the
  Tokens page, an application on the OAuth consent screen, an assistant in the
  onboarding wizard. It is a **consumer** everywhere, glossed on first mention.

  Three dead ends closed. A model refusal in the chat — a spend stop, a rate
  limit, an unreachable model, a rejected key — was "Something went wrong on the
  server. Check the server logs."; the panel now prints the reason, carries the
  budget's own sentence verbatim for a stop, and offers the page that lifts it to
  whoever can open it. The route that clears a pending comment had no button
  anywhere and was reachable only by crafting the request; every comment on a
  smart wiki now carries "Mark as read", and clearing one returns to the page it
  was on instead of the wiki index. The Claude Code login carried a callback
  route the browser is never sent to, because that OAuth client refuses a
  `redirect_uri` of ours — it is gone, and the page says the code is carried
  across by hand.

  On a phone: every one of the 52 rendered pages now fits 390 px with no
  sideways scroll — the `curl … | sh` install line scrolls inside its own box
  instead of pushing the page out — and a column heading stays on one line
  rather than breaking into four stacked words. A refused save on the recall,
  REM or embedding panel hands the form back with what was typed still in it —
  the offending value included, so it can be seen and corrected — instead of an
  error page and eighteen fields to fill again.

- **A forget-request vote is raised near its deadline, not every turn.** The
  `pending_votes` block rides every turn of its seven-day window, and both
  bridges told the agent to raise it once and then judge from the thread
  whether it had. That is a week of a reminder the agent has to keep deciding
  about. Each bridge now reads the clock itself — the nearest deadline among
  the listed requests — and tells the agent to speak only inside the **last
  day**, and to leave it alone before that. The block stays in the prompt
  either way, so somebody who asks about their votes still gets an answer, and
  a deadline the bridge cannot parse counts as due: raising a vote early costs
  a sentence, missing it costs the fact, because silence is consent.

- **BREAKING — the error code for "this wiki is not smart" says so, and there
  is one of it.** A `wiki_admin_push` or `wiki_admin_pull` at a wiki that is
  not smart came back `400 wiki_type_not_admin_writable`, and a briefing write
  at the same wiki came back `400 wiki_type_not_briefing_capable`: two codes,
  both sending a consumer to look at the wiki's **type** — a free-form tone
  label that decides nothing here — for what is one `_meta` smart flag, read
  by both gates. Both are now `wiki_not_smart`, the message says the flag
  rather than the label, and each keeps its own sentence for what the caller
  was refused (a standard wiki is written through `wiki_ingest_message`; a
  briefing board is a smart-wiki file). Nothing about which wikis accept which
  write has changed; a consumer matching on either old string has to match on
  the new one.

- **BREAKING — the per-fragment ACL axis is named `subject`, after what it
  actually holds.** The field saying *who or what a fact is about* had been
  called `owner` since the first commit — one of the three that decide who may
  read a fragment, beside `sender` and `allow_ids` — so it read as either of
  those, and it collided with the four owners this codebase keeps on purpose.
  Migration `0070` renames six columns and two indexes: `owner_id` becomes
  `subject_id` on `fact_index`, `capture_buffer`, `media_catalog` and
  `document_jobs`; `disclosure_audit.prev_owner_id` / `new_owner_id` and the
  two indexes gain the new word. Hand-written SQL has to be updated; nothing
  else is required at upgrade. `smart_wikis.owner_id` stays — that one is a
  real proprietor and a separate axis. **On the wire** every old spelling is
  accepted and advertised as deprecated: `wiki_search`'s `scope` takes
  `subject_ids`, `wiki_navigate` takes `subjects`, and `recall_core_global`
  answers with `filter_applied.subject_user` while repeating the value under
  `owner_user` for **one release only**. That filter has always been a filter
  on the fact's subject — the facts *about* the caller wherever they are filed
  — never on which wikis the caller owns. **On disk** a running server writes
  the bare `{{f=<uuid>}}` key and keeps the governance in the index; an export
  writes `{{subject=… allow=… sender=… f=…}}`, and the parser reads `owner=`
  **permanently** as an alias, never writing it, because dropping a key that
  is on every page produced before the rename would leave those regions
  unreadable to everybody rather than raise an error.

  **In the structured logs this is a clean break, with no dual emission**: the
  tracing field `owner=` on capture, media, ingest, document and operator-edit
  events is now `subject=`. A saved query, alert or panel matching `owner=`
  goes quiet at the deploy without failing, and one spanning the deploy splits
  into two half-populated series. Update them before upgrading — this is the
  one part of the rename an accepted alias cannot catch.

- **BREAKING — `wiki_read` requires a `path`, and the read side has no concept
  of a wiki.** `path` used to default to `index.md` and the reply carried the
  wiki's `children` and `parent_wiki_id`; the argument is required now and
  both fields are gone. The wiki-shaped half of recall went with it: no root
  index in the navigator's prompt, no "enter a wiki" door, no directory
  listing of siblings — the `sibling_floor` knob is deleted rather than
  defaulted off. A page is reachable by three routes: a fact that hit, a match
  on the page's own card, or a link somebody wrote.

- **Stricter argument validation: an unknown tool argument is refused instead
  of dropped.** Every schema has always declared `additionalProperties: false`
  and nothing enforced it, so a misspelled parameter was discarded in silence
  and the call ran with the argument missing. The wire structs deny unknown
  fields now and the refusal (`invalid_input`) names the offending key; a
  deprecated spelling stays a declared field. `top_k` is clamped to 50, the
  ceiling both schemas already advertised, and every `wiki_search` and
  `wiki_navigate` hit carries a wiki-relative `path` in the spelling
  `wiki_read` accepts — the index stores a workdir-relative one, so "open the
  page behind the snippet" was unfollowable.

- **A claim waiting to be written no longer carries a guess at where it will
  go** (migrations `0071`, `0072`, `0073`). Placing a claim belongs to the
  hourly pass, which reads the memory as it stands then. `capture_buffer`
  drops `wiki_id` and `target_page`, and both it and `fact_index` drop
  `page_description` — a page's card belongs to the page (`page_card`,
  migration `0069`). The parking page every wiki used to get goes too: an
  unplaceable claim waits rather than landing on a page whose whole meaning
  was "unsorted", and `notes` is an ordinary page name again. Migration `0068`
  is the same table, storing a capture's vector at birth.

- **A structural change is read, never rolled back.** The proposal lifecycle
  loses its whole undo half: two of its five states, the revert token and its
  window, the confirmation sweep, the restore helpers and the `bundle` kind.
  The nightly cycle also stops reporting its own housekeeping — a split,
  merge, refile or closure decided at night emits nothing, and the event kinds
  `auto_applied` and `dedup_proposed` are gone. A consumer still receives
  `structure_applied` for a change asked for in a conversation or from the
  dashboard, beside `fact_minted_for_you`, `reminder_due` and
  `document_ingested`.

- **The model configuration the dashboard saves is the one the MCP side
  uses**, where it used to hold a copy taken at boot while promising
  otherwise. And because the six slots are mandatory, a missing one is said
  out loud now: a warning per slot and one error naming all of them at boot,
  the same banner on the LLM-config page and the admin's home, and a key under
  `llm:` that is not one of the six named in a warning at load. The seventh
  slot that used to sit in that list is gone, and with it the reason every
  canned setup profile left the dashboard's chat with no model.

- **The 25th person, the 9th group and the 33rd list are refused where they
  are created**, rather than cut where they are shown — the prompt used to
  trim those lists to fit, always dropping whoever sorted last. The limits are
  24 enrolled users, 8 groups a user and 32 list pages a wiki, **checked only
  against growth**, so a deployment already over one keeps working. A list
  refused a page is not a lost fact: the turn tells the agent to say plainly
  that it was not saved and why.

- **The first cycle after the upgrade recompiles every page that carries or
  receives a link.** A page's fingerprint now covers its links, and no longer
  covers the parenthood removed with it — pages have no parent, so the field,
  the testata line and the prompt placeholder are gone.

- **Linux, macOS and Windows are all the gate.** Every push runs the whole
  test suite on all three, and a red one on any of them is a red build. Until
  now Windows ran and was allowed to fail, which is the same as not running
  it: a release published a Windows binary whose suite nobody had to look at,
  and it had been failing since the first public release. Three more tests
  were skipped on Windows outright, each naming the same tracking issue, and
  all three were skipped for the address bug fixed above — the retired-region
  reveal, the gated re-file, and the freshness stamp on a navigated page. They
  run everywhere now, so no test is skipped on any platform. `INSTALL.md` opens
  with what each platform gets — the suite, a prebuilt binary, how the server
  is run as a service, and the two checks that do not fire everywhere: the
  refusal to start under a login account, which is Linux-only, and the
  workdir permission audit, which reads POSIX mode bits and so is silent on
  Windows.

- **The server runs as a service on all three, under an account nobody logs
  in with.** On Linux `mwe-mcp serve` still does the whole thing for you.
  macOS and Windows now have theirs written down, with the file to install:
  a launchd daemon (`packaging/macos/com.mwe-mcp.server.plist`) and a
  scheduled task at boot (`packaging/windows/mwe-mcp-task.xml`) — a task
  rather than a Windows service because `mwe-mcp.exe` is a console program and
  a service made from one is killed at start. Both ship inside the release
  archive, next to the binary they configure.

  The account is the point of it, not the restart-on-boot. The per-reader
  redaction is applied when the server renders an answer, and the memory under
  the workdir is cleartext on disk, so anything running as an account that can
  read those files reads every fragment un-redacted. On Linux the server
  refuses to start under a login account; **on macOS and Windows it does not
  check** — that check reads Linux-only files — so there the setup is yours to
  do and `INSTALL.md` says so in the same table.

- **The desktop tray is Linux, and stays Linux.** `mwe-mcp-tray` draws itself
  through a D-Bus protocol only Linux desktops implement, and everything it
  does is a Linux command — `systemctl` for the service actions, `xdg-open`
  for the dashboard, `journalctl` for the logs. It controls nothing the
  dashboard and your platform's own service tools do not, so its absence
  elsewhere costs a convenience rather than a capability. No release has ever
  published it.

### Deprecated

- `wiki_admin_signpost` says so in the first sentence of its own schema
  description now, so an agent reading the tool list is told before it calls.
  Both signposts ride `wiki_admin_push` as fields; the tool still answers.

### Removed

- **A standard wiki has no `index.md`.** Every night REM assembled one per
  wiki — a listing of the pages that lived there, for whoever files a fact and
  has to decide where it goes. Nothing read it: the side that places a fact is
  handed the same material out of the compilation plan and the `page_card`
  table, both written on **every** page change rather than once a night. Gone
  with it: the `map_writer` REM sub-job and its `map_writer_cap` policy field
  — internal only, never a `rem:` config key, though the configuration
  reference listed it as one — the `wikis/index.md` collector file, the
  `index.md` seeded into every new wiki, and the four rules that kept readers
  away from it.

  **And the name is not reserved either.** `wiki::RESERVED_PAGE_STEMS` holds
  `rules`, `projects`, `project_diary`, `projects_diary` and `profile`, beside
  every `@`-prefixed stem; `index` is not among them, because the fence
  guarded a word rather than a thing. On a **smart** wiki `index.md` is
  ordinary content its consumer authors through `wiki_admin_push`.

  **Migration: none needed.** A leftover `<wiki>/index.md` is no longer exempt
  from the compiler's orphan sweep, so the first compile removes it (a file
  carrying live facts is kept). The loose collector is outside every sweep and
  can be deleted by hand.

- **Arguments and endpoints that were published and never existed.**
  `wiki_read.format`, `wiki_read.include_archived` and
  `wiki_search.scope.include_archived` never reached a filter;
  `dashboard_link.channel` and the `path` / `git_ref` locators on
  `wiki_ingest_external.source` were never read; `POST /mcp/token-refresh` was
  never mounted, and an operator re-issues from the dashboard instead. The
  auth error classes are `missing_bearer`, `invalid_token` and `token_revoked`
  — the `expired` and `secret_rotated` two documents promised both arrive as
  `invalid_token`. Going the other way, `metadata.channel` on
  `wiki_ingest_message` is declared now, and `wiki_forget` /
  `wiki_forget_bulk` stamp the caller's `reason` into the tombstone.

- **The `mwe-mcp-test-faults` build feature and its `fault!` macro**, which
  had no call site outside its own tests while CI paid for a second full suite
  run.

### Fixed

- **The pages of a topic wiki open again.** A *topic wiki* is the kind the
  nightly grouping raises: named for its subject, standing on its own, owned by
  nobody. Every gate that opened a page by first asking *who owns this wiki*
  read that as a broken wiki file and refused. In the dashboard every page view
  of such a wiki was a server error; over MCP the page-reading tool failed the
  same way, and so did leaving a comment on one of those pages. A wiki nobody
  owns is now an ordinary answer rather than a fault: what may be read there is
  decided fact by fact, exactly as it already was everywhere else. Three
  consequences you can see: the `owner` field the page-reading tool returns is
  `null` for such a wiki instead of naming somebody; its pages are editable by
  hand from the dashboard by an administrator, since there is no owner to be;
  and the nightly structural review is shown these wikis in the forest it
  weighs, where before they were missing from it altogether.

- **Commenting on a page now follows the same rule as reading it.** The
  dashboard asked two different questions about the same page: whether you may
  read it, which on a standard wiki is decided fact by fact, and whether you may
  comment on it, which asked who owns the wiki. On a wiki nobody owns the second
  question had no answer at all — that is where the failure above came from —
  and on a person's wiki the two could disagree, so the page could show a
  "+ Comment" link the endpoint then refused. There is one question now: you may
  comment where you may read. The administrator's reveal switch still grants
  nothing on its own — it opens a page to be looked at, not written on.

- **On Windows the downloaded model stays where it was put.** The 2.2 GB of
  bge-m3 weights are fetched once and kept in a cache directory. Linux and
  macOS name that directory after the account the server runs as; Windows has
  no `$HOME`, so unless `XDG_CACHE_HOME` was set by hand the server fell back
  to a `.cache` folder **beside whatever directory it happened to be started
  from** — a different folder per shortcut, per shell, per scheduled task, and
  a fresh 2.2 GB download each time it changed. It now uses
  `%LOCALAPPDATA%\mwe-mcp\models`, the per-user store Windows names for this,
  and `XDG_CACHE_HOME` still wins where it is set (the packaged scheduled task
  sets it, so a service install is unaffected). **A Windows install that had
  already downloaded the weights downloads them once more**, into the stable
  place; the old `.cache` folders can be deleted.

- **On Windows, a page of your own facts no longer reads as entirely
  private.** A page's address in the engine's index is written with `/`
  between its parts, on every platform. Five places built that address from
  the host separator instead, which on Windows is `\` — so the address they
  looked up matched nothing, and what came back was an empty answer that reads
  exactly like "there is nothing here you may see". On the dashboard and
  through `wiki_read` the whole page rendered as redacted; the nightly cycle
  found no facts on any page it was weighing; the recall gate and the
  freshness stamp on a navigated page missed the same way. One helper now
  writes that address, and the accessors built on it are how the rest of the
  engine asks for one. On the compiler's side the same mismatch emptied a
  page's outgoing links, which is the compiler's licence to drop them.

- **`mwe-mcp doctor` stops calling a workdir it never looked at "owner-only".**
  The workdir permission audit reads POSIX mode bits, and Windows expresses
  permissions as ACLs — so on Windows it returned nothing to report and the
  report turned that into a clean bill of health. It now says the permissions
  were not inspected and points at the `icacls` step in `INSTALL.md`, which is
  the same distinction the boot-time warning already makes by staying silent.

- **A page write asks whether the name is free, byte for byte.** macOS and
  Windows treat `Ricette.md` and `ricette.md` as one file; the engine treats
  them as two pages. Asking the filesystem "does this page exist" therefore
  gets a different answer per platform and the wrong one on both: moving a
  fact to a target spelt one way while the page on disk is spelt the other
  either coined a second page that a smart consumer's mirror will later
  collapse, or wrote into the first while telling the index it was the second.
  The three move-and-append handlers, the page re-home, a smart consumer's
  `wiki_admin_push` delete and the dashboard's Revert button now read the
  directory listing, and a name that only differs from an existing page by
  case is refused with the spelling already on disk — where, on a case-folding
  host, either delete removes a page the caller never named.

- **A plan a later message closed no longer rings as a commitment coming
  due.** The due sweep fires on a `plan` carrying a concrete end date, and a
  `valid_to` has two authors: the classifier writes one at capture, from a
  date the speaker stated, and every closure writes one too — the instant the
  fact stopped holding, which for a plan refined in conversation ("a bar of
  soap" → a chosen brand) is the instant of the message that refined it. The
  sweep read them as the same thing, so refining a plan announced it to the
  subject and to everybody it had been shared with, minutes after it was
  dropped. A closure always leaves a `decay_reason` on the row and a date
  correction never does, so that stamp is now the whole rule: a stated
  deadline still rings, on the same schedule as before, and a closed plan is
  silent.

- **Whoever said a claim may say it better.** Rewriting a fact asked only
  its **subject**, so the person who had recorded a sentence could not correct
  it whenever it was filed under somebody else — a parent who recorded the
  hour their child was born was refused the correction, because the sentence
  sits under the other parent. The gate is the subject **or** the author now,
  on both supersede paths (the reconciliation stage and capture-time
  supersede) and on the dashboard's own edit. It is the argument the
  retraction gate already made — they said it, so they may say it better —
  which had left rewriting strictly narrower than withdrawing. The audience
  stays out on purpose: being told something does not make it yours to
  restate, and a reader who thinks a claim has stopped being true retires it
  instead.

- **A second reading is not a correction of the first.** The reconciler is
  taught one pair that reads as two facts and is one: a count measured from a
  fixed point ("at 24 June she was at 29 weeks") against a later turn stating
  the point itself. A measurement is the opposite case, and the model was
  carrying the rule across to it — proposing that a weight recorded on a day
  be replaced by a later one. A weight, a blood value, a blood pressure was
  true of its day and stays true of it; two readings on two days are the shape
  of a history, and superseding one deletes a measurement nobody withdrew,
  which is most of what a memory of somebody's illness is made of.

- **Every dashboard address an assistant offers opens a page.** Five of
  `dashboard_link`'s eight intents minted an address the dashboard has never
  mounted, and the cost fell on the person holding the link: redemption burns
  the token before it redirects, so a single-use link was spent to arrive at a
  `404` — which reads as the memory being broken, with nothing to say that
  only the address was wrong. `answer_proposal` lands in the chat with the
  proposal already summarised, `audit` on the recall traces, `costs` on model
  usage and spend, `settings` on the caller's own settings page, and
  `archive_view` on the fact browser, which is where archived rows live —
  there is no separate archive page. No intent was removed and the enum is
  unchanged; the schema now says which page each one opens, and the admin
  gate covers the two that show the **whole deployment** — the recall traces
  and the spend. A link to one's **own** settings page is open to every
  enrolled sender, the same rule the dashboard itself applies to that page. The sixth dead address was not a link at all: the `pending_votes`
  block sent a member owing a vote to a proposal tray that does not exist,
  when a vote is cast by talking to the dashboard's chat. Two tests now take
  the roster from the tool's own schema and the verdict from the real router,
  so a ninth intent cannot arrive without a page.

- **The ready-made assistant answers, and the memory is in the turn.** The
  NanoClaw bridge went out with three faults that only a real install shows.
  Its patched poll loop asked "is the memory on?" where it should have asked
  "did a follow-up arrive?", so the agent aborted its own stream half a second
  into **every** turn and replied to nobody; the reverse channel looked for a
  paired chat under the bare platform id when NanoClaw stores
  `telegram:<chat id>`, so every notice waited for a chat that was right
  there; and a `senderMap` key with no channel in it — the shape hermes
  accepts — was kept, then failed to route once per key per tick. Beside them:
  a session already on disk is dropped at startup rather than resumed, so
  "no session between turns" holds for the first turn of a replaced container
  too. The offline smoke now runs two turns **on the clock**, with the host
  answering slowly, because a mock that replies in the same microtask never
  lets the follow-up poller fire — which is how a loop that aborted every turn
  stayed green.

- **Installing that assistant no longer has three dead ends.** The served
  installer cloned NanoClaw at one ref, so the setup wizard died at the
  channel step (`fatal: invalid object name 'origin/channels'`) — the channel
  adapters are copied out of a branch that clone does not track; both registry
  branches are fetched now, for a fresh clone and for a checkout it was
  pointed at. It also names `mwe` as the template in the fork's `.env`, the
  one setup key NanoClaw reads from there, so the wizard offers the agent
  instead of making the operator find it. Applying the skill removes the
  memory tree an earlier boot left behind — only when it is still NanoClaw's
  untouched templates, byte for byte — restarts the agent containers as well
  as the service, and hands a memory group NanoClaw's shared `CLAUDE.md`
  without the two sections that send an agent to read `memory/` and
  `conversations/`. **The agent's name is now the memory's**: NanoClaw's
  configured name is not injected, so anybody it serves can tell it what to be
  called, in chat, and it holds.

- **A backlog of memory notices arrives, and arrives once.** The reverse
  channel enqueued one chat message per notice, so an agent catching up after
  an outage delivered eight of them in the same second — a burst of alerts
  rather than somebody who remembers. Everything waiting for the same person in
  one round is now composed into a **single** delivery, each item keeping its
  own source and its own link, and acked as a group; two people's notices stay
  two deliveries, and the operator's daily recap stays separate. The same
  batching closes a hole beside it: a delivery instruction is stored nowhere
  and is acked to the server the moment it is enqueued, so a turn ended halfway
  through a batch of them lost the rest. **A turn carrying notices is no longer
  interrupted** — somebody writing in mid-delivery waits that turn out and is
  served next, with an ingest and a recall block of their own.

- **A reminder arrives as a reminder, and a link arrives as a link.** Three
  things both bridges got wrong on the delivery side. A commitment falling due
  was batched into the voice of the memory notices, so four reminders reached
  one person as "a few things came in for you from other people's
  conversations": each kind keeps its own block and its own heading now, and
  the reminder block says what a reminder is — a commitment already in the
  memory, coming up, at an hour on the clock the person reads, in the
  install's own zone rather than a UTC instant. It does not say whose
  commitment it is, because the notice carries no subject: the memory rings
  one for its subject and for everybody it was shared with alike.

  A dashboard link was handed on as the bare path the server mints, so an
  assistant offered `/dashboard/auth/link?token=…` to somebody holding a
  phone. Completing it is the consumer's job — the server does not know the
  address it is reached at unless the operator declared one — and both bridges
  now hang it on the dashboard origin they already use, as does the page a
  `pending_votes` block names. An answer to a disambiguation carried the
  server's raw JSON; it carries the same block an ordinary turn does,
  under one line saying the message is stored.

  Beside them: a person this consumer has no chat for at all is declared
  `unroutable` and their notices are confirmed as they arrive, instead of
  being retried every thirty seconds for ever; and re-applying the memory
  skill after an upgrade refreshes the modules it copied into the fork, which
  the install directive alone does not do — it wrote what was missing and
  reported success on what was already there, leaving the fork on the old
  bytes.

- **The Users page says where an agent is created.** The "new user" form needs
  an email because it makes a person who signs in; a bot has neither. It now
  points at the Tokens page, where a standard consumer's **Consumer id**
  mints the agent's identity and its wiki with no login.

- **The skills a model is handed at runtime say what the engine does.**
  Twenty-two claims across five of the bundled skills and
  `AGENT_INSTRUCTIONS.md` described an engine that had moved under them, and three carried a visible
  cost: `wiki_admin_notify` was documented as smart-only, so the one path by
  which a standard consumer reaches a smart consumer's inbox read as closed;
  an on-the-fly date correction was documented as the subject's alone when
  the gate is the subject, the author, or anyone the fact was shared with;
  and the briefing inbox was documented as a `## Unread` section to parse out
  of `_briefing.md` when `smart_bootstrap` hands the pending items back
  already filtered — with no mention of the `mark_processed` that clears
  them, so the same item came back every session. The rest: ingest emits no
  structural proposal, `llm_used` is a bool, `_meta.md` carries no
  `owner_user`, a push replaces the touched page's section rows rather than
  the wiki's, REM never dedups a smart wiki, a briefing `kind` is the
  notifier's field, the op-log revert has no time window, there is no bundled
  `wiki-companion` type behind the folder layout, the `/cite/` resolver is
  mounted, and the `initialize` handshake already sends `instructions`.

- **"You are called X for everyone" renamed the user instead of the agent.**
  The ingest prompt covered naming the agent for the speaker alone and nothing
  else, so naming it for the whole audience it serves came back as the
  cross-assistant scope — the opposite referent, a rule about the speaker — and
  it was filed in the user's own memory with a body that renamed the user. The
  prompt now carries the pair: naming the agent for everyone it serves is
  `agent-wide`, with the addressee dropped from the body ("Your name is
  Gandalf."), filed in the agent's wiki; naming the speaker for every assistant
  they talk to is `user-global`, with a third-person body ("Call the user
  Gandalf."), filed in theirs.

- **A turn that stored nothing said it had noted it.** Every fallback of
  `wiki_ingest_message` — model unreachable, unparseable reply, plan that
  could not be applied — returned the `suggested_seed` "I've noted that." on a
  turn where nothing was saved. The failure path has its own seed now, saying
  plainly that nothing was stored; the old sentence stays only for a turn the
  classifier deliberately filed as nothing to keep, so **a consumer matching
  on that string should read the new one.** Around it: no provider call was
  retried at all, so a 429, a 5xx or a timeout was one lost memory, and every
  backend is wrapped in two jittered retries; and Ollama calls set `num_ctx`,
  never sent until now, so its default window had been truncating the prompt.

- **A boot the model probe refuses exits `EX_CONFIG` (78)**, and the generated
  systemd unit names it in `RestartPreventExitStatus`, so a misconfigured
  provider is a stopped service an operator can see rather than a two-second
  relaunch loop of paid probes. `mwe-mcp recall eval` no longer migrates the
  database it opens — it runs beside a live server and takes no lockfile. And
  `init --force-config` rewrites the config file only: `mwe-mcp.env` holds the
  token secret, and rewriting it would invalidate every token, session and 2FA
  enrolment already issued.

- **One bad model reply no longer costs the night.** Auto-promote, page
  grouping and page merge turned a transport error into a failed cycle, which
  then skipped the compile and left the capture queue undrained until the next
  night. All three now record it, skip the candidate without memoising a
  verdict, and stop that sub-job after five *consecutive* failures. Nearby: a
  proposal whose handler keeps refusing is expired after its grace window,
  where the state diagram promised as much and the function doing it had no
  caller; and a page whose markers do not parse whole keeps its facts rather
  than tombstoning every one below the cut.

- **The stages that judge stored facts read what the turn read, and what it
  was asking.** The reconciliation stage saw only the raw sentence, so "I
  bought it" matched no stored claim on its own words; it gets the
  classifier's completed sentence now, with the raw message still above it,
  and where the two disagree the words the user typed win. Its structural leg
  was the navigator's walk alone, so the identity cards served before the walk
  were not part of what the turn had read. And both nightly closure sweeps
  dated their work by the operational clock rather than by when the fact
  began.

- **The rules the prompts teach are the rules the engine enforces.** Prompts
  and skills are read by a model at runtime, so a sentence naming an argument
  the handler ignores or a page name the engine does not refuse is an
  instruction an agent follows into an error. The reserved page names carried
  the visible cost: four prompts recited a list containing `notes`, which is
  free, and missing `projects_diary`, which is refused. A claim aimed at a
  refused name is not dropped quietly: the whole extraction is refused and the
  user is told so. Fourteen further disagreements went the same way, the
  public documents got the same pass, and `install.sh` refuses to install a
  binary whose checksum it could not fetch.

- **The boot probe sends what the slot's calls send.** A model was
  declared reachable on a request the engine never makes: sixteen output
  tokens, no system prompt, extended thinking forced off even where the
  slot had asked for it, and no image on the slot whose calls carry
  photographs. Each of those is a way a provider says no — and it said no
  on the first real turn instead, where the failure reads as the memory
  being broken.

  The probe is now built per slot: the system prompt every call carries,
  the temperature the hot paths pin, the slot's own configured reasoning
  effort, the ceiling from the slot's own configuration rather than a toy
  number, and — on `ingest`, the slot whose calls carry photo bytes — an
  image, under the same gate the real path uses. What follows is that a
  wrong model fails at startup with the provider's own words about what
  it refused.

- **A person the memory has never enrolled is no longer mistaken for one it
  has.** Tell your assistant about a colleague whose first name is a longer
  form of an enrolled person's user id — and carrying a surname nobody on the
  deployment has — and every fact of that conversation was filed as being
  about the enrolled person, most of them onto their own always-on card, where
  anybody reading it takes them as true of them. The roster the memory matches
  names against is user ids and the **aliases** an operator declared, with no
  surnames in it at all, and nothing said that a resemblance is not a match:
  the classifier finished the resemblance itself.

  A name now reaches an enrolled person only when it **is** their user id or
  one of their declared aliases, compared whole and ignoring case and accents
  (an id is plain lowercase letters, so `eowyn` is how "Éowyn" is enrolled —
  and writing the accented name now also brings that person's card into the
  turn, which it did not before). Anything else — a longer or shorter form, a translation, an undeclared
  nickname, a full name whose surname the deployment does not carry — is
  somebody else: the memory keeps the name as said, as a fact **about** that
  person, and files it under whoever answers for them. Underneath the rule
  there is a floor that does not depend on the model: a fact owned by an
  enrolled user the conversation never named is re-filed onto the person
  speaking, so it can never claim somebody else's card. Three cases the words
  cannot settle are left to the classifier — the assistant's own principal,
  two enrolled people sharing a name, and a turn whose photographs the
  classifier is shown. **An uploaded document travels the same road**: there
  the words weighed are the part of the document being read together with its
  title and summary, since a document carries no earlier conversation behind
  it and no photographs, so a fact owned by an enrolled person none of them
  names is filed under whoever uploaded the document.

  This makes a person's **Aliases** load-bearing: they are the only place a
  name the memory answers to can come from, and the field, the welcome page and
  [the guide](docs/operator/users.md) all say so. Two hands fill them — the
  operator's, on that field, and the person's own: the full name and the
  nickname they type at their first sign-in are added to their aliases, so
  somebody who introduces themself as "Frodo Baggins, called Fro" is reachable
  by both names from that moment on. A name of several words is one name,
  matched whole and in the order it was declared. And because a name reaches
  one person, **a name somebody else already answers to is refused** — on that
  field and on the welcome page alike, with the form coming back as it was
  typed and saying whose name it is; a name equal to the person's own id is
  not refused, it is simply not added twice.

- **A page can be commented on even when it has no headings, and a comment can
  say a fact is about somebody else.** Comment mode offered one way in — a
  **+ Comment** link beside each heading — and a person's always-on card is
  written without headings on purpose, so on the most important page of a
  memory there was nowhere to press: the box announced comment mode and nothing
  followed. Every page now also carries **+ Comment on this page**, above the
  text, for a remark about the page as a whole; comments left that way are
  shown together at the top of the page under **On this page**, and the nightly
  pass acts on them exactly as it acts on one left beside a heading — the
  heading only ever said *where on the page*, never *which facts*. The footer
  headed **Orphaned comments** now holds only what it was meant to: a comment
  whose heading the page no longer has.

  The second half is what such a comment may ask for. The pass could correct a
  claim, remove it, add one or move it to another page, but it could not change
  **who a fact is about** — so facts the memory had filed under an enrolled
  person while they were about somebody else, a colleague or a relative with no
  account here, could not be put right from the page. Now they can: say so in a
  comment and the claim is filed again under the name it really concerns,
  keeping its words, with who answers for it decided the same way the memory
  decides it when it first hears the fact.

### Security

- **A dashboard session is only a token minted for the browser.** Every JWT in
  a deployment is signed with the same secret, and the session check verified
  signature, expiry and revocation and nothing else — so an MCP bearer token,
  long-lived and carrying its owner's admin flag, was accepted as a dashboard
  session and skipped the second factor. Session verification refuses every
  device label but the browser's now. In the same pass: the login had no
  attempt limit and is rate-limited per email and per client address, an
  unknown one verified against a fixed hash so the response time does not say
  whether an account exists; four pages embedded JSON in a `<script>` without
  escaping `</script`, letting memory text written by one user run in an
  admin's browser; the smart-wiki briefing and operation-log pages showed any
  wiki's inbox to any signed-in user and now go through the same read gate as
  `wiki_read`; `/mcp` bodies are capped at 32 MiB; and credentials are
  scrubbed from audit summaries.

- **A snapshot no longer lands world-readable, and the new
  `instance.cookie_secure` puts `Secure` on the dashboard's cookies.** A
  snapshot holds the same cleartext memory as the workdir but lives outside
  its owner-only gate, so under the service unit's `UMask=0022` it was written
  as a `0755` directory around a `0644` database; directories it creates are
  `0700` and the vacuumed database `0600`. The cookie flag is off by default,
  because the documented first run is plain `http://127.0.0.1:8742` where a
  `Secure` cookie never comes back — turn it on once the dashboard is behind
  TLS.

- **Two tables grew without bound, and a third had no window at all.** Every
  revoked token id stayed in `token_blacklist` after the token it revoked had
  expired, and the in-memory blacklist reloads that table every 60 seconds;
  every row in `wiki_events` stayed, and every consumer poll scans them.
  Housekeeping drops expired revocations and events past a 30-day retention
  now, and runs daily rather than only at boot; `reminder_due` rows are
  exempt. The third is the recall-trace journal, which holds each recall block
  verbatim: the new `recall.trace_retention_days` (default 90) bounds both how
  far back an operator can ask why recall behaved as it did and how long
  cleartext recalled memory sits in the engine database.

- **A token now has a ceiling, and it applies whether or not anybody wrote one
  down.** Every token has always carried a `rate_limit_id` claim and the wire
  had a `429 rate_limited` class, but nothing counted anything: a stolen token,
  or a consumer whose loop lost its brakes, could run the classifier and the
  navigator — model calls, on somebody's invoice — as fast as the network
  allowed. The dispatcher counts every call against the profile the token
  names: 120 calls a minute and 3 000 an hour, of which 30 a minute and 600 an
  hour may be the calls that put a model or an embedding to work
  (`wiki_ingest_message`, `wiki_ingest_external`, `wiki_navigate`,
  `wiki_search`, `recall_core_global`). Counting is per token, so one runaway
  consumer never spends another's allowance. Past the ceiling the call comes
  back `rate_limited` with a `retry_after` in seconds. The new `rate_limits:`
  section gives a named profile different numbers — `dashboard`, the profile
  the sessions `dashboard_link` mints carry, gets five times the call
  allowance and four times the model allowance out of the box — and a
  `rate_limit_id` with no profile falls back to `default`, so a token cannot
  name its way out of a ceiling.

- **Signing out ends every session, on every device — and it now ends
  this one.** Two faults, one surface. Signing out revoked only the
  cookie doing the signing out, so a phone left on a train, a browser on
  a shared machine and a copied cookie all kept working until they
  expired on their own; there was no way to end them, because a session
  is a stateless JWT re-minted with a fresh id on every request and
  nothing holds a list of them. And the sliding refresher, which re-mints
  that cookie after every request, was re-minting it on the sign-out
  response too — appending a brand-new valid session cookie *after* the
  cleared one, which is the one a browser keeps. Signing out did nothing
  at all in a browser.

  Every session JWT now carries the generation its user was on when it
  was minted (migration `0076`), and signing out moves that number on:
  every session of that person is refused from its next request, one
  UPDATE, nothing to clean up afterwards. Changing the password does the
  same and keeps the browser that changed it signed in; a password reset
  through the recovery link does the same and signs nobody in. And a
  handler that has decided about the session cookie now has the last
  word over the refresher. The button says **Log out everywhere**, because
  that is which of the two plausible things it does, and the settings page
  names the button rather than describing it in its own words. MCP bearer
  tokens are untouched: those are a consumer's credential, not a person's
  session, and they are revoked from the Tokens page.

- **A link the server sends is never built from the browser's `Host`
  header.** The password-reset email took its origin from that header
  whenever the operator had not set one — and the header is chosen by
  whoever sends the request, so anybody could make the reset link in
  somebody else's inbox point at a machine of theirs. The invitation email
  had the same fallback.

  The address a deployment is reached at is now a key of its own,
  `public_base_url`, at the top level of `mwe-mcp.config.yaml` — it is not
  a property of the mail server, which is where it used to live, but of
  the deployment, and three things need it. Validated where it is written
  (`https://…`, `http://` only for a loopback host) and editable from
  Settings → *Public address of this server*. **Without it the two emails
  are not sent at all**: the forgot-password form says recovery is
  unavailable, the invitation flash says the address is missing and hands
  over the link, and the server says so at boot. `dashboard_link` answers
  with a full address when the key is set and, as before, with a path for
  the consumer to complete when it is not.

  **Configuration change**: the key inside `email:` is still read when it
  is the only one set, with a line at boot naming the new one; saving the
  public address from the dashboard moves it up. There is no longer a
  field for it in the Email section.

- **Three things that grew for ever now have a window.** The per-call audit
  trail kept a row per tool call since the day the deployment started; the
  operation log kept, for every push a smart consumer ever made, the full
  text of every page that push overwrote — a second copy of the memory,
  growing in proportion to how much has been written; and a deleted wiki
  waited in `<workdir>/trash/` for ever, a cleartext copy of a memory
  somebody meant to remove. The new `retention:` section bounds all three
  (`audit_days` 90, `undo_days` 30, `trash_days` 30; `0` on any of them
  means keep for ever) and the daily housekeeping sweep applies them.

  `undo_days` **is** the undo window, and the operation-log page says so:
  past it the row stays — who pushed what, and when — and only the Revert
  button goes. The trash sweep reads the moment of the deletion from the
  directory's own name, because moving a directory does not change its
  timestamps, and it touches nothing in `trash/` that the server did not
  put there.

## 1.9.0 — 2026-08-02

Recall was handing the model the wrong material and reaching the right page by
luck. Everything here sits on the path every single turn takes.

### Added

- **A project's description is something the memory is now told, not
  something somebody has to remember to write.** An assistant working inside
  a codebase keeps its own project wiki, and for the rest of the memory to
  know that wiki is worth opening, somebody had to go and describe it on a
  shared overview page. That was a separate act, and it was simply skipped:
  on the first deployment **one of four** project wikis had a description at
  all, and the largest — 1 477 indexed sections — had never had one. So the
  memory held the material and had no way to know it was worth consulting.

  The description now rides the same call that writes the pages
  (`wiki_admin_push` gained `description`), which makes declaring it part of
  working rather than an errand afterwards. The server stamps it on the
  wiki's own metadata, keeps a queryable copy, and **composes** the overview
  page itself from those copies — so several assistants pushing at once can
  no longer overwrite each other's entries, which is what that page had been
  quietly losing. Withdraw a description and its entry retires by itself.

  The other half of that page — "what was done today" — moved onto the
  project's own `@projects_diary.md` and rides the same call (`activity`). The
  overview page is the server's to compose; the diary is the consumer's to
  fill. Nobody writes both.

  Adds migration `0067`.

### Changed

- **Documentation stopped crowding out what people actually said.** Where a
  deployment indexes a codebase's docs, those documents are around **96 % of
  all indexed characters**, and the merged search let them compete directly
  with personal facts on every turn. The document corpus now sits behind a
  funnel: searched when the turn is about a project, out of the way
  otherwise.

- **The navigator's doors are ranked instead of alphabetical, and the
  identity page is no longer the first door on every turn.** Two defects that
  compounded into one. The list of pages the navigator may open was cut to
  its cap by **file order**, so a wiki whose name sorts early filled the list
  and another wiki's twenty pages were never offered at all. It is now
  ordered by kind: pages linked from the prose just read, then pages the
  search found, then folder neighbours — which are a crutch, and are now
  labelled as one in the code.

  And the seed standing for "the person speaking" carried maximum weight,
  which made their own identity page the top door whatever the turn was
  about: on one real trace **82 % of the characters collected** were two
  identity pages. That seed now sits on the ordinary rung it belongs to
  (`1.0 → 0.6`), so a content page leads on roughly half of turns and
  identity leads on the rest. It never disappears — the one-line identity
  card is served unconditionally and separately, exactly as before.

- **A command that needs the memory in order to be carried out is no longer
  filed as small talk.** The test deciding whether a turn needs memory read
  *is this a question*, rather than *does answering it require something only
  this memory holds* — so "put on some music we both like" was treated as
  chatter and the deeper pass never ran. The test is now the unresolved
  reference. Measured on 128 real turns before shipping: 9 of 13
  reclassifications correct, and **no turn that had been storing something
  stopped storing it**, which was the risk worth measuring.

### Deprecated

- `wiki_admin_signpost` — the separate call that used to write a project's
  description and its activity line. Both now ride `wiki_admin_push`. The
  tool still answers; the bundled skill no longer asks for it.

## 1.8.4 — 2026-07-31

Two things that governed who may read what were not governing it. Both are
fixed here, and both were found by looking rather than by a report.

### Fixed

- **Every project and agent wiki was readable by every enrolled user.** The
  check each read path made is a derived one — *can this reader read at
  least one fact in this wiki?* — and it returns "visible" for a wiki that
  holds no facts, which is correct for a standard wiki whose first fact has
  not been promoted yet. A **smart** wiki holds no facts by construction:
  it is markerless, and its content is indexed elsewhere. So it took that
  branch every time, and its `owner_user` / `shared_with` decided nothing.
  Affected the page read over MCP, the two dashboard read surfaces, and the
  dashboard's Smart tab, which had no check at all and listed every smart
  wiki to everyone.

  The gate that governs a smart wiki already existed and was already right —
  the comment path had been using it. The read paths now ask it too, by
  family, in one place. Search and recall were never affected: the section
  index filters correctly, verified against a real dataset.

  A single-user deployment was never exposed, which is why this survived.

- **An addressed notice was handed to every connected consumer.** The
  reverse-channel queue filtered on the polling consumer's acknowledgement
  slot and nothing else, so every registered consumer received every event —
  including the one that carries fact bodies inline. The recipient field was
  advice that nothing enforced. Scope now lives in the query, so a row that
  may not be delivered is never read into the process at all. On the first
  production deployment the hole had been crossed once in 532 notices, and
  no content reached a person: the bridge refused to route a personal notice
  to a recipient it had no private channel for, and logged the refusal
  instead. That last line of defence was in the wrong place.

- **A recalled memory that had expired was being read as the present.** Every
  fact stores when it starts and stops holding, recall carries those bounds,
  and the ranker already pushes a spent fact down the list because it knows.
  The one place that never said so was the instruction handed to the model,
  which listed each recalled fact with its id, owner, audience and text and
  no dates at all — so a fact that had stopped being true looked exactly like
  a permanent one.

  It produced a sentence that contradicted itself: an assistant asked to
  resolve "we ate without him" borrowed the pair of names from a fact whose
  window had closed twenty-one hours earlier, and wrote that two people had
  eaten dinner without waiting for one of the two. Nothing was invented; a
  detail was imported from a memory that had expired.

  Each recalled fact now arrives marked *ended*, *not in force yet*, or *in
  force until*, resolved against the clock of that message. A fact that makes
  no claim about time carries no marking and costs nothing. The instruction
  gained one general paragraph rather than a rule about this case: an ended
  fact is evidence of what was true, its particulars are not available to
  fill in the present, and when a memory and the person disagree, the person
  wins.

### Added

- **A dated commitment announces itself.** The memory held "Thursday at five
  at the dentist", surfaced it to whoever happened to be talking near the
  time, and never spoke first. A sweep now emits a reminder when the
  commitment comes round. It is not a scheduler: it fires only for a fact the
  memory already holds, so a later turn that moves the appointment moves the
  reminder with it, which a job frozen at the moment of asking cannot do.

  New `reminders:` config section (`enabled`, `day_hour_utc`, `lead_secs`,
  `grace_secs`, `cap`). The grace window is what stops the first run after an
  upgrade announcing every appointment already in the past.

- **A personal notice carries the link to what it is about.** The notice
  always knew the exact page; the delivery instructions never mentioned it,
  so the message arrived with no way to open it. The morning digest had a
  link and personal notices did not — now both do, from the same setting, and
  an unconfigured public address means no link rather than a broken one.

- **An undeliverable notice says so when it is written.** If a notice is
  addressed to somebody no agent is delegated to serve, the server warns at
  that moment and names them, rather than leaving a row that looks delivered
  because nothing ever acknowledges it. The notice is still kept, and the
  person still receives it if they ever connect under their own identity;
  what is new is that the operator learns a delegation is missing.

## 1.8.3 — 2026-07-30

### Fixed

- **The memory learned Italian from the prompts' own examples.** The
  language directive every memory-writing slot carries resolves
  correctly — *"User locale: en-GB. Respond in English"*, with no
  fallback warning anywhere in a full corpus build. The memory still came
  out Italian, because immediately below that instruction the model reads
  worked examples, and 68 of them were written in Italian. It followed
  the examples.

  Two symptoms, both from a corpus where every user is `en-GB` and every
  input turn is English: a standing behaviour rule stated in English was
  stored as *"Dammi risposte brevi, con ogni assistente"* — and, driven
  again from scratch, as a different Italian sentence, two attempts out
  of two; and one compiled page in twenty-two came out entirely in
  Italian, its description, its prose and both of its fact bodies
  rewritten from English rows.

  The failure rate tracked each prompt's own dose of Italian: 62 examples
  in the prompt that runs on every turn and owns the rule path, 2 in the
  one that writes the pages. This is the "English by default" path a
  deployment whose users all speak Italian never exercises.

  The remaining examples are now English, which **finishes** a pass that
  was 229 of 297 done and had been stalled at that ratio since the first
  public commit. Three places are adapted rather than translated, because
  translating them literally would have removed something the engine
  needs: the addressee test taught an Italian grammatical suffix and now
  names the English pronoun instead; the tone directive that demonstrated
  *revising* a standing rule relied on Italian's formal/informal pair and
  is now a formality pair that still revises; and the two lists of cue
  phrases — the sharing cues, and the relative-date phrases — existed to
  understand what an Italian-speaking user actually says, so they are now
  **language-neutral** ("in whatever language the user speaks", with
  English examples), which covers the languages the hard-coded pair never
  did.

  Every prompt and skill touched carries a version bump, so an install
  with a local override sees the drift banner instead of silently keeping
  the old body.

- **The first-login primer wrote the user's first memory in Italian.**
  The wizard is in English and composed its output — the identity
  clauses, the privacy and do-not-store rules, the preferences, and all
  three section headers it stamps — in hard-coded Italian, for every
  deployment. Now English, which is what the locale directive then
  renders into the user's own language.

- **The dashboard's chat help listed its examples in Italian**, and
  Italian examples sat in doc comments across ingest, recall, REM, the
  capture buffer, the fact index and the chat route.

- **A full test run no longer leaves its temporary directories behind.**
  46 sites deliberately leaked a `tempfile::tempdir()` so it would
  outlive the helper that made it; on a host where `/tmp` is RAM they had
  accumulated to roughly 20 000 directories and 12 GB.

## 1.8.2 — 2026-07-30

### Fixed

- **A failed snapshot could sit on disk looking like a good backup.** The
  snapshot writes the database first and the rest of the workdir second,
  in that order deliberately. But a snapshot is recognised — by the
  Backup console's list, by the CLI, and by the retention prune — as *a
  directory containing an `engine.db`*, which is the file written first.
  A run that aborted during the file copy therefore left behind a
  directory holding a complete, valid database and **no prose pages, no
  media, no configuration**: indistinguishable from a real backup, and
  unrestorable in the way that matters, because the media blobs are the
  one half the database cannot regenerate (it stores each file's hash,
  not its bytes).

  Any failure after writing begins now discards what was written — the
  whole destination when the run created it, otherwise just the database
  copy, since a directory the operator prepared is not the engine's to
  delete. A directory with an `engine.db` in it once again means a
  snapshot that finished.

- **One unreadable leftover in the workdir could fail every snapshot
  from then on.** A workdir accumulates files from hand-run maintenance,
  and any of them owned by another user with restrictive permissions —
  an archive written by a `sudo` pre-deploy backup is the ordinary case
  — made the file copy return "permission denied" and took the entire
  snapshot down with it. Automatic and manual runs alike, indefinitely,
  for one file the backup did not need.

  Such an entry is now skipped, named in the report, logged, and shown
  on the console: a snapshot that completed *minus something* is a
  different event from a clean one, and the operator is told which.
  Every other I/O failure — a full disk, a tree that vanished mid-copy —
  still aborts, because a backup that quietly stops early is the failure
  this whole area exists to prevent.

- **The Backup console's snapshot list now shows a file count**, flagging
  `DB only — no tree` at one file. Size alone could not tell a real
  snapshot from a husk: the database is written first and is most of the
  weight, so a run that captured nothing else still looked the right
  size.

### Changed

- **The recorded model-call log (`training-spool/`) no longer travels
  inside a snapshot.** A snapshot is the unit of *restore*, and an
  append-only observation log is not state a restore should roll back;
  it was also the largest growing thing in a workdir — 47 % of every
  snapshot on the maintainer's deployment, larger than the database
  itself — so retention held N rolling copies of one ever-lengthening
  file. It is excluded on the same principle as `logs/`. If the spool
  needs protecting it needs its own archive and its own retention, not a
  seat inside the recovery unit.

  A **restore** consequently no longer deletes it. The restore clears the
  workdir before moving a snapshot in, so both halves now read one
  shared list of what a snapshot never carries — otherwise excluding
  something from a backup would destroy it on the next recovery. A
  *memory reset* still removes the spool: wiping the memory means wiping
  the recorded conversations too.

## 1.8.1 — 2026-07-30

### Fixed

- **On macOS and Windows, a new page could be written inside an existing
  one.** Those filesystems treat `Intro.md` and `intro.md` as the same
  file, and three write paths decided whether a page "already exists" by
  asking the operating system whether the path resolves — which answers
  *yes* for a spelling that is not on disk. The guard that refuses
  case-colliding page names was therefore skipped exactly where it is
  needed, and the write landed in the existing page: two different pages
  merged into one, or a mirror push aimed at `Index.md` overwriting
  `index.md`. A capture aimed at `_Meta.md` appended a fact into the
  wiki's own `_meta.md`.

  The question is now asked of the directory listing
  (`wiki::page_exists_byte_exact`), which reads the same on every
  filesystem, and that is the only way a write path answers it. A server
  on Linux was never affected; the published macOS and Windows binaries
  were. The tests that cover this now pass on both kinds of filesystem —
  the macOS CI job is green for the first time.

## 1.8.0 — 2026-07-29

### Added

- **OpenAI is a backend of its own — your key goes straight in the box.**
  `backend: openai` builds, the dashboard offers it in the provider
  dropdown, and `OPENAI_API_KEY` has its own credential card. Until now
  the only route to a GPT model was an OpenRouter account, which means a
  margin on every call and a third party in the path your memory travels;
  for a product sold on where the data goes, that could not stay the only
  door. The adapter is written from the published API behaviour and pinned
  by tests that assert the exact request body: `max_completion_tokens`
  (never `max_tokens`, which the reasoning models reject), no
  `temperature` where the model refuses it, the system prompt on the
  `developer` role for a reasoning model and `system` elsewhere,
  `reasoning_effort` only where it is accepted, images as `image_url` data
  URLs, and the prompt-cache hit read from `usage.prompt_tokens_details`.
  `base_url` covers Azure OpenAI and corporate gateways.

- **The engine asks the model catalog what a model accepts, instead of a
  list somebody has to keep current.** `ModelPolicy` resolves per
  (backend, model) whether the sampling parameters go out, whether the
  output ceiling needs reasoning headroom, and what the model's documented
  maximum is — from `<workdir>/model-catalog.json`, which already
  refreshes from models.dev every six hours and carries `temperature` per
  model. That flag reproduces the hand-written Anthropic list exactly and
  covers the whole `gpt-5` line and the o-series as well. The hand lists
  survive only as the offline answer for a model the catalog has never
  heard of, and the catalog is now republished to the running process
  after every refresh, so a model released this morning is understood six
  hours later rather than at the next restart.

- **A provider's own refusal is now a repair, not a failure.** An HTTP 400
  naming a sampling parameter costs one retry with it stripped, remembered
  for the rest of the run and warned once; on OpenAI a 400 naming the
  message role retries with `developer` ⇄ `system` swapped. This is what
  lets a model no catalogue has heard of answer at all — and it let the
  default boot probe start sending the temperature the hot paths pin, so
  boot exercises the request shape the engine actually sends.

### Fixed

- **A photo the provider cannot read costs the photo, not the turn.** A
  client that declares `image/jpg` — not a MIME type; the registered name
  has always been `image/jpeg` — was accepted by Gemini and refused by
  Anthropic with HTTP 400. Because the image rides the classifier call,
  that 400 was logged as "LLM unavailable" and the whole message was never
  classified: one turn of memory lost over a misspelling, and it was the
  declared type on *every* photo in one production catalog. The type is
  now normalised on the way into the media catalog and again on the way
  out to a provider (so photos already catalogued are covered without a
  migration), only the encodings every provider reads are sent, and
  anything else is dropped with a warning while the caption still files
  the fact.

- **A text-only model is no longer told about photos.** Whether the
  configured model accepts images is answered from the catalog before the
  request is built, so an ingest slot that cannot see degrades to the
  caption instead of paying for a rejection that takes the turn with it.

- **Models that reason unbidden get room to answer.** The Claude 5
  generation reasons whether or not a `thinking` block was requested, and
  spends the caller's `max_tokens` doing it — a response could come back
  with a thinking block and no text at all. The output ceiling now carries
  headroom for those models, clamped to the model's own documented
  maximum, and the boot probe no longer refuses to start a deployment
  whose strong slot thinks before it speaks.

### Changed

- The model catalog covers `openai` alongside `anthropic`, `google` and
  `openrouter`, and the vendored snapshot has been refreshed.
- `LlmFunctionConfig::build_backend` now materialises five backends; a tag
  outside that set still fails at startup rather than at the first
  request.

## 1.7.0 — 2026-07-29

### Added

- **An agent's own wiki now says so, and the whole pipeline reads it.**
  `is_agent` existed on `FamilyScope` but only the dedup revisor looked at
  it. It is now flattened to a `wiki_id → bool` map that completion,
  contradiction and page-merge all consult, and it reaches the
  classifier's `known_users` roster as well: one entry can be marked as
  the assistant itself, with the rule that a human name never resolves
  onto it and a name *addressed* to the agent is vocative, not
  attribution. `smart_bootstrap.is_self` no longer guesses from the slug —
  it requires the marker (or the legacy label) and repairs it on the
  caller's own operating wiki — and `wiki_admin_push` reserves the `agent`
  label for a consumer's own wiki. The page-merge guard matters most:
  `slug_kinship` was proposing one person's page and another's as twins
  because they share a token, and merging them would have undone the
  per-person separation the engine works to keep.

- **The autobiographical voice follows the page's subject, not the wiki.**
  An agent's wiki is its autobiography and gets the first-person voice —
  but measured against a real deployment, 31% of the facts in one agent's
  wiki turned out to be about other people, residue from before the
  routing guard existed. Compiling those in the first person would have
  had the assistant narrate someone else's life as its own. So the tone is
  decided per page (`compiler::tone_for_page`, by majority of fact
  ownership), and misrouted residue keeps the ordinary identity voice.

### Fixed

- **A slot that passed the boot health-check could still fail every real
  call.** `temperature` is deprecated for the Claude 5 generation, which
  was missing from the list of families that reject sampling parameters —
  so with Sonnet 5 behind the navigator, every recall returned HTTP 400
  while the boot line read *all configured slots reachable*. The failure
  is silent by design (`recall_nav` keeps the partial walk and logs a
  warning), so recall degraded without anything failing. Both halves are
  fixed: the Claude 5 family is on the list, **and the probe now pins the
  temperature the hot paths use**, so an unlisted family that drops
  sampling params fails at boot instead of degrading every answer. A probe
  gentler than production is not a check.

- **The navigator retries once when the model answers with no text.** Two
  of 276 calls in a corpus rebuild came back with no text block at all —
  the budget went to a thinking block — and each silently cost that turn
  its navigation. Retried on protocol, transport and backend failures
  only; a 400 reproduces exactly and auth or rate-limit errors want the
  operator or a back-off window instead.

- **A fumbled model response no longer costs the whole night.** The dedup
  revisor was the only one of the four confirmers that propagated a
  per-pair error, and `dream::run_full` aborts the cycle on any error from
  `rem::run_cycle` — so one bad response skipped promotion, reorg and
  every queued recompile, with the next attempt 24 hours away. The pair is
  now skipped as a soft error (no negative verdict memoised, so it stays a
  candidate) and only five *consecutive* failures stop the cycle, which is
  an outage rather than a fumble.

- **Facts about the assistant, said by a person, reach the assistant's
  wiki.** The `self` sentinel only fires on the assistant's own turn, so
  on a user turn ("you are good with paperwork") the fact arrived at the
  routing guard owned by the agent and was dropped. Also: with two bots, a
  fact about B aimed at A was binned instead of redirected home; and the
  marker check walked the entire tree on every request when the wiki did
  not exist.

### Changed

- **The ingest classifier declares its system prompt cacheable.** It is
  the engine's heaviest repeated caller by a wide margin — 4.84M prompt
  tokens against 56k of output over a 174-call corpus rebuild — and its
  system half is the bundled prompt with only the locale substituted,
  while everything per-turn rides in the user half. That split was already
  right; the flag was simply never set, so every call paid full price for
  a prefix it had already sent. Measured on a live run afterwards: 27,077
  of 28,174 prompt tokens served from cache on the second consecutive
  turn. The one-hour window means the discount lands on bursts — a
  conversation, a replay, a REM night — while an isolated turn arriving
  after it expires pays the write surcharge instead.

- **A smart consumer's operating wiki is the agent's to maintain.** REM
  skips every write job on a smart wiki by design, so nothing tidies it
  but its owner. The `smart-consumer` skill (1.15.0 → 1.16.0) now says so
  and describes the maintenance: merge repetitions (never two twins about
  different people or occasions), retire what stopped being true, re-cut
  pages when the content moved, throw away scaffolding — bounded to the
  pages the session already touched, on the push it was about to make
  anyway.

### Operational notes

- A restart resets the REM timer (`initial_delay_secs: 300`), so every
  deploy triggers a full cycle five minutes later. Worth knowing when
  restarting with dirty pages queued.

## 1.6.0 — 2026-07-28

### Added

- **Memory is now written in the language its owner declared — on every
  slot, not just the two that asked.** `enrollment_users.locale` has
  driven the LANGUAGE directive since migration 0020, but nothing ever
  wrote it: the welcome wizard turned its `language` answer into a prose
  clause and left the column NULL, and the users page had no field at
  all, so an admin who wanted a household's memory in its own language
  had to reach for `sqlite3`. The column is now a real field on both
  dashboard forms and a required wizard answer, pre-filled from
  `Accept-Language` and written *before* the primer is ingested, so the
  primer's own facts come out in the language just declared. And the
  directive reaches the slots that actually compose prose: eleven
  prompts put natural language in front of a person and only two were
  told which one to use — the rest answered in the language of their own
  few-shot examples, which are Italian, so an English deployment got
  Italian page titles and Italian document summaries over English facts.
  The Cronista, both hub writers, comment-apply, the three
  document-ingest phases and the REM cartographers now all carry it. A
  wiki's language is its scope principal's: a user wiki speaks its
  owner's declared language, a group wiki speaks the one every member
  declared and has none when they disagree. Undeclared resolves to
  "mirror the user's message" on the two slots that answer a live turn,
  and to English on the compiling slots, which never see a turn to
  mirror. `prompts::PROSE_REGISTRY` classifies every bundled prompt as
  prose or internal and **fails the build** on one it does not name, so
  a new slot cannot be merged without answering the language question.
- **Three REM slots regrouped so a language directive can mean
  something.** The cartographer, the conciliator and the date normaliser
  each batched the whole forest into one call, where a statement about
  one language says nothing. Each now groups by source wiki before
  chunking. The batch is what narrows — the *context* each model is
  shown is unchanged, and the normaliser still spends the same per-cycle
  cap — with the side benefit that one wiki's transport failure no
  longer sinks the whole sweep.
- **The prompt cache is measurable.** `CompletionUsage` gains
  `cached_prompt_tokens`, folding Gemini's `cachedContentTokenCount` and
  Anthropic's `cache_read_input_tokens` into one field whose meaning is
  written down (both are *inclusive* of the prompt count beside them),
  with a per-call hit-ratio log line and the value recorded on the
  training spool. The finding it produced: the cached span is
  **block-quantised** at ~4 090 tokens, so a shorter prompt can cost
  *more* — trimming 6.4% of the `ingest` prompt lowered the bill by
  1.1%, while deleting a section from its middle cut 13.4% of the tokens
  and raised the bill 26.1%, by dropping the cached prefix from six
  blocks to four.
- **A replay differential for prompt changes**
  (`examples/ingest_replay.rs`): replays recorded production requests
  against a modified system prompt and diffs the resulting plans field
  by field, with resume, bounded retry and a compare mode. Its first
  result is about the classifier rather than any prompt — replaying the
  same turns twice against the *unmodified* prompt agrees on 51.2% of
  fields and 45.7% of capturing turns, because Gemini 3 mandates
  `temperature: 1.0`. Every prompt A/B has to be read against that noise
  floor.

### Fixed

- **An agent's diary stops scattering into its users' pages.** Part 12's
  `owner_id: "self"` routes a fact the agent states about *itself* into
  its own wiki, and the engine matched that sentinel as a literal
  string. A model that writes its own principal instead
  (`user:<agent>`) is making the identical claim, but fell through to
  the normal path and filed the diary entry in whatever wiki
  `target_wiki_id` named — 40 agent-owned facts sitting in their users'
  wikis on the reference deployment. Both spellings now route to the
  self path. The alias cannot misfire: the agent principal resolves only
  on a turn that agent authored, so on a user turn an owner naming the
  agent keeps its ordinary meaning.

## 1.5.6 — 2026-07-27

### Fixed

- **A section *titled* with the query now outranks one that merely cites
  it.** 1.5.4 claimed identifiers rank their defining section first; the
  live check found otherwise, and the claim is corrected here in both
  senses. What was true: the lexical pass ranks the definition first
  (7 of 7 on the reference store). What was not: the *fusion* then put a
  citing section back on top, because a section quoting `D-006` is in
  **both** lists — leading on cosine and two places behind lexically,
  which reciprocal rank fusion cannot recover from at any `RRF_K` or
  lexical weight (both are monotone in a rank gap of two). The fix is a
  **tier, not a knob**: a second, sharper index question — which sections
  carry *every* query term in their heading — and those outrank every
  citation. It uses `AND` where the ranking pass uses `OR`, so a prose
  query matches no heading and the tier goes quiet; verified on the live
  corpus, where the identifier query promotes exactly the two defining
  sections and a five-word prose query promotes none.

## 1.5.5 — 2026-07-27

### Added

- **First connect is its own skill, and the server says when it
  applies.** `smart_bootstrap` accepts the exact `project_id` a consumer
  derives from its working directory and answers a `first_connect` block:
  the wiki to resume, or one line pointing at the new bundled
  `smart-onboarding` skill when this project has no memory yet. The
  procedure — the intro, the faithful import of existing documents, the
  post-import report, the page repair — moved out of the three places
  that carried it and is fetched only by the sessions that need it; the
  everyday skills shed 452 lines. The trigger had to move to the server
  for the split to be safe: a procedure behind an extra fetch, gated on
  an agent remembering to fetch it, is easier to skip than one already
  open.
- **Page shape is measured and reported.** `wiki_admin_push` returns a
  plain-language `warnings[]` line for each written page whose blocks are
  too long for the index to keep whole (they are cut mid-sentence, and
  several sections end up under one heading with different content), and
  `wiki_admin_pull` accepts `shape: true` to report a whole wiki —
  sections, over-cap blocks, the share of the page they hold, a per-page
  note and a summary — without returning any content. Both are derived
  from the bytes by the same segmentation the indexer runs, so they are
  correct while section indexing is still queued. The trigger is density,
  not size: three over-cap blocks, or a quarter of the page.
- **`wiki_admin_pull` accepts `paths`** to narrow a pull to named pages —
  the narrowing the smart-consumer skill had documented since the MVP.

### Changed

- **`signpost_hint` no longer fires on a consumer's own operational
  wiki** (`wiki_type: agent`). Signposts exist so a conversational turn
  can discover *projects*; nudging an agent to signpost its private
  working memory only added noise to the owner's `@projects.md`.
- **Create-mode errors say what to pass.** A parent-less smart-wiki
  create now names the caller's own root wiki id in the message instead
  of stating only that top-level is not allowed; the `title` and
  `wiki_type` errors say what those fields are for.

### Fixed

- **The skill told agents to notify their own wiki, which the server
  refuses.** Writing your own `_briefing.md` is an ordinary push;
  `wiki_admin_notify` is how *others* reach it. The documented
  "note to next session" flow had been impossible as written.
- **Two documented behaviours that did not exist**: the folder-structure
  deviation warnings described in three documents (no validator was ever
  written — `warnings[]` now carries page shape instead) and the `paths`
  argument above.

## 1.5.4 — 2026-07-27

### Added

- **Exact-term matching fused into the section ranking** (migration
  `0065`). Recall was pure vector, so a query that *is* an identifier — a
  decision code, an ADR number, a ticket id, a file path — ranked worst
  exactly where a project's decision log lives. `wiki_sections_fts`
  indexes `heading_path` as its own 4×-weighted column beside the text
  and the three section entry points fuse the two rankings by position
  (RRF). Both passes always run: a per-query gate would spend a model
  call to guard a sub-millisecond index lookup, and a wrong "no" drops
  the hit with nothing to notice. The fusion reorders only — a hit's
  `score` keeps its cosine meaning — and the signpost floor still runs
  before it, so an `OR` match on an ordinary sentence can never start a
  dig into project documentation. Measured on the reference store (4 220
  sections): 7 of 7 decision identifiers now rank their defining section
  first, prose queries unmoved.
- **REM regroups pages into sub-wikis.** The nightly promotions slot
  reads a wiki's whole page inventory once per cycle and cuts the groups
  of pages that are *already* one subject area: a group founds a new
  sub-wiki (floor `auto_promote_group_min_pages`, default 9) or moves
  into one that already exists (no floor). The trigger is evidence on
  disk rather than a forecast about one page's mass.

### Changed

- **Deleting a wiki defaults to Dissolve**: the structure goes, every
  fact stays. Facts are evacuated to a live home and their pages parked
  on the compilation plan as `reopen_pages`, so the next narrative build
  re-decides where each fact belongs corpus-wide instead of letting it
  inherit the page it happened to sit on.

### Removed

- **Page → sub-wiki emergence**, its prompt, and the
  `auto_promote_subwiki_min_page_facts` knob — superseded by the
  page-group pass above. A wiki is born holding every page of its
  subject, so it can never be born with a single page.

## 1.5.3 — 2026-07-26

### Fixed

- **The section cap was bypassed by a heading whose body starts on the
  next line.** The prose segmenter splits on blank lines, so a heading
  followed immediately by its text — a changelog entry, a table, a dense
  list — is a *single* paragraph; the heading branch pushed those
  trailing lines into the packing buffer without ever applying
  `segment_max_chars`. Only the plain-paragraph branch enforced it. Both
  bodies now go through the same packing helper, so the cap holds for
  every shape. Observed on the reference store immediately after the
  1.5.2 deploy: two pages of that shape kept sections of 6 994 and 5 239
  characters through a full re-cut. Document ingest shares the segmenter
  and gains the same guarantee, which its own knob already promised.

## 1.5.2 — 2026-07-26

### Added

- **Smart-wiki documentation moved out of the fact store** (migration
  `0062`, plus `0063`). A project wiki's pages are chunked into
  `wiki_sections`, with read access held once per wiki in `smart_wikis`,
  instead of one governed `fact_index` row per chunk. On the reference
  store that was 72% of the fact table and **75.6% of the characters** a
  conversational turn spent on recall; a turn now recalls facts only, and
  a sharing change is a one-row write instead of one row per section. The
  data move is an idempotent boot pass that copies embeddings verbatim —
  no re-embedding, no migration downtime.
- **Project awareness: the everyday agent learns that a project exists.**
  A smart consumer writes short **signposts** into its owner's own wiki
  through the new `wiki_admin_signpost` (H family) — one plain-language
  description per project, plus one activity line per day over a rolling
  5-day window, on a reserved `@projects.md`. Length caps are enforced
  server-side and an over-long field is refused with its measured length,
  never truncated; rewriting an unchanged signpost is a no-op, and
  `wiki_admin_push` answers with a `signpost_hint` when something is
  missing. When a signpost surfaces in a turn, that project's
  documentation can be opened for that turn — so a question that never
  names the project can still reach it.
- **The dig is a judgement, not a threshold** (ingest prompt v2.45). The
  classifier now answers, in the JSON it already returns, whether a
  signposted project's documentation would help answer this turn — at no
  extra model call. Built this way because the threshold version was
  measured first and no similarity signal separated "a customer says the
  content is frozen" (needs the docs) from "I have an appointment at that
  customer" (does not).
- **Dashboard: a Sections tab** in the memory browser, the smart half of
  the corpus, mirroring the Standard/Smart split of the wiki explorer.

### Changed

- **The nightly cycle stops re-buying the verdict it already has**
  (migration `0064`). Every REM confirmer asks about a stable piece of
  the corpus and mostly hears "no", and nothing recorded that "no": a
  single cycle spent its whole confirm budget re-judging the same pairs,
  so the backlog past the cap had never been examined once. Negative
  verdicts are now memoized for all seven confirmers, keyed by a hash
  over the slot's model id plus the rendered prompt — an edited fact, an
  edited prompt or a repointed model all invalidate themselves, with no
  hand-bumped cache constant. Positives are never stored. The same budget
  now drains the backlog instead of circling it.
- **The compiler stops re-buying its own standing brief** (`cronista`
  v1.14). Input, not output, was ~70% of a page's compile cost, and 97%
  of that input was the same brief and page index re-sent per page. The
  prompt now ships in two halves — the shared block as a cacheable system
  prompt, the page's own facts on the user turn — with a 1-hour cache
  window, because a compile run outlives the 5-minute default. A rejected
  request (invalid / auth) no longer buys a retry.
- **`rem-promotions` v2.1** presents facts as positional handles instead
  of UUIDs (~18 tokens of noise per fact on the most expensive slot);
  raw ids still resolve.
- **Sections are cut for retrieval, not for extraction.** Smart-wiki
  pages are chunked at 1 200/2 000 characters instead of the
  document-ingest 3 000/4 500 they used to borrow. A section is ranked by
  one embedding and quoted whole into a bounded recall slot, and at
  ingest sizes one oversized hit consumed the entire slot on its own —
  25% of sections were larger than the slot, the largest 6 994
  characters. The re-cut needs no migration: the next reindex sweep
  re-chunks any page whose stored cut no longer matches.

### Fixed

- **A documentation paragraph can no longer be filed as a fact about the
  user.** The rule forbidding it had been placed inside the prompt
  section that applies only to assistant-authored turns, whose opening
  line tells the model to ignore the whole part otherwise — so it was
  inactive on exactly the turns that carry documentation. It is now a
  turn-level rule.

## 1.5.0 — 2026-07-23

### Added

- **The reverse channel now tells the affected human.** When a turn (or a
  document upload) files a fact owned by an enrolled user who was *not* the
  human of that conversation, the engine emits a new `fact_minted_for_you`
  event carrying the fact bodies themselves — batched one notice per
  beneficiary — so a consumer can deliver the content to its subject instead
  of leaving them to stumble on it at their next recall. This is the server
  half of the consumer-push contract (`INTEGRATING.md` step 8).
- **hermes bridge: the reverse-channel half, zero fork.** A `mwe-events`
  gateway hook (auto-discovered from `$HERMES_HOME/hooks/`) drains
  `fact_minted_for_you` every ~30 s and delivers each notice to its
  recipient's private chat as an agent-composed message, routed through a
  one-shot cron job on hermes's own scheduler; a `mwe-daily-digest.py` cron
  script batches every other event kind into a once-a-day recap. Built
  entirely on supported hermes seams — no upstream patch.

### Fixed

- **A fact can no longer be owned by a principal that does not exist.** Both
  ingest paths (conversational and document) now check the classifier-emitted
  owner against enrollment before filing: an owner that resolves to no
  registered user/group is re-owned to the sender (or the uploader), closing
  the gap that let a fabricated subject take ownership of a memory. An
  enrolled third party — the legitimate subject of a fact about someone
  else — passes untouched.
- **Owner attribution on assistant turns follows the subject, not the
  speaker (ingest prompt v2.43).** Advice the agent synthesises *for* an
  enrolled user is owned by that user, decided deliberately via a necessity
  test rather than guessed from a mention; and the fact body narrates the
  advice passing through the sender instead of asserting an interaction with
  the absent subject (the wording that made a relayed plan read as a false
  memory).

## 1.4.8 — 2026-07-20

### Fixed

- **The agent's own memory now counts as "used" when it is used.** The
  agent's self-memory (identity + history with the current user) is injected
  into every turn, but the injection path never updated the recall counters —
  every self-fact read as never-recalled forever, hiding real usage from
  metrics and from recall-weighted REM decisions. The agent-self recall path
  now bumps `last_recall_at` / `recall_count_30d` for each fact it surfaces,
  exactly like the standard recall path.

## 1.4.7 — 2026-07-20

### Fixed

- **Nightly memory reorganization (REM) was silently inert for every
  standard wiki.** The auto-promote pass that splits an over-long page into
  sections or a dedicated sub-wiki skipped every candidate
  (`candidates_examined: 0`), so structure never emerged organically. Its
  skip-gate matched a `wiki_promote` proposal by kind plus a blind substring
  of the fact id — but that kind is overloaded: routine fact-lifecycle
  operations (validity close, refile, ACL change) share it, so any page
  holding one ever-touched fact was permanently marked "already promoted."
  The gate now counts only genuine page-promotion receipts (paragraph→page,
  page→sub-wiki), scoped to the receipt's own source page, so a fact that
  later migrated onto another page no longer freezes it.

- **The agent could "rename itself" from a mis-heard command.** Ingest read
  the agent's name used as a form of address inside an unrelated command
  ("Gandalf, turn it down" — mis-transcribed) as an explicit rename. A naming
  rule now changes the agent's name only on an explicit naming predicate
  ("your name is X"); a vocative address never renames (ingest prompt v2.40),
  with no edit-distance heuristic.

- **A user's or group's fact could be filed into an agent's own wiki**,
  fragmenting that principal's memory across two wikis. The capture planner
  now redirects a non-`self` fact aimed at an agent wiki to the owner's own
  wiki (or drops it rather than misfiling), via a new per-wiki `is_agent`
  signal. Owner (audience) and physical wiki stay decoupled otherwise.

### Changed

- **The agent's self-memory (its "diary") is now organized per served user.**
  Relationship self-facts are written to a per-user page
  (`esperienze_<user>.md`) and identity self-facts to the agent's index,
  instead of accumulating on one heterogeneous catch-all page.

## 1.4.6 — 2026-07-20

### Added

- **Per-user timezone — two users, two places, both right.** One
  deployment-wide `recall.ingest_timezone` is wrong the moment two
  users live in different zones (London and Sydney hear "tomorrow
  at 9" eleven hours apart), so the reference-time zone now resolves
  **per sender**: `enrollment_users.timezone` (new migration 0061) wins
  over the deployment default, which stays as the fallback; unset both,
  spoken times read as UTC as before. The admin sets it on the users
  page (create + edit); the welcome wizard's existing timezone question
  now also lands in the column (it previously became only a memory
  fact — the stamping plumbing reads the column, never the memory).
  A per-turn zone from the consumer (device time, covers travel) is a
  tracked protocol extension.

## 1.4.5 — 2026-07-20

### Added

- **Server-settings sections on the Settings page — no more YAML-only
  config.** Every typed config section without a dashboard surface is
  now an admin-only section of `/dashboard/settings/me`, closing the
  gap survey: **Ingest timezone** (`recall.ingest_timezone`, hot —
  swapped into the shared recall handle so the next ingest turn stamps
  wall-clock times in the deployment's zone), **Dream cadence**
  (`rem.schedule:` — mode, full/light intervals and initial delays,
  the light-dream backlog trigger), **Logging** (`logging:` — level,
  file rotation, file path), and **Document pipeline** (`document:` —
  segmenting, extraction caps, worker cadence, merge threshold). Same
  atomic `.bak`-guarded `load_raw` round-trip as the other editors;
  the boot-read sections say so and point at the Backup console's
  Restart button. The REM-settings and recall-settings panels now
  cross-link instead of declaring those keys YAML-only.
- **Recovery surfaces — automatic snapshots, dashboard restore, safe
  memory reset (roadmap 4d).** A new `backup:` config section (on by
  default: daily, retention 7) drives an automatic-snapshot scheduler:
  a due-check loop that hot-snapshots the whole workdir into a
  snapshots home (default: the `<workdir-name>-snapshots` sibling),
  prunes the oldest `auto-*` snapshots beyond the retention, persists
  its last-run stamp in `engine_meta` (a restart never re-fires inside
  the interval), and reports its outcome to the dashboard. The Backup
  console grows into the full recovery surface: settings editor
  (hot-swapped, `.bak`-guarded), the snapshots-on-disk listing with
  provenance badges, per-snapshot **Restore…** / **Delete**, and a
  type-`RESET`-to-confirm **Memory reset**. Restore and reset are
  **staged**: a one-shot `recovery-pending.json` marker (excluded from
  snapshots) that the next boot applies under the lockfile — automatic
  safety snapshot first, refusal-leaves-untouched, outcome persisted
  for the console — because a live server cannot safely replace its
  own open workdir. Reset wipes the memory tables and the
  `wikis/`/`media/`/`training-spool/` trees while preserving accounts,
  enrollment, consumers, tokens, 2FA/OAuth state, custom skills,
  config, env and prompt overrides; identity wikis are re-scaffolded
  empty and `profile_initialized` is cleared so the welcome wizard
  re-seeds each profile. A "Restart now" button applies a pending
  recovery from the dashboard: graceful shutdown, then exit code 75
  (`EX_TEMPFAIL`) so a `Restart=on-failure` systemd unit relaunches.
- **Verbatim source promotion — pasted text becomes a cited document
  (roadmap 46).** The server now backstops the media-first routing:
  document-shaped inline text is promoted to the media rail — the text
  is materialised verbatim as a content-addressed blob +
  `media_catalog` row (kind `doc`, `text/plain`) and the document
  pipeline runs against it, so extracted facts cite `source_ref =
  catalog_id` and the dashboard serves the preserved original, exactly
  like an uploaded file. Two doors, one deterministic shape heuristic
  (email headers, forwarded banners, quote/markup density,
  greeting/sign-off, size): `wiki_ingest_external source.type=inline`
  (response: `promoted_catalog_id`; `dry_run` reports `would_promote`)
  and an oversized `wiki_ingest_message` turn — the paste-into-chat
  case — which is archived + enqueued as a document job while the
  conversational ingest sees a bounded excerpt plus the promoted
  document as a linked attachment (response: `document_promoted`).
  A new `promote: always | never` dial on both tools forces the
  decision either way, disposition-style; guests, `dashboard_command`
  and assistant-authored turns never promote.
- **Behaviour-rule scopes — the user's rule for every assistant
  (roadmap 42).** The `behaviour_scope` axis grows a third value,
  `user-global`: a directive the user explicitly addresses to every
  assistant they talk to ("voglio che TUTTI gli assistenti mi parlino
  in italiano") now files in the **sender's own identity-wiki
  `@rules.md`** (owner = the sender, no admin gate — it binds only
  their conversations) and the `rules` channel of every consumer
  serving that user surfaces it, the bindingless smart consumer
  included. Order pinned in `YOUR RULES`, most specific last:
  agent-wide → user-global → per-user. The classifier sees the union
  with fact ids and scope tags, so a revision supersedes across all
  three sources — but only the admin may supersede an agent-wide rule
  (a non-admin's revision files additively at its own scope). This
  retires the old salience-`high` workaround for cross-assistant
  directives, and the governance read (`sender_rules`) now strips fact
  regions from the user's `@rules.md` so a user-global rule never leaks
  into the classifier's policy section.

### Fixed

- **Dashboard saves no longer persist `MWE_LLM_*` env overrides.** The
  config gains a `load_raw` round-trip primitive (file contents
  verbatim, no env overlay); every dashboard section editor now loads
  through it, so saving an unrelated panel can never bake a
  runtime-only override into the YAML. The LLM-config editor remains
  deliberately what-you-see-is-what-you-save.
- **hermes `mwe-watchdog` 0.2.0 — two verification gaps closed.** The
  watchdog now hashes the whitespace-trimmed turn text (the memory
  provider's canonical form), so a padded gateway message can no longer
  silently skip its handshake entry; and it requires the
  `<memory-context>` fence on the request's **last user message** —
  host injection is API-call-time only, so a fence on an earlier
  message is a stale injection index landing on the wrong turn and now
  counts as a miss.

## 1.4.1 — 2026-07-19

### Added

- **Training spool — teacher traces for local-slot distillation.** When
  `training_spool.enabled` is on (default off), every internal-LLM
  exchange — any slot, any backend, every transport (MCP ingest, REM
  cycle, dashboard chat) — is recorded verbatim as one JSON line (slot,
  backend, model, full request, full response, finish reason, token
  usage) into per-day files under `<workdir>/training-spool/`. The
  strong API slots act as teachers; their traces become the dataset for
  fine-tuning the local workhorse on mwe-mcp's own structured tasks.
  Recording is a decorator inside `build_backend` (no call-site
  changes), best-effort (an I/O failure never fails the turn); health
  probes and failed calls are excluded, images ride as MIME-only. New
  admin dashboard panel `/admin/training-spool` ("Spool" in the nav):
  checkbox with the atomic-YAML + `.bak` save idiom, hot-flip of the
  running recorder (no restart), on-disk inventory, and the privacy
  stance (the spool embeds recalled memory content — it stays on the
  host; scrub before sharing a dataset). See
  [`llm-functions.md` §6](docs/design-notes/llm-functions.md).
- **hermes bridge: `mwe-watchdog` verification plugin (trio → quartet).**
  Out-of-tree hermes plugin that verifies the mwe recall block actually
  reaches the model each turn — born from a silent memory-blackout
  incident where the host's stale injection index dropped the
  `<memory-context>` block after transcript repair. See
  [`agents-bridges.md`](docs/development/agents-bridges.md).

## 1.4.0 — 2026-07-15

### Added

- **Fresh-session resume: a blank-context requester is served its own
  surface.** A `wiki_ingest_message` turn that carries no `recent_messages`
  has no local context a served thread could duplicate — a reborn/blank
  session (e.g. a hermes gateway session silently reset by idle-expiry,
  upstream hermes-agent#43008) or a consumer that keeps no window at all.
  Such a turn now receives the cross-consumer recent window **including its
  own surface**: the thread the user is continuing, minus the message being
  spoken (the window fetch runs before the turn's own buffer write). Turns
  that bring their window keep the self-echo exclusion unchanged. A consumer
  on this contract never wakes up amnesiac — session resume with no
  host-side support.

## 1.3.0 — 2026-07-15

### Added

- **Cross-consumer recent window — the thread of discourse follows the user.**
  The server now retains a bounded serving buffer of the exchanges the
  per-turn ingest already receives (per user, hard cap `recent_window_entries`
  = 32 AND TTL `recent_window_ttl_hours` = 4, enforced in the write path;
  never indexed, never embedded, never REM-processed; deleted with the user)
  and every `wiki_ingest_message` response serves it back as the
  self-labelled `recent_window` field: the user's live thread from their
  OTHER surfaces, entries tagged with relative age and origin
  (`[2 min ago · via <consumer>/<channel>] user: …`), oldest first, newest
  winning the `recent_window_chars` (1200) budget, headed by an explicit
  do-not-re-answer framing. Consumers declare their surface with the new
  optional `metadata.channel` label; self-echo is excluded by
  (consumer, channel) — whole consumer when no label is sent. Windows never
  cross users. This restates the no-transcript invariant as *no unbounded
  transcript*: the buffer serves the live thread (minutes-to-hours), while
  long-range continuity stays with recalled facts.
- **hermes bridge, memory plugin 0.2.0** — sends the gateway key as
  `metadata.channel` and injects `recent_window` verbatim between the rules
  and the recalled facts.

### Fixed

- **Dashboard Facts pager: real ACL-projected totals and an editable page
  box.** Prev/next now derive from the real filtered total instead of a
  page-size heuristic, the page number is directly editable (jumps preserve
  the active filters), the disabled state reads "of M", and totals beyond the
  scan ceiling render as an "M+" estimate.
- **hermes bridge: `compression.in_place: true` is withdrawn — rotation mode
  (the vanilla default) is required.** hermes-agent's in-place compaction
  path re-appends the whole compacted window into the same active transcript
  after a preflight cut (its flush bookkeeping resets and the turn's history
  reference is nulled), doubling the conversation; the model then re-answers
  the replayed tail — observed live as a Telegram bot answering yesterday's
  messages. The bridge no longer recommends in-place anywhere; with rotation
  the same re-append lands in the freshly rotated session, where it is
  correct behaviour.
- **mwe-truncate 0.3.0: oversized tool results are snipped on a cut**
  (`snip_tool_chars`, default 4000). The window bounds *turns*, not *weight* —
  browser-tool spam kept the bounded window permanently above the compression
  trigger (fire-abort on every call, one session crash-looping at ~328k
  tokens). Snipping is copy-on-write (the rotated-out archive keeps full
  contents) and never touches the tail from the last user message onward; a
  snip-only pass must save ≥8% or it reports a no-op through the abort
  protocol.

## 1.2.0 — 2026-07-14

### Added

- **Deployment timezone for the ingest classifier** — `recall.ingest_timezone`
  (or the `MWE_INGEST_TIMEZONE` env var) names the users' IANA timezone (e.g.
  `Europe/Rome`). When set, a bare wall-clock time a user speaks ("alle 16") is
  read in that zone and converted to UTC for a fact's validity interval,
  instead of being stamped verbatim as UTC — which drifted every dated
  commitment by the local offset, so deadlines expired late and stale plans
  resurfaced as if still current. Unset keeps the prior UTC-only anchor. The
  DST-aware conversion is delegated to the classifier; no timezone database is
  compiled in.

### Changed

- **Relationships between people now reach the always-on identity core.** A
  statement of who someone is to someone else ("X is Y's partner / parent /
  child") is classified as identity core (`bio` / `high`), extracted
  reciprocally when both are enrolled, and shielded from dedup and
  contradiction retirement — so an agent stops confusing who is who across a
  family or a team.

### Fixed

- **hermes bridge: the per-turn recall block no longer injects
  `suggested_seed`.** A consumer that brings its own model was handed a
  pre-drafted reply inside the user turn, which a weaker model could adopt or
  continue — laundering the ingest classifier's guesses into the agent's
  replies. The bridge now surfaces only the recalled facts.

## 1.1.2 — 2026-07-14

### Fixed

- **An orphaned identity wiki can now be deleted (admin).** Deleting a
  user keeps their wikis (the sender-scrub invariant), but the
  identity-wiki guard refused deletion unconditionally ("remove the
  user/group instead") — a dead end once the user was already gone. The
  refusal is now scoped to *living* principals: an identity wiki whose
  user/group is no longer enrolled is deletable from the dashboard like
  any other wiki, with the same typed-id confirmation and move/tombstone
  dispositions. (#4)

## 1.1.1 — 2026-07-14

### Fixed

- **The binary self-reports its release version again** — the v1.1.0
  artifacts printed `1.0.0` because the workspace version was not bumped
  at release time.
- **Deleting an enrolled user no longer 500s when the identity is bound
  to a consumer.** `consumers.system_user_id` is a plain FK with no `ON
  DELETE` action, so the dashboard's bare `DELETE FROM enrollment_users`
  was rejected by SQLite for any identity registered as a consumer's
  system user. Deletion now goes through `enrollment::remove_user`, one
  transaction that dismantles everything hanging off the identity:
  consumers system-bound to it (registration row, delegation grant,
  web-agent OAuth rows), the user's own OAuth codes/refresh rows (a live
  refresh row would keep minting tokens for a vanished sender), then the
  enrollment row (CASCADE clears credentials, invitations, 2FA, votes).
  The delegation cache is refreshed post-commit so act-as dies on the
  next call. The deleted user's *memory* outlives the identity: their
  wikis stay, and facts they authored are re-pointed at the containing
  wiki's scope principal (the sender-scrub invariant).
- **hermes bridge — the `mwe-truncate` context engine now actually bounds
  the conversation window** (plugin 0.2.0). Its only trigger was hermes's
  `threshold_percent` (0.75 of the model context — a summarization
  default), so on a million-token model the first cut sat at ~786k prompt
  tokens: far beyond per-minute provider token quotas, which a long-lived
  session exhausted first. The window is now counted in recent **user
  turns** (`protect_last_users`, default 5 — cut at a user-message
  boundary, so tool-call pairing holds by construction) with a slack
  (`slack_users`, default 3) that keeps the prompt prefix cache-stable
  between cuts, and the trigger is capped in absolute tokens
  (`threshold_tokens_cap`, default 30k). A no-op fire reports through the
  host's abort protocol instead of rotating the session; pair with
  hermes's `compression.in_place: true` (see the bridge README). The
  `protect_last_n` config key is retired (logged and ignored).

## 1.1.0 — 2026-07-06

*(Section reconstructed after the fact — 1.1.0 shipped without a
changelog entry; the content below is from the release commit.)*

### Added

- **Proactive smart-consumer wiki onboarding** — onboarding is offered
  at connect.
- **Bulk-copy bootstrap** — bulk-copy moves bytes without going through
  the LLM.
- **Bounded log pages** — append-only log pages get a rotate-by-period
  discipline.

## 1.0.0 — 2026-07-06

First public release.

### Changed

- **License: AGPL-3.0-or-later** (was `MIT OR Apache-2.0`), with a
  commercial dual-license available — see [LICENSING.md](LICENSING.md).
  SPDX headers on all first-party sources; contributions now require a
  DCO sign-off plus a relicensing grant ([CONTRIBUTING.md](CONTRIBUTING.md)).
- **Public repository with a fresh history.** The engineering wiki stays
  in the maintainer's private archive; the user/integrator documentation
  ships in [`docs/`](docs/).
- The MCP tool families are declared **stable under semver** from this
  release.

### Added

- **Bridge distribution from the running server** (roadmap 3i). A running
  mwe-mcp is now the distribution point for its own bridges — no repo
  clone, no manual symlinks. A public, anonymous root surface serves a
  slim **front page** (`GET /`: an agent line → the catalog, a human
  sign-in link), the **bridge catalog** (`GET /bridges`,
  `GET /bridges/<consumer>` — each entry with an *agent instructions*
  link to its `install.md`; the install command tailored to the request
  `Host`), and a **self-contained installer**
  (`GET /bridges/<consumer>/install.{sh,ps1,md}`): the bridge's plugin
  files are embedded in the binary and inlined into the script, so one
  `curl … | sh` from inside the hermes checkout lays everything down. The
  **same** catalog + guide are also a dashboard **Bridges** tab
  (`/dashboard/bridges`), and the dashboard home gained a *Connect a
  consumer* card (MCP URL + issue-token + wire-a-consumer). The **token**
  is issued from that card — never from the bridge pages or the
  installer, which only instruct the operator to set it, disable the
  host's built-in memory, and restart. The standalone admin-only
  `/connect` page was **retired** (its onboarding role moved to the home
  + Bridges tab; the `/connect/hooks/*` bundle endpoints remain).

- **The media pipeline** (roadmap group 12). Photos, video, audio and
  documents become memory without betraying the pillars: a media item
  enters as an ordinary described fact whose body carries a bare
  `{{embed=<catalog_id>}}` key, while everything behind the key — kind,
  MIME, size and the **per-media ACL** — is authoritative in the new
  `media_catalog` table (migration 0039), the twin of `fact_index`;
  bytes live once in a global content-addressed store under
  `<workdir>/media/` (sha256, blob-before-row write order). Entry is
  two-phase: `POST /media` (multipart, the same bearer JWT +
  `X-MWE-Act-As` as `/mcp`, idempotent per-owner dedup) mints the
  `c-YYYY-MM-DD-<kind>-NNN.<ext>` id with the closed English kind
  vocabulary, then the new optional `attachments` array on
  `wiki_ingest_message` links it to the turn. Undescribed photos ride
  the existing ingest LLM call as inline image parts (Gemini
  `inlineData`, Anthropic `image` blocks, Ollama `images`; prompt
  v2.27); a consumer-supplied `description` is trusted instead. The
  classifier claims attachments per extraction; markers are rendered by
  code, claimed media widen their catalog ACL to the fact's read set
  (monotone union), and a deterministic fallback files whatever no plan
  claimed — catalogued media is never dead memory. Exit:
  `GET /media/<catalog_id>` (per-media ACL, strong sha256 ETag,
  inline-safe MIME policy) plus the dashboard's cookie-authenticated
  alias with inline `<img>`/`<video>`/`<audio>` rendering of embeds;
  the export archive bundles referenced blobs under `_media/` with a
  catalog manifest; `wiki_lint` ships the `embed_missing` check. The
  marker grammar legalizes embeds inside region bodies (collected on
  the Region event, no more `NestedRegion` warning) so media travel
  with their facts through page reorganizations. The hermes bridge
  grows a standalone `mwe-media` gateway-hook plugin (opt-in):
  Telegram media → fail-closed sender gate → upload → spool →
  `attachments` on the turn's ingest, closing the host's
  native-image-mode memory bypass.

- **The agent-bridge home (`agents-bridges/`) and the hermes bridge.** Host
  adapters are now in-repo deliverables: an authoring guide, a per-bridge
  `bridge.toml` compat manifest (pinned upstream + per-turn-contract
  version, schema-checked), a two-tier smoke harness (offline against a
  recording stub endpoint; live against a real server), and a separate
  non-blocking CI workflow with a weekly upstream-HEAD canary. The
  per-turn contract in `INTEGRATING.md` is stamped **v1**. The first
  bridge ships with it: a zero-fork **hermes-agent plugin pair**
  (`mwe` memory provider — one mechanical ingest per turn, consumer-owned
  window, per-sender act-as pool, one-way mirror of the built-in memory;
  `mwe-truncate` context engine — bounded window, no summarization pass),
  validated live end-to-end.
- **Per-user (addressed) structure proposals** (migration 0032). A
  `recipient_id` column records who a proposal concerns; REM derives it
  from the triggering fact and carries it on the `StructureProposed` /
  `DedupProposed` event payloads. The dashboard tray,
  `structure_proposal_list`, and `pending_attention` scope to "addressed
  to me or unaddressed" for a non-admin (admins see all); apply / confirm
  / revert are gated to the addressee or an admin.
- **Single-use dashboard magic-link.** `GET /dashboard/auth/link` redeems
  a `dashboard_link` token exactly once (compare-and-set on the `jti` in
  `token_blacklist`), sets the sliding session cookie, and redirects to
  the deep-link. `dashboard_link` URLs now target this endpoint and are
  no longer replayable. Together these wire the per-user proposal
  notification flow: REM event → consumer agent → Telegram → single-use
  dashboard link.

### Changed

- **`mwe-mcp serve` provisions the dedicated-user service for you.** The
  dedicated-user gate (roadmap 14b) used to refuse to boot under a login
  account or root and only *print* the `useradd`/`chown`/`chmod` steps.
  Now, on an interactive terminal, it **offers to set the whole thing
  up**: on confirmation it creates the `mwe-mcp` account, installs the
  binary to `/usr/local/bin/mwe-mcp`, relocates (preserving data) or
  creates and locks the workdir at `/home/mwe-mcp/workdir`, installs the
  `mwe-mcp.service` unit (`User=mwe-mcp`, `Restart=on-failure`,
  `ProtectSystem=strict`, boot-enabled), and `enable --now`s it — then
  hands the port to the service and exits. Each privileged step is shown
  and runs under `sudo`. Declining, or a non-interactive host (systemd,
  CI, container, piped stdin), keeps the printed manual steps.
  And on an interactive **`--bypassdedicateduser`** run under a login
  account — the shape for a box dedicated to mwe-mcp (no co-located
  consumer to wall off) — it likewise offers a restart-on-boot service,
  this one `User=<your login user>` with the bypass baked into `ExecStart`
  and no workdir relocation. Both generated units pin `XDG_CACHE_HOME`
  inside the workdir so the bge-m3 weights (~2.2 GB) download succeeds
  under `ProtectSystem=strict`. Net effect: `mwe-mcp serve` takes a fresh
  operator from a login-account refusal to a running, boot-enabled,
  auto-restarting service in one prompt — co-located *or* standalone.
- **`serve` asks where to listen.** `--bind` / `--port` are now optional;
  on a bare interactive `serve` it asks whether to expose the server to
  other machines (`0.0.0.0`, LAN / port-forwardable) or keep it local
  (`127.0.0.1`, the default) and on which port — mwe-mcp is a server
  multiple consumers reach over HTTP, often from other hosts. The choice
  bakes into the systemd unit when the gate provisions one. Passing either
  flag, or a non-interactive host, skips the prompt and uses the loopback
  defaults. When you do expose it, the endpoint is JWT-gated but plain
  HTTP — put TLS in front (reverse proxy / tunnel) and mint `exposed`
  tokens.

### Removed / Breaking

- **The `wiki_type` registry tools and the runtime type-forge were removed.**
  Concretely: the three MCP tools `wiki_type_register` / `wiki_type_list` /
  `wiki_type_describe` (the tool roster drops 23 → 20); the dashboard **Types** page
  and the chat **forge / schema-evolve** verbs; and the emergent *vertical-genre*
  layer. The bundled templates, the registry table, and the structured routing remain;
  the core `_internal.wiki_type_*` functions still exist server-side.

## [0.2.0] — 2026-05-30

First real release. It back-fills the whole feature set built across
Phase B (memory engine + MVP dashboard) and Phase C (REM, structure
proposals, smart wikis) — the surface that turns the Phase A
scaffold into a working product. This is a **documentation-consolidation
milestone**, not a frozen-API 1.0: the public release with stability
guarantees is the Phase E target. The repo is now at Phase D
(first-consumer cutover). For what each capability *is and does*, the
documentation set (now `docs/`) is the single source
of truth; the pointers below link the relevant page.

### Added

- **Filesystem-SSOT memory model.** Memory lives as Obsidian-native
  markdown on disk; the `engine.db` sqlite index is fully
  reconstructible by re-walking the filesystem, so deleting it is a
  recoverable operation rather than data loss
  ([`docs/concepts/memory-model.md`](docs/concepts/memory-model.md)).
- **`wiki_type` registry** with bundled templates and an on-demand
  forge that invents a new template (frontmatter schema + lifecycle
  rules) at apply time
  ([`docs/concepts/memory-model.md`](docs/concepts/memory-model.md)).
- **Block-level ACL** via inline `{{owner=… allow=… sender=…}}…{{/}}`
  markers, with per-sender redaction applied region-by-region at render
  time ([`docs/concepts/identity-and-acl.md`](docs/concepts/identity-and-acl.md)).
- **Multi-user identity.** Users and groups with a single-admin model,
  managed through the dashboard CRUD; one unified JWT shape shared by
  the MCP and dashboard surfaces
  ([`docs/concepts/identity-and-acl.md`](docs/concepts/identity-and-acl.md)).
- **Write-side flow:** `wiki_capture` / `wiki_supersede` / `wiki_forget`
  / `wiki_link`, with jaccard 6-gram dedup against active facts on
  capture ([`docs/protocol/tool-reference.md`](docs/protocol/tool-reference.md)).
- **Hybrid recall:** lexical + semantic (embedding cosine) + wikilink
  multi-hop traversal, ACL-filtered
  ([`docs/protocol/tool-reference.md`](docs/protocol/tool-reference.md)).
- **`wiki_ingest_message` LLM router:** a single LLM call classifies a
  consumer message into capture / supersede / recall / structural-hint /
  skip and routes it to the write-side flow
  ([`docs/protocol/tool-reference.md`](docs/protocol/tool-reference.md)).
- **REM self-reorganization.** A nightly cycle runs lifecycle rules,
  settles overdue structure proposals, and emits dedup / promotion /
  type-forge / archive proposals plus hub regeneration
  ([`docs/architecture/overview.md`](docs/architecture/overview.md)).
- **MCP tool surface over HTTP** — families A–K (identity, capture,
  recall, ingest, structure proposals, audit, smart-wiki admin,
  skills, smart-consumer bootstrap, …). The exact roster lives in
  [`docs/protocol/mcp-tools.md`](docs/protocol/mcp-tools.md); the
  proposal-write actions (apply / confirm / revert) are dashboard-only,
  not on the MCP surface.
- **Smart-wikis + smart-consumer surface:** `wiki_admin_*`
  authoritative writes, the `_briefing.md` channel, cooperative leases,
  an append-only op-log with revert, the `/cite` resolver, and inline
  dashboard comments
  ([`docs/protocol/mcp-tools.md`](docs/protocol/mcp-tools.md)).
- **Built-in dashboard:** identity console, memory MVP (wiki / fact
  browser), agentic chat panel, admin LLM-config editor, and the
  operational-prompt editor
  ([`docs/architecture/overview.md`](docs/architecture/overview.md)).
- **Configurable internal LLM** with all-local / hybrid / all-api
  profiles across Ollama, Anthropic, and Gemini backends, wired per
  function and per backend through config + the dashboard editor
  ([`docs/architecture/runtime-topology.md`](docs/architecture/runtime-topology.md),
  [`docs/protocol/config-schema.md`](docs/protocol/config-schema.md)).

### Changed

- **Documentation consolidated.** The engineering documentation is now
  the single source of truth for what the system is and does; the
  planning corpus is forward-only (roadmap + open questions).
- Rust toolchain pinned to **1.88** (was 1.85).

### Removed / Breaking

- **stdio MCP transport removed** — the server is HTTP-only now
  ([`docs/architecture/runtime-topology.md`](docs/architecture/runtime-topology.md)).
- **Legacy `enrollment.yaml` loader removed** — identity is created and
  managed through the dashboard first-run wizard + CRUD, not a seed file
  ([`docs/concepts/identity-and-acl.md`](docs/concepts/identity-and-acl.md)).
- Internally, the `wiki_type` "family" column was refactored to a
  `companion: bool` marker (the live registry reads `companion` via
  `is_companion()`; the old `family TEXT` column is retired).

## [0.0.1] — 2026-05-17

### Added
- Initial scaffold for Phase A.
- Cargo workspace with three crates:
  - `mwe-core` — headless memory engine (module skeleton).
  - `mwe-mcp-server` — CLI binary `mwe-mcp` with `init`, `serve`, `token-issue`,
    `token-revoke`, `token-list`, `doctor` subcommands (stubs).
  - `mwe-dashboard` — built-in PWA library (router stub).
- Pinned core dependencies: `rmcp` 1.7, `axum` 0.7, `maud` 0.26, `sqlx` 0.8,
  `tokio` 1, `jsonwebtoken` 9, `uuid` 1 (v7), `notify` 6, `clap` 4,
  `reqwest` 0.12 (rustls), `fs2` 0.4, `rust-embed` 8.
- Rust toolchain pinned to 1.85 (edition 2024).
- `rustfmt.toml`, `clippy.toml`, `.cargo/config.toml`, `deny.toml`.
- Dual license `MIT OR Apache-2.0`.
- Full planning corpus copied into `docs/design/` (read-only reference).
- Placeholder `AGENT_INSTRUCTIONS.md` with cardinal rule + decision tree.
