-- 0078_fact_slot — the identity-card SLOT a fact fills, in the fact's own words.
--
-- An identity card holds a handful of always-on facts, and each of them fills a
-- SLOT that holds one value: a person has one date of birth, one address they
-- live at, one mobile number you call. The classifier already names that slot
-- when it declares a conflict — "the date of birth", "the mobile number" — and
-- until now the name was read once, spent on the question put to the speaker,
-- and thrown away.
--
-- WHY IT HAS TO BE KEPT. The question can only be asked when the speaker may
-- READ the value already on the card, because asking quotes it back to them. A
-- speaker who may not read it is shown nothing of that slot at all, so the
-- classifier cannot name a conflict and the second value files quietly beside
-- the first: a card holding two mobile numbers, with neither the speaker nor
-- the person told. Deciding that case is the engine's, and to decide it the
-- engine must be able to ask "does this new value fill a slot this card already
-- fills?" without a model — which needs the stored fact to still know its slot.
--
-- The slot is the fact's own, not the card's: two facts on one card fill
-- different slots, and the same slot on two people's cards is two slots. So it
-- is a column on the fact and not a table of its own.
--
-- WHAT IT IS NOT. It is not a key, not a uniqueness constraint and not a
-- vocabulary: the words are whatever the classifier wrote, in the memory's own
-- language, and two spellings of one slot are two slots to a string
-- comparison. That is why it decides only whether to ASK the card's owner, and
-- never whether to overwrite anything.
--
-- ADDITIVE ONLY. Existing rows get NULL, and a fact with no slot recorded takes
-- part in no slot comparison — it behaves exactly as it does now. Filling them
-- in after the fact is a separate job and is not attempted here.

ALTER TABLE fact_index     ADD COLUMN slot TEXT;
ALTER TABLE capture_buffer ADD COLUMN slot TEXT;

-- The hot read is "which facts on this card carry a slot", asked per turn over
-- the handful of rows one identity page holds, so the partial index is there to
-- keep the NULL majority out of it rather than to serve a large scan.
CREATE INDEX idx_fact_slot ON fact_index(slot) WHERE slot IS NOT NULL;
