//! The shared-writer lint: two programmes deployed against one database
//! share rows for any same-named predicate, each ungoverned by the
//! other's gates. The finding is the shared WRITE - admit or retract -
//! never a read; and "guarded" claims only what the controls surface
//! claims, a top-level admission gate.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::{Lint, Program, shared_writer_lints};
use morpholog_surface::parse_program;

fn program(source: &str) -> Program {
    let p = parse_program(source).expect("parses");
    p.validate().expect("validates");
    p
}

const SECURE: &str = "
program secure
predicate AuditSigningKey(key_id: Subject, purpose: Subject, public_key: Subject)
predicate KeyAdmin(person: Subject)
transformation register(key_id, purpose, public_key):
    require KeyAdmin(actor)
    admit AuditSigningKey(key_id, purpose, public_key)
";

const ROGUE: &str = "
program rogue
predicate AuditSigningKey(key_id: Subject, purpose: Subject, public_key: Subject)
transformation bring_own_key(key_id, purpose, public_key):
    admit AuditSigningKey(key_id, purpose, public_key)
";

const READER: &str = "
program reader
predicate AuditSigningKey(key_id: Subject, purpose: Subject, public_key: Subject)
predicate Seen(key_id: Subject)
transformation note(key_id):
    require AuditSigningKey(key_id, _, _)
    admit Seen(key_id)
";

const REVOKER: &str = "
program revoker
predicate AuditSigningKey(key_id: Subject, purpose: Subject, public_key: Subject)
transformation purge(keys):
    for k in keys:
        require AuditSigningKey(k, _, _)
        retract AuditSigningKey(k, _, _)
";

/// One finding, flattened: (transformation, predicate, guarded, other
/// programme, [(other transformation, guarded)]).
type Flat<'a> = (&'a str, &'a str, bool, &'a str, Vec<(&'a str, bool)>);

fn shared(found: &[Lint]) -> Vec<Flat<'_>> {
    found
        .iter()
        .map(|l| match l {
            Lint::SharedWriter {
                transformation,
                predicate,
                guarded,
                other_program,
                other_writers,
            } => (
                transformation.as_str(),
                predicate.as_str(),
                *guarded,
                other_program.as_str(),
                other_writers
                    .iter()
                    .map(|p| (p.transformation.as_str(), p.guarded))
                    .collect(),
            ),
            other => panic!("only shared-writer findings expected, got {other:?}"),
        })
        .collect()
}

#[test]
fn a_guarded_writer_is_told_about_an_ungated_one() {
    let found = shared_writer_lints(&program(SECURE), &program(ROGUE));
    assert_eq!(
        shared(&found),
        vec![(
            "register",
            "AuditSigningKey",
            true,
            "rogue",
            vec![("bring_own_key", false)]
        )]
    );
    // And the other way round: the ungated side hears of the guarded one.
    let found = shared_writer_lints(&program(ROGUE), &program(SECURE));
    assert_eq!(
        shared(&found),
        vec![(
            "bring_own_key",
            "AuditSigningKey",
            false,
            "secure",
            vec![("register", true)]
        )]
    );
}

#[test]
fn a_reader_is_not_a_collision() {
    assert!(shared_writer_lints(&program(SECURE), &program(READER)).is_empty());
    assert!(shared_writer_lints(&program(READER), &program(SECURE)).is_empty());
}

#[test]
fn a_nested_retraction_is_a_write_and_a_nested_gate_is_not_admission() {
    let found = shared_writer_lints(&program(SECURE), &program(REVOKER));
    assert_eq!(
        shared(&found),
        vec![(
            "register",
            "AuditSigningKey",
            true,
            "revoker",
            vec![("purge", false)]
        )],
        "the retract inside the for is a write; the require inside it is not a gate"
    );
}

#[test]
fn both_guarded_still_fires_with_both_gates_reported() {
    let found = shared_writer_lints(&program(SECURE), &program(SECURE));
    assert_eq!(
        shared(&found),
        vec![(
            "register",
            "AuditSigningKey",
            true,
            "secure",
            vec![("register", true)]
        )]
    );
}
