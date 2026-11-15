-- Who may assert every actor on this deployment. Open, a proposal may
-- name any actor nobody restricted; closed, every actor needs a grant,
-- and the root actor is asserted by its root login alone, the trust
-- anchor the first grants are admitted under. The login is held by OID
-- as well as name, so a role dropped and made again is not trusted.
-- Exactly one row, written by the owner; a deployment's roles may only
-- read it, and a proposal on a deployment without it is refused. A
-- deployment upgraded to this migration stays open, as it was;
-- `init --root-actor` closes it.
CREATE TABLE morpholog.deployment_identity (
    singleton       boolean      PRIMARY KEY DEFAULT true CHECK (singleton),
    closed          boolean      NOT NULL DEFAULT false,
    root_actor      text,
    root_login      text,
    root_login_oid  oid,
    closed_at       timestamptz,
    closed_by       text,
    CHECK (num_nonnulls(root_actor, root_login, root_login_oid, closed_at, closed_by)
           = CASE WHEN closed THEN 5 ELSE 0 END)
);
INSERT INTO morpholog.deployment_identity DEFAULT VALUES;
