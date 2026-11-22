-- A refused `or` of comparisons names each branch's two values, in
-- branch order, beside the single comparison a refusal may blame.
ALTER TABLE morpholog.rejections
    ADD COLUMN compared_branches jsonb
        CHECK (
            compared_branches IS NULL
            OR (jsonb_typeof(compared_branches) = 'array'
                AND jsonb_array_length(compared_branches) > 0
                AND NOT jsonb_path_exists(compared_branches, '$[*] ? (@.type() != "object")'))
        ),
    -- One comparison blamed, or every branch of an `or`, never both.
    ADD CONSTRAINT rejections_one_blame CHECK (compared IS NULL OR compared_branches IS NULL);
