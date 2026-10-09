-- A refused gate carries the bindings it failed under, as an invariant
-- does, and the log keeps them.
ALTER TABLE morpholog.rejections
    DROP CONSTRAINT rejections_witness_is_invariant_only;
