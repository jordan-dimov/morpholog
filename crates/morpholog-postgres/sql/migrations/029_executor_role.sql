-- The executor: a role that may move an outbox row through delivery and
-- nothing else. The record names it beside the writer and the reader; a
-- deployment recorded before it gets one when `init --least-privilege`
-- runs again.
ALTER TABLE morpholog.deployment_roles ADD COLUMN executor_role text;
