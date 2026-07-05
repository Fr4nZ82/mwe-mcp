-- 0032_structure_proposals_recipient — per-user (addressed) structure proposals
--
-- Supersedes the deferred "emitted_by" column referenced in
-- proposals.rs (RevertError::RevertNotAuthorized, count_in_flight) and
-- the dashboard pending-confirms note. Adds the recipient (addressee) of
-- a structure proposal: the human the consumer agent should notify and
-- who (together with an admin) may apply / confirm / revert it.
--
-- Wire form is a Principal string ("user:<id>" | "group:<id>" | "global"),
-- matching owner_id / sender_id on fact_index. NULL = deployment-wide /
-- admin-fallback, which is also the value every pre-0032 row reads as
-- (full back-compat — the dashboard and pending_attention keep showing
-- those rows to admins exactly as before).
--
-- Derivation rule (proposals::recipient_from_fact, applied by the
-- per-kind emitters via EmitParams.recipient): the fact's sender_id when
-- present, else the owning user, else NULL (group/global owner with no
-- sender → admin-fallback). REM always emits; "emitted_by" was always the
-- system, so the meaningful column is the addressee, hence recipient_id.

ALTER TABLE structure_proposals ADD COLUMN recipient_id TEXT;  -- Principal string | NULL (deployment-wide / admin fallback)

-- Backs the per-recipient pending / pending-confirm tray + the
-- pending_attention count: "rows addressed to me OR unaddressed", by status.
CREATE INDEX idx_struct_recipient ON structure_proposals(recipient_id, status);
