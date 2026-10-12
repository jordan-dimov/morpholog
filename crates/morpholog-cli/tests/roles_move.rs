//! `roles move` gives a deployment a pair of roles of its own.
//!
//! The attacker modelled is a login of another deployment that holds the
//! shared pair: after the move it must hold nothing here through that
//! pair, while its own deployment keeps working. The operator is trusted:
//! the move refuses whatever it cannot carry over faithfully rather than
//! widen or silently narrow anyone's authority.
//!
//! Every database, role and login here belongs to the whole cluster and
//! is named per test, and each test drops them even when it fails.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::process::Command;
use std::time::Duration;

use sqlx::PgPool;

#[derive(Clone)]
struct World {
    tag: &'static str,
}

impl World {
    fn name(&self, what: &str) -> String {
        format!("mtest_mv_{}_{what}", self.tag)
    }
    fn db(&self, which: &str) -> String {
        self.name(&format!("db_{which}"))
    }
    fn url(&self, which: &str) -> String {
        let base = morpholog_postgres::with_default_user(&common::database_url());
        common::with_database(&base, &self.db(which))
    }
    fn roles(&self) -> Vec<String> {
        ["old_writer", "old_reader", "new_writer", "new_reader"]
            .into_iter()
            .chain(["l1", "l2", "l3", "l4", "l5", "stranger", "group"])
            .map(|r| self.name(r))
            .collect()
    }
    async fn admin(&self) -> PgPool {
        let base = morpholog_postgres::with_default_user(&common::database_url());
        PgPool::connect(&common::with_database(&base, "postgres"))
            .await
            .unwrap()
    }
    async fn pool(&self, which: &str) -> PgPool {
        PgPool::connect(&self.url(which)).await.unwrap()
    }

    async fn clean(&self) {
        let admin = self.admin().await;
        for which in ["a", "b"] {
            run(
                &admin,
                &format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", self.db(which)),
            )
            .await;
        }
        for role in self.roles() {
            run(&admin, &format!("DROP ROLE IF EXISTS {role}")).await;
        }
        admin.close().await;
    }

    /// Two databases sharing the old pair, as deployments provisioned
    /// before each had its own: `a` records and grants it through `init`,
    /// `b` records it and `migrate` grants it the same floor. Logins:
    /// `l1` and `l2` write, `l3` reads and writes, `l4` writes with
    /// inheritance off and `SET ROLE` on.
    async fn build(&self) {
        self.clean().await;
        let admin = self.admin().await;
        for which in ["a", "b"] {
            run(&admin, &format!("CREATE DATABASE {}", self.db(which))).await;
        }
        let (status, _, stderr) = cli(&[
            "init",
            "--least-privilege",
            "--role-prefix",
            &self.name("old_"),
            "--database-url",
            &self.url("a"),
        ]);
        assert!(status.success(), "{stderr}");
        let (status, _, stderr) = cli(&["init", "--database-url", &self.url("b")]);
        assert!(status.success(), "{stderr}");
        let b = self.pool("b").await;
        run(
            &b,
            &format!(
                "INSERT INTO morpholog.deployment_roles (writer_role, reader_role) \
                 VALUES ('{}', '{}')",
                self.name("old_writer"),
                self.name("old_reader")
            ),
        )
        .await;
        b.close().await;
        let (status, _, stderr) = cli(&["migrate", "--database-url", &self.url("b")]);
        assert!(status.success(), "{stderr}");
        let (w, r) = (self.name("old_writer"), self.name("old_reader"));
        for (login, grants) in [
            ("l1", vec![format!("GRANT {w} TO {{}}")]),
            ("l2", vec![format!("GRANT {w} TO {{}}")]),
            (
                "l3",
                vec![format!("GRANT {w} TO {{}}"), format!("GRANT {r} TO {{}}")],
            ),
            (
                "l4",
                vec![format!("GRANT {w} TO {{}} WITH INHERIT FALSE, SET TRUE")],
            ),
        ] {
            let name = self.name(login);
            run(&admin, &format!("CREATE ROLE {name} LOGIN")).await;
            for grant in grants {
                run(&admin, &grant.replace("{}", &name)).await;
            }
        }
        admin.close().await;
    }

    fn move_roles(&self, logins: &[&str]) -> (std::process::ExitStatus, String, String) {
        let prefix = self.name("new_");
        let url = self.url("a");
        let mut args = vec![
            "roles",
            "move",
            "--role-prefix",
            &prefix,
            "--database-url",
            &url,
        ];
        let names: Vec<String> = logins.iter().map(|l| self.name(l)).collect();
        for name in &names {
            args.extend(["--login", name]);
        }
        cli(&args)
    }

    /// Whether `login` may insert audit rows in database `which`, by
    /// whatever route PostgreSQL's privilege check follows.
    async fn writes(&self, which: &str, login: &str) -> bool {
        let pool = self.pool(which).await;
        let can = sqlx::query_scalar::<_, bool>(
            "SELECT has_table_privilege($1, 'morpholog.audit', 'INSERT')",
        )
        .bind(self.name(login))
        .fetch_one(&pool)
        .await
        .unwrap();
        pool.close().await;
        can
    }

    /// Grants, ownerships and policy references the old pair holds in
    /// database `a` or on it.
    async fn old_pair_held(&self) -> i64 {
        let pool = self.pool("a").await;
        let held = sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM pg_shdepend s JOIN pg_roles r ON r.oid = s.refobjid \
             WHERE s.refclassid = 'pg_authid'::regclass AND r.rolname = ANY($1) \
               AND (s.dbid = (SELECT oid FROM pg_database WHERE datname = current_database()) \
                    OR (s.dbid = 0 AND s.objid = (SELECT oid FROM pg_database \
                                                   WHERE datname = current_database())))",
        )
        .bind(vec![self.name("old_writer"), self.name("old_reader")])
        .fetch_one(&pool)
        .await
        .unwrap();
        pool.close().await;
        held
    }

    async fn recorded(&self) -> (String, String) {
        let pool = self.pool("a").await;
        let row: (String, String) =
            sqlx::query_as("SELECT writer_role, reader_role FROM morpholog.deployment_roles")
                .fetch_one(&pool)
                .await
                .unwrap();
        pool.close().await;
        row
    }

    async fn new_pair_exists(&self) -> bool {
        let admin = self.admin().await;
        let n =
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM pg_roles WHERE rolname = ANY($1)")
                .bind(vec![self.name("new_writer"), self.name("new_reader")])
                .fetch_one(&admin)
                .await
                .unwrap();
        admin.close().await;
        n > 0
    }

    /// Nothing changed: the old pair is still recorded and holds its
    /// floor, and the new pair was never created.
    async fn unchanged(&self) {
        assert_eq!(
            self.recorded().await,
            (self.name("old_writer"), self.name("old_reader"))
        );
        assert!(self.old_pair_held().await > 0, "the old floor is intact");
        assert!(!self.new_pair_exists().await, "no new role was left behind");
        assert!(self.writes("a", "l1").await);
    }
}

fn cli(args: &[&str]) -> (std::process::ExitStatus, String, String) {
    let output = Command::new(common::bin())
        .args(args)
        .output()
        .expect("spawn morpholog binary");
    (
        output.status,
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

async fn run(pool: &PgPool, sql: &str) {
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_string()))
        .execute(pool)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

/// Build the world, run `scenario`, and drop the world whatever happens.
/// `None` when the test role cannot create roles and databases.
async fn in_world<F, Fut>(tag: &'static str, scenario: F)
where
    F: FnOnce(World) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let world = World { tag };
    let admin = world.admin().await;
    let superuser: bool =
        sqlx::query_scalar("SELECT rolsuper FROM pg_roles WHERE rolname = current_user")
            .fetch_one(&admin)
            .await
            .unwrap();
    admin.close().await;
    if !superuser {
        eprintln!("skipping: needs a superuser test role");
        return;
    }
    world.build().await;
    let outcome = tokio::spawn(scenario(world.clone())).await;
    world.clean().await;
    if let Err(failure) = outcome {
        std::panic::resume_unwind(failure.into_panic());
    }
}

#[tokio::test]
async fn named_logins_move_and_the_other_database_keeps_the_old_pair() {
    in_world("happy", |w| async move {
        let (status, stdout, stderr) = w.move_roles(&["l1", "l3", "l4"]);
        assert!(status.success(), "{stderr}");
        let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(report["to"]["writer"], w.name("new_writer"), "{report}");
        assert_eq!(
            report["unmoved_members"],
            serde_json::json!([w.name("l2")]),
            "{report}"
        );
        assert_eq!(
            report["old_pair_reaches"],
            serde_json::json!([w.db("b")]),
            "{report}"
        );
        let l3 = report["logins"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["login"] == w.name("l3").as_str())
            .unwrap();
        assert_eq!(
            (l3["reader"].as_bool(), l3["writer"].as_bool()),
            (Some(true), Some(true))
        );

        assert!(
            w.writes("a", "l1").await,
            "a named login keeps writing here"
        );
        assert!(
            !w.writes("a", "l2").await,
            "an unnamed member loses the old pair's grants here"
        );
        assert!(
            w.writes("b", "l2").await,
            "and keeps them in the other deployment"
        );
        assert_eq!(w.old_pair_held().await, 0);
        assert_eq!(
            w.recorded().await,
            (w.name("new_writer"), w.name("new_reader"))
        );

        // l4 keeps its settings: no inheritance, SET ROLE allowed, no admin.
        let admin = w.admin().await;
        let options: (bool, bool, bool) = sqlx::query_as(
            "SELECT bool_or(am.inherit_option), bool_or(am.set_option), bool_or(am.admin_option) \
             FROM pg_auth_members am JOIN pg_roles g ON g.oid = am.roleid \
             JOIN pg_roles m ON m.oid = am.member WHERE g.rolname = $1 AND m.rolname = $2",
        )
        .bind(w.name("new_writer"))
        .bind(w.name("l4"))
        .fetch_one(&admin)
        .await
        .unwrap();
        admin.close().await;
        assert_eq!(options, (false, true, false));

        // migrate re-applies the floor to the new pair alone.
        let (status, _, stderr) = cli(&["migrate", "--database-url", &w.url("a")]);
        assert!(status.success(), "{stderr}");
        assert_eq!(
            w.old_pair_held().await,
            0,
            "migrate granted the old pair again"
        );

        // Asking again changes nothing.
        let (status, _, stderr) = w.move_roles(&["l1"]);
        assert!(!status.success());
        assert!(stderr.contains("already records"), "{stderr}");
    })
    .await;
}

#[tokio::test]
async fn what_cannot_be_carried_over_faithfully_is_refused_before_anything_changes() {
    in_world("refuse", |w| async move {
        let admin = w.admin().await;
        let refuse = |logins: &'static [&'static str], expect: &'static str| {
            let w = w.clone();
            async move {
                let (status, _, stderr) = w.move_roles(logins);
                assert!(!status.success(), "{logins:?} moved");
                assert!(stderr.contains(expect), "{logins:?}: {stderr}");
                w.unchanged().await;
            }
        };
        run(&admin, &format!("CREATE ROLE {} LOGIN", w.name("stranger"))).await;
        refuse(&["l1", "stranger"], "is not a direct member").await;

        run(&admin, &format!("CREATE ROLE {} NOLOGIN", w.name("group"))).await;
        run(
            &admin,
            &format!("GRANT {} TO {}", w.name("old_writer"), w.name("group")),
        )
        .await;
        refuse(&["group"], "cannot log in").await;

        run(&admin, &format!("CREATE ROLE {} LOGIN", w.name("l5"))).await;
        run(
            &admin,
            &format!(
                "GRANT {} TO {} WITH ADMIN TRUE",
                w.name("old_writer"),
                w.name("l5")
            ),
        )
        .await;
        refuse(&["l5"], "admin option").await;

        run(
            &admin,
            &format!("CREATE ROLE {} NOLOGIN", w.name("new_reader")),
        )
        .await;
        let (status, _, stderr) = w.move_roles(&["l1"]);
        assert!(!status.success());
        assert!(stderr.contains("already exists"), "{stderr}");
        run(&admin, &format!("DROP ROLE {}", w.name("new_reader"))).await;
        admin.close().await;

        let (status, _, stderr) = cli(&[
            "roles",
            "move",
            "--role-prefix",
            &w.name("new_"),
            "--database-url",
            &w.url("b"),
            "--login",
            &w.name("l1"),
        ]);
        // b records the old pair too, so this one moves; then a database
        // with nothing recorded refuses.
        assert!(status.success(), "{stderr}");
        let b = w.pool("b").await;
        run(&b, "DELETE FROM morpholog.deployment_roles").await;
        b.close().await;
        let (status, _, stderr) = cli(&[
            "roles",
            "move",
            "--role-prefix",
            &w.name("old_"),
            "--database-url",
            &w.url("b"),
        ]);
        assert!(!status.success());
        assert!(
            stderr.contains("records no least-privilege roles"),
            "{stderr}"
        );
    })
    .await;
}

#[tokio::test]
async fn privileges_beyond_the_floor_refuse_the_move_instead_of_being_revoked() {
    in_world("beyond", |w| async move {
        let a = w.pool("a").await;
        let (ow, or) = (w.name("old_writer"), w.name("old_reader"));
        for (grant, revoke, named) in [
            (
                format!("GRANT UPDATE ON morpholog.audit TO {ow}"),
                format!("REVOKE UPDATE ON morpholog.audit FROM {ow}"),
                "UPDATE on table morpholog.audit",
            ),
            (
                format!(
                    "CREATE FUNCTION public.mv_probe() RETURNS int LANGUAGE sql AS 'SELECT 1'; \
                     GRANT EXECUTE ON FUNCTION public.mv_probe() TO {or}"
                ),
                "DROP FUNCTION public.mv_probe()".to_string(),
                "function mv_probe()",
            ),
            (
                format!("ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT ON TABLES TO {or}"),
                format!(
                    "ALTER DEFAULT PRIVILEGES IN SCHEMA public REVOKE SELECT ON TABLES FROM {or}"
                ),
                "default privileges",
            ),
        ] {
            run(&a, &grant).await;
            let (status, _, stderr) = w.move_roles(&["l1"]);
            assert!(!status.success(), "moved past {grant}");
            assert!(stderr.contains(named), "{grant}: {stderr}");
            assert!(!w.new_pair_exists().await);
            assert_eq!(w.recorded().await, (ow.clone(), or.clone()));
            run(&a, &revoke).await;
        }
        a.close().await;
        let (status, _, stderr) = w.move_roles(&["l1"]);
        assert!(status.success(), "{stderr}");
    })
    .await;
}

#[tokio::test]
async fn a_failure_after_the_new_roles_exist_leaves_nothing_behind() {
    in_world("fail", |w| async move {
        let a = w.pool("a").await;
        run(
            &a,
            "CREATE FUNCTION public.mv_refuse() RETURNS trigger LANGUAGE plpgsql AS \
             $$BEGIN RAISE EXCEPTION 'forced failure while recording'; END$$; \
             CREATE TRIGGER mv_refuse BEFORE INSERT ON morpholog.deployment_roles \
             FOR EACH ROW EXECUTE FUNCTION public.mv_refuse()",
        )
        .await;
        let (status, _, stderr) = w.move_roles(&["l1"]);
        assert!(!status.success());
        assert!(stderr.contains("forced failure"), "{stderr}");
        run(&a, "DROP TRIGGER mv_refuse ON morpholog.deployment_roles; DROP FUNCTION public.mv_refuse()").await;
        a.close().await;
        w.unchanged().await;
        assert!(w.writes("a", "l2").await, "an unnamed member was not cut off");
    })
    .await;
}

// A `migrate` that read the old pair before the move must not grant it
// again after the move: the move waits for it, then revokes what it did.
#[tokio::test]
async fn a_migrate_reading_the_old_pair_cannot_grant_it_after_the_move() {
    in_world("race", |w| async move {
        let a = w.pool("a").await;
        let mut held = a.begin().await.unwrap();
        sqlx::query("SELECT writer_role FROM morpholog.deployment_roles")
            .fetch_one(&mut *held)
            .await
            .unwrap();

        let prefix = w.name("new_");
        let url = w.url("a");
        let login = w.name("l1");
        let child = std::process::Command::new(common::bin())
            .args([
                "roles",
                "move",
                "--role-prefix",
                &prefix,
                "--database-url",
                &url,
                "--login",
                &login,
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "GRANT SELECT, INSERT ON morpholog.audit TO {}",
            w.name("old_writer")
        )))
        .execute(&mut *held)
        .await
        .unwrap();
        held.commit().await.unwrap();
        let output = tokio::task::spawn_blocking(move || child.wait_with_output())
            .await
            .unwrap()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        a.close().await;
        assert_eq!(
            w.old_pair_held().await,
            0,
            "the old pair was granted after the move"
        );
    })
    .await;
}
