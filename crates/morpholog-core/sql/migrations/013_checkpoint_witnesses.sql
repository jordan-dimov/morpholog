-- External witnesses on checkpoints: the exact response a timestamp
-- authority returned for a tree head, stored beside the head's own
-- signatures. `[]` for every existing checkpoint, which stays valid - a
-- witness adds "existed no later than T", it never subtracts from the
-- tree's verdict. Nothing derived is stored; the verifier reads the
-- attested time and the verdict from the proof itself. See schema.sql.
--
-- A column of this name in another shape is refused rather than
-- accepted as already done.

DO $$
DECLARE
    shape text := (
        SELECT format_type(a.atttypid, a.atttypmod)
               || CASE WHEN a.attnotnull THEN ' not null' ELSE ' null' END
        FROM pg_attribute a
        WHERE a.attrelid = 'morpholog.audit_checkpoints'::regclass
          AND a.attname = 'witnesses'
          AND NOT a.attisdropped
    );
BEGIN
    IF shape IS NOT NULL AND shape <> 'jsonb not null' THEN
        RAISE EXCEPTION 'morpholog.audit_checkpoints.witnesses exists with another shape (%); '
            'refusing to declare it current', shape;
    END IF;
END $$;

ALTER TABLE morpholog.audit_checkpoints
    ADD COLUMN IF NOT EXISTS witnesses jsonb NOT NULL DEFAULT '[]'::jsonb
        CHECK (jsonb_typeof(witnesses) = 'array');
