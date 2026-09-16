-- External witnesses on checkpoints: the exact response a timestamp
-- authority returned for a tree head, stored beside the head's own
-- signatures. `[]` for every existing checkpoint, which stays valid - a
-- witness adds "existed no later than T", it never subtracts from the
-- tree's verdict. Nothing derived is stored; the verifier reads the
-- attested time and the verdict from the proof itself. See schema.sql.

ALTER TABLE morpholog.audit_checkpoints
    ADD COLUMN IF NOT EXISTS witnesses jsonb NOT NULL DEFAULT '[]'::jsonb
        CHECK (jsonb_typeof(witnesses) = 'array');
