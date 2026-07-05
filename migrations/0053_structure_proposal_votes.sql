-- 0053_structure_proposal_votes — member votes on a governed group-wiki deletion
--
-- A page deletion in a group-owned memory wiki is born-applied as one revertible
-- `bundle` receipt (wiki/planning/30_page-deletion-governance.md), then put to the
-- OTHER members of the owning group: more than half of them voting NO within the
-- 7-day revert window reverts the whole bundle; silence is consent (it stays);
-- once all eligible members have voted the outcome resolves early. The eligible
-- voter set (the group's roster minus the deleter, who consented by acting) lives
-- in the bundle row's `context` JSON; this table records the cast votes so the
-- tally (crate::votes) can count NOs and detect an all-voted quorum.
--
-- One row per (proposal, voter): the PK makes a re-vote a primary-key conflict
-- (votes are final). `vote` is the closed set 'yes' | 'no' — explicit YES votes
-- are recorded too (not just NO) so the all-voted early-resolution can tell a
-- voted-yes member apart from a silent one. Both FKs cascade: dropping the bundle
-- row or removing the voter from enrollment clears their dangling votes.

CREATE TABLE structure_proposal_votes (
    proposal_id TEXT NOT NULL REFERENCES structure_proposals(proposal_id) ON DELETE CASCADE,
    voter_id    TEXT NOT NULL REFERENCES enrollment_users(user_id) ON DELETE CASCADE,
    vote        TEXT NOT NULL,                          -- 'yes' | 'no'
    voted_at    TEXT NOT NULL,
    PRIMARY KEY (proposal_id, voter_id)
);

-- The tally counts a proposal's votes by value; the in-recall pending-vote
-- reminder asks "is there a bundle this member still owes a vote on", i.e. a
-- per-voter scan.
CREATE INDEX idx_proposal_votes_voter ON structure_proposal_votes(voter_id);
