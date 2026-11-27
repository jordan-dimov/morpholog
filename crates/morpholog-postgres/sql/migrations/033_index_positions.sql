-- An index specification may cover several positions of one predicate,
-- in order, so the registry records each one's positions as a list: a
-- single position becomes a list of one. The digest and the name of a
-- one-position specification are unchanged, so no index is rebuilt.
CREATE FUNCTION morpholog.index_positions_valid(p integer[]) RETURNS boolean
    LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
    RETURN cardinality(p) > 0
        AND array_ndims(p) = 1
        AND array_position(p, NULL) IS NULL
        AND 0 <= ALL (p)
        AND cardinality(p) = (SELECT count(DISTINCT x) FROM unnest(p) AS x);

ALTER TABLE morpholog.managed_index
    ADD COLUMN positions integer[] CHECK (morpholog.index_positions_valid(positions));
UPDATE morpholog.managed_index SET positions = ARRAY[position];
ALTER TABLE morpholog.managed_index
    ALTER COLUMN positions SET NOT NULL,
    DROP COLUMN position;

ALTER TABLE morpholog.index_requirement
    ADD COLUMN positions integer[] CHECK (morpholog.index_positions_valid(positions));
UPDATE morpholog.index_requirement SET positions = ARRAY[position] WHERE position IS NOT NULL;
ALTER TABLE morpholog.index_requirement DROP COLUMN position;
