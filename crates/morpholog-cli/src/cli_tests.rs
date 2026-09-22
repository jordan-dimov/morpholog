//! The binary's own tests: what the argument surface parses to, and
//! that a refused command returns to `main` rather than ending the
//! process. The command modules keep their own unit suites beside
//! their code; the CLI's behaviour is tested end to end in `tests/`.

use super::*;
use clap::error::ErrorKind;

/// Helper: parse the argv into our `Cli` and return the `database_url`
/// that landed on the resulting inspect-subcommand args.
fn parsed_url(argv: &[&str]) -> String {
    let cli = Cli::parse_from(argv);
    let Command::Inspect { what } = cli.command else {
        panic!("expected Command::Inspect, got {:?}", cli.command);
    };
    match what {
        Inspect::Claims(args) => args.db.database_url,
        Inspect::Audit(args) => args.db.database_url,
        Inspect::Rejections(args) => args.db.database_url,
        Inspect::Outbox(args) => args.db.database_url,
        Inspect::Coverage(args) => args.db.database_url,
        Inspect::Derived(_) => {
            panic!("use the dedicated inspect-derived parse tests, not parsed_url")
        }
        Inspect::Controls(_) => {
            panic!("inspect controls is static; it takes no database URL")
        }
        Inspect::Predicates(_) => {
            panic!("inspect predicates does not take a database URL")
        }
        Inspect::Guarantees(_) => {
            panic!("inspect guarantees does not take a database URL")
        }
    }
}

#[test]
fn inspect_claims_with_flag_url_parses() {
    let url = parsed_url(&[
        "morpholog",
        "inspect",
        "claims",
        "--database-url",
        "postgres:///morpholog_dev",
    ]);
    assert_eq!(url, "postgres:///morpholog_dev");
}

#[test]
fn inspect_audit_with_flag_url_parses() {
    let url = parsed_url(&[
        "morpholog",
        "inspect",
        "audit",
        "--database-url",
        "postgres://u:p@h/db",
    ]);
    assert_eq!(url, "postgres://u:p@h/db");
}

#[test]
fn inspect_outbox_with_flag_url_parses() {
    let url = parsed_url(&[
        "morpholog",
        "inspect",
        "outbox",
        "--database-url",
        "postgres:///morpholog_dev",
    ]);
    assert_eq!(url, "postgres:///morpholog_dev");
}

/// `inspect claims` without `--as-of` parses to `as_of = None`.
/// Pins that the optional flag is genuinely optional.
#[test]
fn inspect_claims_without_as_of_parses_to_none() {
    let cli = Cli::parse_from([
        "morpholog",
        "inspect",
        "claims",
        "--database-url",
        "postgres:///morpholog_dev",
    ]);
    let Command::Inspect {
        what: Inspect::Claims(args),
    } = cli.command
    else {
        panic!("expected Inspect::Claims, got {:?}", cli.command);
    };
    assert!(args.as_of.is_none(), "as_of must be None without the flag");
}

/// `inspect claims --as-of <uuid>` parses the UUID into the
/// optional field.
#[test]
fn inspect_claims_with_as_of_parses_uuid() {
    let tid = "0192e000-0000-7000-8000-000000000001";
    let cli = Cli::parse_from([
        "morpholog",
        "inspect",
        "claims",
        "--database-url",
        "postgres:///morpholog_dev",
        "--as-of",
        tid,
    ]);
    let Command::Inspect {
        what: Inspect::Claims(args),
    } = cli.command
    else {
        panic!("expected Inspect::Claims, got {:?}", cli.command);
    };
    assert_eq!(
        args.as_of,
        Some(AsOf::Transition(Uuid::parse_str(tid).unwrap())),
        "--as-of with a UUID must parse into the transition form"
    );
}

/// `--as-of` also accepts an RFC 3339 timestamp, parsed into the
/// at-or-before form (resolved against the audit log at run time).
#[test]
fn inspect_claims_with_as_of_timestamp_parses() {
    let cli = Cli::parse_from([
        "morpholog",
        "inspect",
        "claims",
        "--database-url",
        "postgres:///morpholog_dev",
        "--as-of",
        "2026-06-30T12:00:00Z",
    ]);
    let Command::Inspect {
        what: Inspect::Claims(args),
    } = cli.command
    else {
        panic!("expected Inspect::Claims, got {:?}", cli.command);
    };
    let Some(AsOf::AtOrBefore(at)) = args.as_of else {
        panic!("expected the at-or-before form, got {:?}", args.as_of);
    };
    assert_eq!(at.to_string(), "2026-06-30T12:00:00Z");
}

/// A bare date is rejected: the coordinate must be explicit about
/// the instant, not leave the time of day to a guess.
#[test]
fn inspect_claims_with_bare_date_as_of_errors_at_parse_time() {
    let err = Cli::try_parse_from([
        "morpholog",
        "inspect",
        "claims",
        "--database-url",
        "postgres:///morpholog_dev",
        "--as-of",
        "2026-06-30",
    ])
    .expect_err("bare date must surface a clap parse error");
    assert!(
        matches!(
            err.kind(),
            ErrorKind::ValueValidation | ErrorKind::InvalidValue
        ),
        "expected a value-validation error, got {:?}",
        err.kind()
    );
}

/// `--predicate` repeats into a Vec, in argv order; without it the
/// filter defaults to empty (the unfiltered read stays the default).
#[test]
fn inspect_claims_predicate_flag_repeats_into_vec() {
    let cli = Cli::parse_from([
        "morpholog",
        "inspect",
        "claims",
        "--database-url",
        "postgres:///morpholog_dev",
        "--predicate",
        "OfficialPrice",
        "--predicate",
        "CurrentOfficialPrice",
    ]);
    let Command::Inspect {
        what: Inspect::Claims(args),
    } = cli.command
    else {
        panic!("expected Inspect::Claims, got {:?}", cli.command);
    };
    assert_eq!(args.predicate, ["OfficialPrice", "CurrentOfficialPrice"]);
}

#[test]
fn inspect_claims_without_predicate_defaults_to_empty_filter() {
    let cli = Cli::parse_from([
        "morpholog",
        "inspect",
        "claims",
        "--database-url",
        "postgres:///morpholog_dev",
    ]);
    let Command::Inspect {
        what: Inspect::Claims(args),
    } = cli.command
    else {
        panic!("expected Inspect::Claims, got {:?}", cli.command);
    };
    assert!(args.predicate.is_empty(), "no flag means no filter");
}

/// `inspect claims --as-of <garbage>` is rejected by clap's
/// `FromStr` parser before any async work happens.
#[test]
fn inspect_claims_with_bad_as_of_errors_at_parse_time() {
    let err = Cli::try_parse_from([
        "morpholog",
        "inspect",
        "claims",
        "--database-url",
        "postgres:///morpholog_dev",
        "--as-of",
        "not-a-uuid",
    ])
    .expect_err("bad UUID must surface a clap parse error");
    // clap classifies FromStr failures as ValueValidation in recent
    // versions, InvalidValue in older ones; accept either.
    assert!(
        matches!(
            err.kind(),
            ErrorKind::ValueValidation | ErrorKind::InvalidValue
        ),
        "expected a value-validation/invalid-value error, got {:?}",
        err.kind()
    );
}

/// `inspect derived --as-of <uuid>` parses the optional flag.
#[test]
fn inspect_derived_with_as_of_parses_uuid() {
    let tid = "0192e000-0000-7000-8000-000000000002";
    let cli = Cli::parse_from([
        "morpholog",
        "inspect",
        "derived",
        "double_entry_ledger",
        "TrialBalanceRow",
        "--database-url",
        "postgres:///morpholog_dev",
        "--as-of",
        tid,
    ]);
    let Command::Inspect {
        what: Inspect::Derived(args),
    } = cli.command
    else {
        panic!("expected Inspect::Derived, got {:?}", cli.command);
    };
    assert_eq!(
        args.as_of,
        Some(AsOf::Transition(Uuid::parse_str(tid).unwrap()))
    );
}

/// `inspect audit --as-of <uuid>` is rejected by clap because
/// `InspectAuditArgs` does not declare the `--as-of` flag. Pins
/// the design decision that as-of does not apply to the audit
/// subcommand: the audit table IS the chronological record, and
/// the tail's coordinate is `--after`.
#[test]
fn inspect_audit_rejects_as_of_flag() {
    let err = Cli::try_parse_from([
        "morpholog",
        "inspect",
        "audit",
        "--database-url",
        "postgres:///morpholog_dev",
        "--as-of",
        "0192e000-0000-7000-8000-000000000001",
    ])
    .expect_err("inspect audit must not accept --as-of");
    assert_eq!(err.kind(), ErrorKind::UnknownArgument);
}

/// Same for `inspect outbox`.
#[test]
fn inspect_outbox_rejects_as_of_flag() {
    let err = Cli::try_parse_from([
        "morpholog",
        "inspect",
        "outbox",
        "--database-url",
        "postgres:///morpholog_dev",
        "--as-of",
        "0192e000-0000-7000-8000-000000000001",
    ])
    .expect_err("inspect outbox must not accept --as-of");
    assert_eq!(err.kind(), ErrorKind::UnknownArgument);
}

#[test]
fn propose_with_all_args_parses() {
    let cli = Cli::parse_from([
        "morpholog",
        "propose",
        "examples/03_double_entry_ledger/ledger.morph",
        "post_simple_entry",
        "--args",
        "[]",
        "--actor",
        "jordan",
        "--database-url",
        "postgres:///morpholog_dev",
    ]);
    let Command::Propose(args) = cli.command else {
        panic!("expected Propose, got {:?}", cli.command);
    };
    assert_eq!(
        args.file,
        std::path::PathBuf::from("examples/03_double_entry_ledger/ledger.morph")
    );
    assert_eq!(args.transformation.as_deref(), Some("post_simple_entry"));
    assert_eq!(args.args.as_deref(), Some("[]"));
    assert!(args.args_named.is_none());
    assert_eq!(args.actor.as_deref(), Some("jordan"));
    assert_eq!(args.db.database_url, "postgres:///morpholog_dev");
}

/// `propose --args-named '{...}'` parses with `args_named: Some(...)`
/// and `args: None`. Confirms the new flag plumbs through.
#[test]
fn propose_with_args_named_parses_into_the_named_slot() {
    let cli = Cli::parse_from([
        "morpholog",
        "propose",
        "examples/03_double_entry_ledger/ledger.morph",
        "post_simple_entry",
        "--args-named",
        r#"{"trade":"a"}"#,
        "--actor",
        "jordan",
        "--database-url",
        "postgres:///morpholog_dev",
    ]);
    let Command::Propose(args) = cli.command else {
        panic!("expected Propose, got {:?}", cli.command);
    };
    assert!(args.args.is_none(), "--args should not be set");
    assert_eq!(args.args_named.as_deref(), Some(r#"{"trade":"a"}"#));
}

/// Passing BOTH `--args` and `--args-named` must be rejected at
/// Clap-parse time so the run path never sees an ambiguous
/// request shape.
#[test]
fn propose_with_both_args_codecs_errors() {
    let err = Cli::try_parse_from([
        "morpholog",
        "propose",
        "examples/03_double_entry_ledger/ledger.morph",
        "post_simple_entry",
        "--args",
        "[]",
        "--args-named",
        "{}",
        "--actor",
        "jordan",
        "--database-url",
        "postgres:///morpholog_dev",
    ])
    .expect_err("both --args and --args-named should error");
    assert_eq!(err.kind(), ErrorKind::ArgumentConflict);
}

#[test]
fn propose_missing_args_flag_errors() {
    let err = Cli::try_parse_from([
        "morpholog",
        "propose",
        "examples/03_double_entry_ledger/ledger.morph",
        "post_simple_entry",
        "--actor",
        "jordan",
        "--database-url",
        "postgres:///morpholog_dev",
    ])
    .expect_err("missing --args should error");
    assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
}

#[test]
fn propose_missing_actor_flag_errors() {
    let err = Cli::try_parse_from([
        "morpholog",
        "propose",
        "examples/03_double_entry_ledger/ledger.morph",
        "post_simple_entry",
        "--args",
        "[]",
        "--database-url",
        "postgres:///morpholog_dev",
    ])
    .expect_err("missing --actor should error");
    assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
}

#[test]
fn propose_missing_positional_errors() {
    let err = Cli::try_parse_from([
        "morpholog",
        "propose",
        "examples/03_double_entry_ledger/ledger.morph",
        // missing transformation positional
        "--args",
        "[]",
        "--actor",
        "jordan",
        "--database-url",
        "postgres:///morpholog_dev",
    ])
    .expect_err("missing positional should error");
    assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
}

#[test]
fn inspect_derived_with_all_args_parses() {
    let cli = Cli::parse_from([
        "morpholog",
        "inspect",
        "derived",
        "examples/03_double_entry_ledger/ledger.morph",
        "TrialBalanceRow",
        "--database-url",
        "postgres:///morpholog_dev",
    ]);
    let Command::Inspect { what } = cli.command else {
        panic!("expected Inspect, got {:?}", cli.command);
    };
    let Inspect::Derived(args) = what else {
        panic!("expected Inspect::Derived, got {what:?}");
    };
    assert_eq!(
        args.file,
        std::path::PathBuf::from("examples/03_double_entry_ledger/ledger.morph")
    );
    assert_eq!(args.derived, "TrialBalanceRow");
    assert_eq!(args.db.database_url, "postgres:///morpholog_dev");
}

#[test]
fn inspect_derived_missing_derived_name_errors() {
    // Two positionals are required (file + derived name). Omit the
    // derived name; clap must surface MissingRequiredArgument rather
    // than silently taking the flag as the missing arg.
    let err = Cli::try_parse_from([
        "morpholog",
        "inspect",
        "derived",
        "examples/03_double_entry_ledger/ledger.morph",
        "--database-url",
        "postgres:///morpholog_dev",
    ])
    .expect_err("missing derived positional should error");
    assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
}

/// `inspect predicates <file.morph>` parses to `Inspect::Predicates`
/// with the file path on the args struct. No `--database-url` flag:
/// predicate declarations are programme metadata, not state.
#[test]
fn inspect_predicates_parses_with_file_argument() {
    let cli = Cli::parse_from([
        "morpholog",
        "inspect",
        "predicates",
        "examples/06_clinical_trial_enrolment/clinical_trial_enrolment.morph",
    ]);
    let Command::Inspect { what } = cli.command else {
        panic!("expected Inspect, got {:?}", cli.command);
    };
    let Inspect::Predicates(args) = what else {
        panic!("expected Inspect::Predicates, got {what:?}");
    };
    assert_eq!(
        args.file,
        std::path::PathBuf::from(
            "examples/06_clinical_trial_enrolment/clinical_trial_enrolment.morph"
        )
    );
}

/// Omitting the file positional must produce a clap
/// MissingRequiredArgument error.
#[test]
fn inspect_predicates_missing_file_errors() {
    let err = Cli::try_parse_from(["morpholog", "inspect", "predicates"])
        .expect_err("missing file positional should error");
    assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
}

/// `propose --trace` parses to a `ProposeArgs` with `trace: true`. All other
/// fields keep their existing behaviour.
#[test]
fn propose_with_trace_flag_parses() {
    let cli = Cli::parse_from([
        "morpholog",
        "propose",
        "examples/01_settlement_netting/netting.morph",
        "create_net_settlement",
        "--actor",
        "jordan",
        "--args",
        "[]",
        "--database-url",
        "postgres:///morpholog_dev",
        "--trace",
    ]);
    let Command::Propose(args) = cli.command else {
        panic!("expected Propose, got {:?}", cli.command);
    };
    assert!(args.trace, "expected trace flag to be set");
    assert_eq!(
        args.transformation.as_deref(),
        Some("create_net_settlement")
    );
    assert_eq!(args.actor.as_deref(), Some("jordan"));
}

/// Without `--trace`, `ProposeArgs.trace` defaults to false. The non-trace
/// path must not be affected by the flag.
#[test]
fn propose_without_trace_flag_defaults_to_false() {
    let cli = Cli::parse_from([
        "morpholog",
        "propose",
        "examples/01_settlement_netting/netting.morph",
        "create_net_settlement",
        "--actor",
        "jordan",
        "--args",
        "[]",
        "--database-url",
        "postgres:///morpholog_dev",
    ]);
    let Command::Propose(args) = cli.command else {
        panic!("expected Propose, got {:?}", cli.command);
    };
    assert!(!args.trace, "expected trace flag to default to false");
}

#[test]
fn propose_outcome_serialises_with_status_tag() {
    // Pin the JSON wire shape that the CLI emits for outcomes.
    // The codec uses a `status` discriminant via serde's tagged-enum
    // representation; the CLI relies on this so that scripts can
    // parse stdout and branch on `.status`.
    use morpholog_core::{ClaimInstance, Subject};
    use morpholog_postgres::PgProposalOutcome;
    use uuid::Uuid;

    let committed = PgProposalOutcome::Committed {
        transition_id: Uuid::nil(),
        actor: Subject::from("jordan"),
        asserted_claims: vec![ClaimInstance {
            predicate: "Foo".into(),
            args: vec![],
        }],
        retracted_claims: vec![],
        emitted_intents: vec![],
    };
    let json = serde_json::to_string(&committed).unwrap();
    assert!(
        json.contains(r#""status":"committed""#),
        "committed outcome must carry status=committed, got: {json}"
    );
    assert!(json.contains(r#""transition_id":"00000000-0000-0000-0000-000000000000""#));
    assert!(
        json.contains(r#""actor":{"type":"subject","value":"jordan"}"#),
        "committed outcome must carry actor on the wire, got: {json}"
    );

    let rejected = PgProposalOutcome::Rejected {
        reason: "require failed".to_string(),
        rule: None,
        witness: Vec::new(),
    };
    let json = serde_json::to_string(&rejected).unwrap();
    assert!(
        !json.contains("witness"),
        "an empty witness must be absent, not `[]` - otherwise every \
         pre-witness envelope changes shape: {json}"
    );
    assert!(
        json.contains(r#""status":"rejected""#),
        "rejected outcome must carry status=rejected, got: {json}"
    );
    assert!(json.contains(r#""reason":"require failed""#));
}

#[test]
fn missing_required_argument_surfaces_as_clap_error() {
    let err = Cli::try_parse_from(["morpholog"]).expect_err("no subcommand should error");
    // clap 4 surfaces this as DisplayHelpOnMissingArgumentOrSubcommand;
    // older versions used MissingSubcommand. Accept either so the
    // test does not break under minor clap updates.
    assert!(
        matches!(
            err.kind(),
            ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                | ErrorKind::MissingRequiredArgument
                | ErrorKind::MissingSubcommand
        ),
        "expected a missing-argument/subcommand error, got {:?}",
        err.kind()
    );
}

/// `check --ir` owns IR output; a bare `parse` subcommand must
/// stay unknown. Pins both.
#[test]
fn parse_is_gone_and_check_ir_replaces_it() {
    let err = Cli::try_parse_from(["morpholog", "parse", "demo.morph"])
        .expect_err("parse must no longer be a subcommand");
    assert_eq!(err.kind(), ErrorKind::InvalidSubcommand);

    let cli = Cli::parse_from(["morpholog", "check", "demo.morph", "--ir"]);
    let Command::Check(args) = cli.command else {
        panic!("expected Command::Check, got {:?}", cli.command);
    };
    assert!(args.ir);
}

/// `check -v` parses to `CheckArgs { verbose: true }`. Pins the
/// short form alongside the long one.
#[test]
fn check_with_verbose_flag_parses() {
    let cli = Cli::parse_from(["morpholog", "check", "demo.morph", "-v"]);
    let Command::Check(args) = cli.command else {
        panic!("expected Command::Check, got {:?}", cli.command);
    };
    assert!(args.verbose, "expected -v to set verbose");
    assert_eq!(args.file.as_os_str(), "demo.morph");
}

/// Without `--verbose`, `CheckArgs.verbose` defaults to false -
/// the silent-success contract scripts rely on stays the default.
#[test]
fn check_without_verbose_defaults_to_false() {
    let cli = Cli::parse_from(["morpholog", "check", "demo.morph"]);
    let Command::Check(args) = cli.command else {
        panic!("expected Command::Check, got {:?}", cli.command);
    };
    assert!(!args.verbose, "expected verbose to default to false");
}

#[test]
fn check_missing_file_argument_errors() {
    let err = Cli::try_parse_from(["morpholog", "check"]).expect_err("expected clap parse error");
    assert!(
        matches!(err.kind(), ErrorKind::MissingRequiredArgument),
        "expected missing-argument error, got {:?}",
        err.kind()
    );
}

#[test]
fn explain_with_all_args_parses() {
    let cli = Cli::parse_from([
        "morpholog",
        "explain",
        "model.morph",
        "issue_credit",
        "--args",
        "[]",
        "--actor",
        "jordan",
        "--database-url",
        "postgres:///morpholog_dev",
    ]);
    let Command::Explain(args) = cli.command else {
        panic!("expected Explain, got {:?}", cli.command);
    };
    assert_eq!(args.file.as_os_str(), "model.morph");
    assert_eq!(args.transformation, "issue_credit");
    assert_eq!(args.args.as_deref(), Some("[]"));
    assert!(args.args_named.is_none());
    assert_eq!(args.actor, "jordan");
    assert_eq!(args.db.database_url, "postgres:///morpholog_dev");
    assert!(!args.json, "expected --json to default to false");
}

/// `explain --args-named` parses with `args_named: Some(...)` and
/// `args: None`. Mirrors `run_with_args_named_parses_into_the_named_slot`
/// to confirm explain plumbs the new flag identically.
#[test]
fn explain_with_args_named_parses_into_the_named_slot() {
    let cli = Cli::parse_from([
        "morpholog",
        "explain",
        "model.morph",
        "issue_credit",
        "--args-named",
        r#"{"x":"y"}"#,
        "--actor",
        "jordan",
        "--database-url",
        "postgres:///morpholog_dev",
    ]);
    let Command::Explain(args) = cli.command else {
        panic!("expected Explain, got {:?}", cli.command);
    };
    assert!(args.args.is_none(), "--args should not be set");
    assert_eq!(args.args_named.as_deref(), Some(r#"{"x":"y"}"#));
}

/// Mutual exclusion at parse time for explain too: passing both
/// `--args` and `--args-named` is a hard error.
#[test]
fn explain_with_both_args_codecs_errors() {
    let err = Cli::try_parse_from([
        "morpholog",
        "explain",
        "model.morph",
        "issue_credit",
        "--args",
        "[]",
        "--args-named",
        "{}",
        "--actor",
        "jordan",
        "--database-url",
        "postgres:///morpholog_dev",
    ])
    .expect_err("both --args and --args-named should error");
    assert_eq!(err.kind(), ErrorKind::ArgumentConflict);
}

#[test]
fn explain_with_json_flag_parses() {
    let cli = Cli::parse_from([
        "morpholog",
        "explain",
        "model.morph",
        "issue_credit",
        "--args",
        "[]",
        "--actor",
        "jordan",
        "--database-url",
        "postgres:///morpholog_dev",
        "--json",
    ]);
    let Command::Explain(args) = cli.command else {
        panic!("expected Explain, got {:?}", cli.command);
    };
    assert!(args.json, "expected --json flag to be set");
}

#[test]
fn explain_missing_actor_flag_errors() {
    let err = Cli::try_parse_from([
        "morpholog",
        "explain",
        "model.morph",
        "issue_credit",
        "--args",
        "[]",
        "--database-url",
        "postgres:///morpholog_dev",
    ])
    .expect_err("missing --actor should error");
    assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
}

/// `morpholog schema <file> <transformation>` parses into the
/// expected positional args. The schema subcommand takes no flags
/// (no `--json`, no `--database-url`), so the test pins that the
/// minimal positional surface is what the embedder will type.
#[test]
fn schema_with_file_and_transformation_parses() {
    let cli = Cli::parse_from([
        "morpholog",
        "schema",
        "examples/10_trade_lifecycle/trade_lifecycle.morph",
        "capture_trade",
    ]);
    let Command::Schema(args) = cli.command else {
        panic!("expected Command::Schema, got {:?}", cli.command);
    };
    assert_eq!(args.transformation.as_deref(), Some("capture_trade"));
    assert!(args.intent.is_none());
    assert_eq!(
        args.file.expect("file is present").to_string_lossy(),
        "examples/10_trade_lifecycle/trade_lifecycle.morph"
    );
}

/// `schema --result` needs no `.morph` file (the envelope contract
/// is programme-independent) and conflicts with every per-programme
/// mode.
#[test]
fn schema_result_parses_without_a_file() {
    let cli = Cli::parse_from(["morpholog", "schema", "--result"]);
    let Command::Schema(args) = cli.command else {
        panic!("expected Command::Schema, got {:?}", cli.command);
    };
    assert!(args.result);
    assert!(args.file.is_none());
}

#[test]
fn schema_result_conflicts_with_per_programme_modes() {
    for extra in [
        vec!["file.morph", "capture_trade"],
        vec!["file.morph", "--intent", "X"],
        vec!["file.morph", "--all"],
    ] {
        let mut argv = vec!["morpholog", "schema", "--result"];
        argv.extend(extra.clone());
        let err = Cli::try_parse_from(argv)
            .expect_err("--result with a per-programme mode should conflict");
        assert_eq!(err.kind(), ErrorKind::ArgumentConflict, "case {extra:?}");
    }
}

/// Without `--result`, schema still demands a file plus exactly one
/// mode - the pre-existing contract survives the file becoming
/// optional.
#[test]
fn schema_without_result_still_requires_a_file_and_mode() {
    let err = Cli::try_parse_from(["morpholog", "schema"])
        .expect_err("bare schema should be missing required args");
    assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
}

/// `--intent <Type>` parses as the payload-schema alternative to a
/// positional transformation name.
#[test]
fn schema_with_intent_parses() {
    let cli = Cli::parse_from([
        "morpholog",
        "schema",
        "file.morph",
        "--intent",
        "TradeSettlementRequested",
    ]);
    let Command::Schema(args) = cli.command else {
        panic!("expected Command::Schema, got {:?}", cli.command);
    };
    assert_eq!(args.intent.as_deref(), Some("TradeSettlementRequested"));
    assert!(args.transformation.is_none());
}

/// Supplying both a transformation and `--intent` is a conflict.
#[test]
fn schema_transformation_and_intent_conflict() {
    let err = Cli::try_parse_from(["morpholog", "schema", "file.morph", "cap", "--intent", "X"])
        .expect_err("transformation + --intent should conflict");
    assert_eq!(err.kind(), ErrorKind::ArgumentConflict);
}

/// Neither a transformation nor `--intent` should error at
/// clap-parse time, before any file IO happens.
#[test]
fn schema_missing_transformation_errors() {
    let err = Cli::try_parse_from(["morpholog", "schema", "file.morph"])
        .expect_err("missing transformation name should error");
    assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
}

#[test]
fn generate_views_defaults_to_the_morpholog_views_schema() {
    let cli = Cli::parse_from(["morpholog", "generate", "views", "model.morph"]);
    let Command::Generate {
        what: GenerateCmd::Views(args),
    } = cli.command
    else {
        panic!("expected generate views, got {:?}", cli.command);
    };
    assert_eq!(args.schema, "morpholog_views");
    assert!(args.out.is_none());
}

#[test]
fn generate_views_accepts_schema_and_out() {
    let cli = Cli::parse_from([
        "morpholog",
        "generate",
        "views",
        "model.morph",
        "--schema",
        "analytics",
        "--out",
        "views.sql",
    ]);
    let Command::Generate {
        what: GenerateCmd::Views(args),
    } = cli.command
    else {
        panic!("expected generate views, got {:?}", cli.command);
    };
    assert_eq!(args.schema, "analytics");
    assert_eq!(args.out.as_deref(), Some(std::path::Path::new("views.sql")));
}

#[test]
fn refresh_derived_takes_a_file_and_database_url() {
    let cli = Cli::parse_from([
        "morpholog",
        "refresh",
        "derived",
        "model.morph",
        "--database-url",
        "postgres:///morpholog_dev",
    ]);
    let Command::Refresh {
        what: RefreshCmd::Derived(args),
    } = cli.command
    else {
        panic!("expected refresh derived, got {:?}", cli.command);
    };
    assert_eq!(args.file, std::path::Path::new("model.morph"));
    assert_eq!(args.db.database_url, "postgres:///morpholog_dev");
}

#[test]
fn refresh_derived_requires_a_file() {
    let err = Cli::try_parse_from(["morpholog", "refresh", "derived"])
        .expect_err("missing file should error");
    assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
}

mod exit_path {
    use super::*;

    /// The point of returning instead of exiting: a command that
    /// refuses hands its caller an error and the caller is still
    /// running to receive it. Written as a test because it could not
    /// have been one before - `std::process::exit` inside the parse
    /// gate would have taken the test runner down with it.
    #[test]
    fn a_refused_command_returns_rather_than_ending_the_process() {
        // A unique path per run, and no UTF-8 assumption about where
        // the temp dir lives: this suite runs in parallel with itself.
        let file = tempfile::Builder::new()
            .suffix(".morph")
            .tempfile()
            .unwrap();
        std::fs::write(
            file.path(),
            "program p\npredicate P(x: Subject)\ntransformation t(x):\n    admit Q(x)\n",
        )
        .unwrap();
        let args = Cli::parse_from([
            std::ffi::OsStr::new("morpholog"),
            std::ffi::OsStr::new("check"),
            file.path().as_os_str(),
        ]);
        let Command::Check(check) = args.command else {
            panic!("expected check");
        };
        let err = commands::check::run(check).expect_err("an undeclared predicate is refused");
        assert!(
            err.is::<commands::AlreadyReported>(),
            "the diagnostics were printed, so main must add nothing: {err:?}"
        );
        // Still here - which is the whole assertion.
    }

    /// The commit-unknown exit is its own number, and nothing else
    /// borrows it: an ordinary failure, even one wrapping the marker
    /// in context, exits 1 only when the marker is absent.
    #[test]
    fn a_commit_outcome_unknown_exits_three_and_nothing_else_does() {
        let unknown: anyhow::Error = commands::CommitOutcomeUnknown("reset".into()).into();
        assert_eq!(exit_code_for(&unknown), 3);
        assert_eq!(exit_code_for(&unknown.context("wrapped")), 3);
        assert_eq!(exit_code_for(&anyhow::anyhow!("not committed")), 1);
        assert_eq!(
            commands::EXIT_COMMIT_OUTCOME_UNKNOWN,
            3,
            "2 is clap's usage error"
        );
    }
}
