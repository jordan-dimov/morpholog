-- One canonical order for the audit log. From here on every audit row
-- takes its place from a trigger, and records the row operations its
-- commit actually performed. Legacy rows keep order 0 and their leaf
-- encoding: nothing already committed changes.
--
-- The columns, constraints, indexes, sequences and functions were
-- prepared in steps before this transaction, outside any long lock (the
-- files under `027/`). This transaction holds only EXCLUSIVE locks, so
-- plain reads carry on; the runner writes the activation record after
-- this script, in the same transaction, so the legacy root comes from
-- the Rust leaf encoder. A database `init` creates is ordered from its
-- first row and has none.

-- A writer that has already written makes activation wait, so its row
-- lands in the legacy prefix. One that starts later waits here and then
-- meets the trigger.
LOCK TABLE morpholog.claims, morpholog.audit IN EXCLUSIVE MODE;

-- A cutover that was interrupted drew order 1 and rolled back, but a
-- sequence does not roll back, so the next attempt would draw 2 and the
-- trigger would refuse the activation. Nothing ordered exists yet, so the
-- order starts again here; ALTER SEQUENCE is transactional.
ALTER SEQUENCE morpholog.audit_order_seq RESTART WITH 1;

CREATE OR REPLACE TRIGGER audit_order BEFORE INSERT ON morpholog.audit
    FOR EACH ROW EXECUTE FUNCTION morpholog.audit_order_assign();
