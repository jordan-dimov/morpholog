#!/usr/bin/env bash
# Spike step 2: kill-signal probe. At N=10k entries (30k claims), can a
# hand-tuned residual query with partial expression indexes avoid a
# relation-level SIRead lock and a seq scan? If not, the spike is likely
# no-go before any compiler is built.
set -euo pipefail
DB=morpholog_dev

psql -q -d "$DB" <<'SQL'
TRUNCATE morpholog.claims;
INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in)
SELECT 'JournalEntry',
       jsonb_build_array(
         jsonb_build_object('type','subject','value','e_'||i),
         jsonb_build_object('type','subject','value','d_1'),
         jsonb_build_object('type','subject','value','p_'||(i % 12))),
       '00000000-0000-0000-0000-000000000000'::uuid
FROM generate_series(1, 10000) i;
INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in)
SELECT 'JournalLine',
       jsonb_build_array(
         jsonb_build_object('type','subject','value','e_'||i),
         jsonb_build_object('type','subject','value','acc_d_'||(i % 50)),
         jsonb_build_object('type','decimal','value','100'),
         jsonb_build_object('type','decimal','value','0')),
       '00000000-0000-0000-0000-000000000000'::uuid
FROM generate_series(1, 10000) i;
INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in)
SELECT 'JournalLine',
       jsonb_build_array(
         jsonb_build_object('type','subject','value','e_'||i),
         jsonb_build_object('type','subject','value','acc_c_'||(i % 50)),
         jsonb_build_object('type','decimal','value','0'),
         jsonb_build_object('type','decimal','value','100')),
       '00000000-0000-0000-0000-000000000000'::uuid
FROM generate_series(1, 10000) i;
DROP INDEX IF EXISTS morpholog.spike_jl_a0;
DROP INDEX IF EXISTS morpholog.spike_je_a0;
CREATE INDEX spike_jl_a0 ON morpholog.claims ((arguments -> 0 ->> 'value'))
  WHERE predicate_name = 'JournalLine';
CREATE INDEX spike_je_a0 ON morpholog.claims ((arguments -> 0 ->> 'value'))
  WHERE predicate_name = 'JournalEntry';
ANALYZE morpholog.claims;
SQL

RESIDUAL="
SELECT (t0.arguments -> 0 ->> 'value')::text AS w_entry
FROM morpholog.claims t0
WHERE t0.predicate_name = 'JournalEntry'
  AND (t0.arguments -> 0 ->> 'value') IN ('e_42')
  AND NOT (
    COALESCE((SELECT sum((s0.arguments -> 2 ->> 'value')::numeric)
              FROM morpholog.claims s0
              WHERE s0.predicate_name = 'JournalLine'
                AND (s0.arguments -> 0 ->> 'value') = (t0.arguments -> 0 ->> 'value')), 0::numeric)
    =
    COALESCE((SELECT sum((s1.arguments -> 3 ->> 'value')::numeric)
              FROM morpholog.claims s1
              WHERE s1.predicate_name = 'JournalLine'
                AND (s1.arguments -> 0 ->> 'value') = (t0.arguments -> 0 ->> 'value')), 0::numeric)
  )
ORDER BY t0.predicate_name, t0.arguments
LIMIT 1"

echo "=== EXPLAIN (residual, with partial indexes) ==="
psql -d "$DB" -c "EXPLAIN (ANALYZE, BUFFERS) $RESIDUAL"

echo "=== SIRead locks held by the residual under SERIALIZABLE ==="
# Session A holds the tx open via pg_sleep; session B inspects pg_locks.
psql -q -d "$DB" -c "BEGIN ISOLATION LEVEL SERIALIZABLE; $RESIDUAL; SELECT pg_sleep(4);" &
A_PID=$!
sleep 1.5
psql -d "$DB" -c "
SELECT locktype, relation::regclass::text AS rel, count(*)
FROM pg_locks
WHERE mode = 'SIReadLock'
GROUP BY 1, 2
ORDER BY 1, 2;"
wait $A_PID

echo "=== control: same probe WITHOUT the indexes ==="
psql -q -d "$DB" -c "DROP INDEX morpholog.spike_jl_a0; DROP INDEX morpholog.spike_je_a0; ANALYZE morpholog.claims;"
echo "--- EXPLAIN (no indexes) ---"
psql -d "$DB" -c "EXPLAIN (ANALYZE, BUFFERS) $RESIDUAL" | head -25
psql -q -d "$DB" -c "BEGIN ISOLATION LEVEL SERIALIZABLE; $RESIDUAL; SELECT pg_sleep(4);" &
A_PID=$!
sleep 1.5
psql -d "$DB" -c "
SELECT locktype, relation::regclass::text AS rel, count(*)
FROM pg_locks
WHERE mode = 'SIReadLock'
GROUP BY 1, 2
ORDER BY 1, 2;"
wait $A_PID
