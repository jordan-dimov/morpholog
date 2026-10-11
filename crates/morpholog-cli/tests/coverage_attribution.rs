//! `inspect coverage` names what refused each transformation's proposals,
//! and sets apart the names in history the programme does not declare.
//! Two programmes share one database here, as embedders' do, so the
//! report is held to what the record can show: names, not owners.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::{database_url, reset_db};

use std::process::Command;

const DESK: &str = "\
program desk

predicate Desk(desk: Subject)
predicate Limit(desk: Subject, cap: Decimal)
    unique by (desk)
predicate Order(order: Subject, desk: Subject, qty: Decimal)
    unique by (order)

invariant within_limit:
    Order(o, d, q) and Limit(d, cap) implies q <= cap

transformation open_desk(desk, cap):
    admit Desk(desk)
    admit Limit(desk, cap)

transformation open_bare_desk(desk):
    admit Desk(desk)

transformation place(order, desk, qty):
    require desk_is_open: Desk(desk)
    require 0 < qty
    bind within_limit: Limit(desk, cap)
    admit Order(order, desk, qty)

transformation amend(order, desk, qty):
    require desk_is_open: Desk(desk)
    admit Order(order, desk, qty)
";

/// Another programme on the same database, with one transformation name
/// in common.
const NOTES: &str = "\
program notes

predicate Note(note: Subject)

transformation place(order):
    admit Note(order)

transformation jot(note):
    admit Note(note)
";

fn propose(file: &std::path::Path, transformation: &str, args: serde_json::Value) -> String {
    let output = Command::new(common::bin())
        .arg("propose")
        .arg(file)
        .args([transformation, "--actor", "trader", "--args-named"])
        .arg(args.to_string())
        .args(["--database-url", &database_url()])
        .output()
        .unwrap();
    let outcome: serde_json::Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&output.stderr)));
    outcome["status"].as_str().unwrap().to_string()
}

fn coverage(file: &std::path::Path, json: bool) -> String {
    let mut command = Command::new(common::bin());
    command.args(["inspect", "coverage"]).arg(file);
    if json {
        command.arg("--json");
    }
    let output = command
        .args(["--database-url", &database_url()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn each_refusal_is_attributed_to_its_transformation_by_kind_and_rule() {
    reset_db().await;
    let desk = common::write_fixture("coverage_desk", DESK);
    let notes = common::write_fixture("coverage_notes", NOTES);
    let p = |order: &str, desk_id: &str, qty: &str| serde_json::json!({"order": order, "desk": desk_id, "qty": qty});

    assert_eq!(
        propose(
            &desk.path,
            "open_desk",
            serde_json::json!({"desk": "d1", "cap": "100"})
        ),
        "committed"
    );
    assert_eq!(
        propose(
            &desk.path,
            "open_bare_desk",
            serde_json::json!({"desk": "d2"})
        ),
        "committed"
    );
    assert_eq!(
        propose(&desk.path, "place", p("o1", "d1", "50")),
        "committed"
    );
    // place: two refusals by the named gate, one by the unnamed one, one
    // by the bind, one by the invariant whose name the bind shares.
    assert_eq!(
        propose(&desk.path, "place", p("o2", "nowhere", "5")),
        "rejected"
    );
    assert_eq!(
        propose(&desk.path, "place", p("o3", "nowhere", "5")),
        "rejected"
    );
    assert_eq!(propose(&desk.path, "place", p("o4", "d1", "0")), "rejected");
    assert_eq!(propose(&desk.path, "place", p("o5", "d2", "5")), "rejected");
    assert_eq!(
        propose(&desk.path, "place", p("o6", "d1", "500")),
        "rejected"
    );
    // amend: the same gate name, refused in another transformation.
    assert_eq!(
        propose(&desk.path, "amend", p("o1", "nowhere", "5")),
        "rejected"
    );
    // The other programme: one name in common, one of its own.
    assert_eq!(
        propose(&notes.path, "place", serde_json::json!({"order": "n1"})),
        "committed"
    );
    assert_eq!(
        propose(&notes.path, "jot", serde_json::json!({"note": "n2"})),
        "committed"
    );

    let report: serde_json::Value = serde_json::from_str(&coverage(&desk.path, true)).unwrap();
    let usage = |name: &str| -> serde_json::Value {
        report["transformations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["transformation"] == name)
            .unwrap_or_else(|| panic!("{name} missing: {report}"))
            .clone()
    };
    let causes = |name: &str| -> Vec<(String, String, u64)> {
        usage(name)["refusals"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| {
                (
                    c["kind"].as_str().unwrap().to_string(),
                    c["rule"].as_str().unwrap().to_string(),
                    c["proposals_refused"].as_u64().unwrap(),
                )
            })
            .collect()
    };
    let cause = |kind: &str, rule: &str, n: u64| (kind.to_string(), rule.to_string(), n);

    assert_eq!(
        causes("place"),
        vec![
            cause("require", "0 < qty", 1),
            cause("require", "desk_is_open", 2),
            cause("bind", "within_limit", 1),
            cause("invariant", "within_limit", 1),
        ],
        "ordered by kind, then rule; one name under two kinds stays two causes"
    );
    assert_eq!(
        usage("place")["proposals_refused"],
        5,
        "the causes sum to the total"
    );
    assert_eq!(
        causes("amend"),
        vec![cause("require", "desk_is_open", 1)],
        "a gate name shared by two transformations counts for each apart"
    );
    let open = usage("open_desk");
    assert!(
        open.get("refusals").is_none() && open.get("proposals_refused").is_none(),
        "a transformation never refused keeps the shape it had: {open}"
    );

    // History is read by name: the other programme's `place` counts with
    // this one's, and its `jot` is flagged as undeclared, not as foreign.
    assert_eq!(usage("place")["transitions"], 2);
    assert_eq!(usage("jot")["not_in_programme"], true);
    assert!(usage("place").get("not_in_programme").is_none());

    let prose = coverage(&desk.path, false);
    let (declared, undeclared) = prose
        .split_once("names in history that this programme does not declare:")
        .unwrap_or_else(|| panic!("no section for undeclared names:\n{prose}"));
    assert!(undeclared.contains("jot - 1 transition(s)"), "{undeclared}");
    assert!(!declared.contains("jot"), "{declared}");
    assert!(
        undeclared.contains("the record cannot tell which"),
        "the section claims no owner: {undeclared}"
    );
    for line in [
        "require `desk_is_open`: 2",
        "require `0 < qty`: 1",
        "bind `within_limit`: 1",
        "invariant `within_limit`: 1",
    ] {
        assert!(declared.contains(line), "missing `{line}`:\n{declared}");
    }
    assert!(
        prose.contains("whichever programme wrote it"),
        "the legend says history is read whole: {prose}"
    );
    assert!(!prose.contains("note: appears"), "{prose}");
}
