-- Group 17 provenance tube, downstream half: a turn's project-wiki
-- authorship breadcrumbs (plain `[[wiki_id/page]]` wikilinks the smart
-- consumer carried in via wiki_ingest_message's metadata.authored_refs)
-- ride the capture all the way into the fact index, so consolidation can
-- record a reference to the project page instead of re-storing its body
-- ("link, don't duplicate"). Same JSON-array-of-strings shape as `topics`.
-- NOT NULL DEFAULT '[]' so existing rows backfill cleanly and the reader
-- never has to special-case a missing column.
ALTER TABLE fact_index ADD COLUMN authored_refs TEXT NOT NULL DEFAULT '[]';
ALTER TABLE capture_buffer ADD COLUMN authored_refs TEXT NOT NULL DEFAULT '[]';
