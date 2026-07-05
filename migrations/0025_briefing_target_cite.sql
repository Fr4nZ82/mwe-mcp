-- Citation IDs: un briefing item può puntare a una
-- sezione specifica della wiki via handle stabile.
--
-- Wire format (validato server-side):
--     wiki://<wiki_id>/<page_path>(#<heading-slug>)?
--
-- Esempi:
--     wiki://w_lnprint_xy/modules/auth.md#mfa-flow
--     wiki://w_lnprint_xy/decisions/2026-05-24-recovery-codes.md
--     wiki://w_lnprint_xy/_briefing.md#openclaw-202605241830
--
-- La colonna è NULL per default: la maggior parte delle notify (REM
-- briefing dispatcher, openclaw forwarding di una observation
-- generica) non ha un target specifico. Quando presente, il render
-- di `_briefing.md` include il link inline così lo smart consumer al
-- prossimo bootstrap può aprire la sezione esatta della wiki citata.
--
-- Persistenza degli anchors (`wiki_anchors` table + scan markdown al
-- `wiki_admin_push`) è rinviata (la dashboard ne sarà il
-- primo consumatore via endpoint `GET /wikis/<id>/cite/<path>#<anchor>`).
-- Per ora il `target_cite` è una stringa opaca lato server; lo
-- smart consumer decide come risolverla.

ALTER TABLE wiki_briefing_items
    ADD COLUMN target_cite TEXT;
