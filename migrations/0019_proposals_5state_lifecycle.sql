-- 0019_proposals_5state_lifecycle — structure_proposal lifecycle a 5 stati
-- (vedi [engine DB & migrations](../wiki/design-notes/engine-db-and-migrations.md))
--
-- Estende lo schema di `structure_proposals` per il nuovo lifecycle a 5 stati:
--   pending → applied_pending_confirm → applied | reverted | expired
--
-- L'apply esplicito-utente dal cruscotto continua a portare a `applied`
-- direttamente (`apply_mode = 'manual'`). L'auto-apply via sweep su
-- `timeout_at` produce invece lo stato intermedio `applied_pending_confirm`
-- (`apply_mode = 'auto'`); l'utente ha tempo `confirm_deadline` (default
-- applied_at + 7gg) per confermare o annullare. Silenzio entro la finestra
-- → la sweep `auto_revert_unconfirmed_proposals` flippa a `reverted` con
-- `revert_triggered_by = 'sweep'`.
--
-- Backward compatibility:
--   * L'enum `status` resta `TEXT`, le righe storiche con valore `applied`
--     restano valide; le righe nuove possono assumere anche
--     `applied_pending_confirm` come valore.
--   * Tutte le nuove colonne ammettono `NULL`. Per le righe legacy
--     precedenti a questo schema il campo `apply_mode` resta NULL (interpretato a runtime
--     come "manual" se la riga è `applied`, indeterminato altrimenti);
--     `revert_triggered_by` resta NULL per le righe `reverted` legacy
--     (interpretato come "user").
--   * `confirm_deadline` valorizzato solo per le righe `applied_pending_confirm`
--     create dalla sweep auto-apply da questo schema in poi.

ALTER TABLE structure_proposals ADD COLUMN apply_mode TEXT;            -- 'manual' | 'auto' | NULL (legacy)
ALTER TABLE structure_proposals ADD COLUMN confirm_deadline TEXT;      -- applied_at + confirm_window; valorizzato sse status = 'applied_pending_confirm'
ALTER TABLE structure_proposals ADD COLUMN confirmed_at TEXT;          -- istante della transizione applied_pending_confirm → applied
ALTER TABLE structure_proposals ADD COLUMN confirmed_by TEXT;          -- sender_id del confermante post-auto-apply
ALTER TABLE structure_proposals ADD COLUMN revert_triggered_by TEXT;   -- 'user' | 'sweep' | NULL (legacy)

-- Indice dedicato per la sweep auto-revert: scan periodico delle proposte
-- in `applied_pending_confirm` con `confirm_deadline < now`.
CREATE INDEX idx_struct_confirm_deadline
    ON structure_proposals(status, confirm_deadline)
    WHERE status = 'applied_pending_confirm';
