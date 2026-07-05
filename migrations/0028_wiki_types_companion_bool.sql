-- Refactor `family: TEXT` → `companion: BOOLEAN` su
-- `wiki_types_registry`.
--
-- Motivazione (see [engine DB & migrations](../wiki/design-notes/engine-db-and-migrations.md)): tutti i
-- branch attuali del marker sono binari (`is_companion_family()` →
-- `family.as_deref() == Some("companion")`), zero `match` multi-way,
-- unico valore mai usato `"companion"`. Il razionale storico di
-- future-proofing dichiarato in origine (2026-05-24) è speculativo:
-- nessuna seconda famiglia è disegnata o nominata nello spec corrente.
-- Refactor a `companion: bool` collassa parser + predicato + sito
-- d'uso senza perdere espressività; se mai serve una seconda famiglia,
-- si aggiunge campo dedicato con migration parallela.
--
-- Backward compatibility: il valore `family = 'companion'` esistente
-- viene tradotto a `companion = TRUE`; tutti gli altri (`NULL` oggi)
-- a `companion = FALSE` (default della nuova colonna).

-- 1. Aggiungi la nuova colonna boolean con default FALSE.
ALTER TABLE wiki_types_registry
    ADD COLUMN companion BOOLEAN NOT NULL DEFAULT FALSE;

-- 2. Mappa il vecchio valore stringa al nuovo bool.
UPDATE wiki_types_registry
    SET companion = TRUE
    WHERE family = 'companion';

-- 3. Drop dell'indice vecchio + creazione del nuovo.
DROP INDEX IF EXISTS idx_wiki_types_family;
CREATE INDEX IF NOT EXISTS idx_wiki_types_companion
    ON wiki_types_registry(companion);

-- 4. Drop della colonna obsoleta (SQLite >= 3.35).
ALTER TABLE wiki_types_registry
    DROP COLUMN family;
