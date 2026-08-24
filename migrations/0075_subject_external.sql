-- 0075_subject_external — the name of the thing a fact is about, when that
-- thing is not a principal.
--
-- A fact already carries three axes: `subject_id` (which principal answers for
-- it), `sender_id` (who said it) and `allow_ids` (who else may read it). All
-- three are principals, because all three grant something — read, amendment,
-- an address to file under.
--
-- What none of them can hold is the **name of what the fact is actually
-- about** when that is not a user and not a group: a relative who does not use
-- the product, the dog, a tree in the garden, a car, a house. Founder,
-- 2026-08-24: *«può essere di tutto, anche un albero, o un oggetto… se ha un
-- nome, diventa una entità esterna che può avere la sua pagina e la sua wiki se
-- continua ad ingrandirsi»*.
--
-- WHY THIS IS NOT A FOURTH PRINCIPAL. Making the subject itself free was the
-- other candidate and it loses information: a fact would then say what it is
-- about and stop saying who answers for it. Both are wanted, and they are
-- genuinely different — a clinical fact about a father is ABOUT him and
-- ANSWERED FOR by the household. So the column sits beside `subject_id`, and
-- `subject_id` keeps every job it has: read, amendment, the wiki a new page is
-- born in, the identity card. Nothing that reads it changes.
--
-- THE THREE THINGS IT BUYS, in the order they are built:
--
--   1. **One answer instead of a coin toss.** Choosing the responsible
--      principal for a non-principal subject is a judgement the classifier
--      makes per fact, per turn, from the group's scope prose. Made
--      independently across weeks it is not stable: in the corpus this column
--      was designed from, 121 facts about one non-enrolled person split 68/51
--      between two answers — and the SAME sentence, captured twice, got both.
--      With the name in a column the classifier is shown what was decided for
--      that name before, so the second occurrence is a lookup, not a judgement.
--
--   2. **Findable by name a week later.** Recall over standard wikis has no
--      lexical index at all (`wiki_sections` is built for smart wikis only), so
--      "Lady is very playful" reaches "we have a dog called Lady" only if the
--      two vectors happen to land near each other. A closed list of names can
--      be matched against the turn's text literally, which similarity cannot
--      promise.
--
--   3. **A page, and later a wiki, that is about something.** Fifty facts
--      sharing a name are a page because they are about one dog — not because
--      fifty sentences resemble each other. An entity that outgrows its page is
--      what a wiki can emerge from.
--
-- IT ALSO CLOSES ONE HOLE IMMEDIATELY. A fact carrying a name is not about the
-- person whose card it would otherwise land on, so it is refused there. That is
-- the mechanical form of a rule the Cartografo prompt already states in prose
-- ("NEVER assign a fact to a person page that is not in its identity_pages
-- tag") and which nothing enforced.
--
-- SHAPE. Free text, the display spelling as written ("Bilbo Baggins", not a
-- slug): it is read by a model and shown to a person. Matching is exact and
-- case/accent-insensitive; there are deliberately **no aliases** in this
-- version — "Bilbo Baggins", "Bilbo" and "uncle" are three names. The roster
-- the classifier is shown carries the spellings already in use and asks it to
-- reuse one, exactly as `known_users` carries declared aliases rather than
-- inferring them.
--
-- NOT ON DISK, AND THAT IS SAFE. The runtime marker is `{{f=<uuid>}}` and
-- nothing else — the ACL columns are already DB-only and no write path puts
-- attributes in a marker (`capture::render_marker`). The export form
-- (`render_full_marker`) does carry the axes, and gains `external=` so an
-- export round-trips.
--
-- ADDITIVE ONLY. Existing rows get NULL and behave exactly as before: a fact
-- without a name is a fact about its subject, which is every fact written until
-- now.

ALTER TABLE fact_index     ADD COLUMN subject_external TEXT;
ALTER TABLE capture_buffer ADD COLUMN subject_external TEXT;

-- The hot read is "which facts are about this name", asked once per turn
-- against the turn's text, and "which names exist" when the roster is built.
CREATE INDEX idx_fact_subject_external
    ON fact_index(subject_external) WHERE subject_external IS NOT NULL;
CREATE INDEX idx_capture_subject_external
    ON capture_buffer(subject_external) WHERE subject_external IS NOT NULL;
