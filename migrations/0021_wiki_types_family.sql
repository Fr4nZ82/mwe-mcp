-- Famiglia `companion` per i wiki_type.
--
-- Aggiunge il campo `family TEXT` nullable a `wiki_types_registry` per
-- supportare il marker comportamentale documentato nel
-- [memory model](../wiki/concepts/memory-model.md).
--
-- Semantica: `family = 'companion'` segna i wiki_type "amministrati
-- autorevolmente da un consumer smart dell'utente" (bundled
-- `wiki-project-companion` + custom user-registered `wiki-companion-<slug>`
-- via il futuro `wiki_type_register`). I controlli di
-- read/write split del REM, il vincolo `400
-- wiki_type_not_writable_via_ingest` su `wiki_ingest_message`, e il gate
-- `consumer_class=smart` su `wiki_admin_*` filtreranno tutti su questo
-- campo.
--
-- Backward compatibility: i bundled più vecchi non hanno `family` nel
-- frontmatter ⇒ resta `NULL` (la maggior parte dei tipi) e niente
-- comportamenti companion. Solo `wiki-project-companion` (bundled aggiunto
-- in questo round) e i futuri custom user `wiki-companion-<slug>`
-- popoleranno la colonna a `'companion'`.

ALTER TABLE wiki_types_registry ADD COLUMN family TEXT;

CREATE INDEX IF NOT EXISTS idx_wiki_types_family
    ON wiki_types_registry(family);
