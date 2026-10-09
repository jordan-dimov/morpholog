-- A refusal names the two values its failing comparison compared, and
-- the log keeps them beside the witness.
ALTER TABLE morpholog.rejections
    ADD COLUMN compared jsonb
        CHECK (compared IS NULL OR jsonb_typeof(compared) = 'object');
