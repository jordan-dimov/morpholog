//! `morpholog generate local-days` and `generate business-days` end to
//! end: the rows across a clock change, the inclusive range, whole-run
//! validation with nothing on stdout when refused, the fail-closed tzdb
//! version, and the rows admitted by example 23 and by a minimal
//! business-day programme through both `propose --batch` and `transact`.
//! The zone is read from a committed zoneinfo fixture, so the dates and
//! the version stamp do not depend on the machine's tzdata.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::{bin, database_url, repo_root, reset_db};

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use serde_json::Value;

fn example_23() -> PathBuf {
    repo_root().join("examples/23_delivery_hour_limits/delivery_hour_limits.morph")
}

fn fixture(name: &str) -> PathBuf {
    repo_root()
        .join("crates/morpholog-cli/tests/fixtures")
        .join(name)
}

fn zoneinfo() -> String {
    fixture("zoneinfo").to_str().unwrap().to_string()
}

fn run(args: &[&str]) -> Output {
    Command::new(bin()).args(args).output().expect("cli runs")
}

fn local_days(calendar: &str, from: &str, until: &str) -> Output {
    let file = example_23();
    let zoneinfo = zoneinfo();
    run(&[
        "generate",
        "local-days",
        file.to_str().unwrap(),
        "publish_delivery_day",
        "--actor",
        "cal_uk",
        "--arg",
        &format!("calendar={calendar}"),
        "--zone",
        "Europe/London",
        "--zoneinfo",
        &zoneinfo,
        "--from",
        from,
        "--until",
        until,
    ])
}

fn business_days(from: &str, until: &str) -> Output {
    let file = fixture("business_days.morph");
    let holidays = fixture("uk_holidays_2026.json");
    run(&[
        "generate",
        "business-days",
        file.to_str().unwrap(),
        "admit_business_day",
        "--actor",
        "ops",
        "--arg",
        "calendar=uk",
        "--holidays",
        holidays.to_str().unwrap(),
        "--from",
        from,
        "--until",
        until,
    ])
}

fn rows(output: &Output) -> Vec<Value> {
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).expect("a JSON row"))
        .collect()
}

fn named<'a>(row: &'a Value, field: &str) -> &'a str {
    row["args_named"][field].as_str().unwrap()
}

/// Refused whole: a non-zero exit, nothing on stdout, and the reason on
/// stderr naming what was wrong.
fn refused(output: &Output, names: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "should refuse: {stderr}");
    assert!(
        output.stdout.is_empty(),
        "nothing is written when refused, got {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(stderr.contains(names), "stderr names `{names}`: {stderr}");
}

#[test]
fn a_local_day_is_23_24_or_25_hours_across_the_clock_changes() {
    let spring = rows(&local_days("uk", "2026-03-28", "2026-03-30"));
    let autumn = rows(&local_days("uk", "2026-10-24", "2026-10-26"));
    let bounds = |row: &Value| {
        (
            named(row, "local_date").to_string(),
            named(row, "starts_at").to_string(),
            named(row, "ends_at").to_string(),
        )
    };
    let day =
        |date: &str, from: &str, to: &str| (date.to_string(), from.to_string(), to.to_string());
    assert_eq!(
        spring.iter().map(bounds).collect::<Vec<_>>(),
        vec![
            day("2026-03-28", "2026-03-28T00:00:00Z", "2026-03-29T00:00:00Z"),
            day("2026-03-29", "2026-03-29T00:00:00Z", "2026-03-29T23:00:00Z"),
            day("2026-03-30", "2026-03-29T23:00:00Z", "2026-03-30T23:00:00Z"),
        ]
    );
    assert_eq!(
        autumn.iter().map(bounds).collect::<Vec<_>>(),
        vec![
            day("2026-10-24", "2026-10-23T23:00:00Z", "2026-10-24T23:00:00Z"),
            day("2026-10-25", "2026-10-24T23:00:00Z", "2026-10-26T00:00:00Z"),
            day("2026-10-26", "2026-10-26T00:00:00Z", "2026-10-27T00:00:00Z"),
        ]
    );
    for row in spring.iter().chain(&autumn) {
        assert_eq!(row["transformation"], "publish_delivery_day");
        assert_eq!(row["actor"], "cal_uk");
        assert_eq!(named(row, "calendar"), "uk");
        assert_eq!(named(row, "tzdb_version"), "2026e");
    }
}

#[test]
fn the_range_includes_both_ends() {
    assert_eq!(
        rows(&local_days("uk", "2026-01-01", "2026-12-31")).len(),
        365
    );
    assert_eq!(rows(&local_days("uk", "2026-07-01", "2026-07-01")).len(), 1);
    refused(
        &local_days("uk", "2026-01-02", "2026-01-01"),
        "inclusive at both ends",
    );
}

#[test]
fn business_days_are_weekdays_less_the_listed_holidays() {
    let year = rows(&business_days("2026-01-01", "2026-12-31"));
    let dates: Vec<&str> = year.iter().map(|r| named(r, "date")).collect();
    // 261 weekdays in 2026, less the six listed holidays that fall on one.
    assert_eq!(dates.len(), 253);
    assert_eq!(dates[0], "2026-01-02");
    for (holiday, following) in [("2026-04-03", "2026-04-07"), ("2026-12-25", "2026-12-29")] {
        assert!(!dates.contains(&holiday), "{holiday} is a holiday");
        assert!(dates.contains(&following), "{following} is a working day");
    }
    assert!(!dates.contains(&"2026-01-03"), "a Saturday");
    assert!(!dates.contains(&"2026-01-04"), "a Sunday");
    for row in &year {
        assert_eq!(named(row, "authority"), "uk_bank_holidays");
        assert_eq!(named(row, "version"), "2026-03");
        assert_eq!(named(row, "calendar"), "uk");
    }
}

#[test]
fn the_rows_are_refused_whole_when_they_do_not_fit_the_transformation() {
    let file = example_23();
    let zoneinfo = zoneinfo();
    let base = |transformation: &str, extra: &[&str]| {
        let mut args = vec![
            "generate",
            "local-days",
            file.to_str().unwrap(),
            transformation,
            "--actor",
            "cal_uk",
            "--zone",
            "Europe/London",
            "--zoneinfo",
            &zoneinfo,
            "--from",
            "2026-01-01",
            "--until",
            "2026-01-03",
        ];
        args.extend_from_slice(extra);
        run(&args)
    };
    // The parameters do not match the generated fields plus --arg.
    refused(
        &base("capture_day", &["--arg", "calendar=uk"]),
        "Not supplied: [book, trade, mw]",
    );
    refused(
        &base("publish_delivery_day", &[]),
        "Not supplied: [calendar]",
    );
    refused(
        &base(
            "publish_delivery_day",
            &["--arg", "calendar=uk", "--arg", "extra=1"],
        ),
        "Not a parameter: [extra]",
    );
    // A generated field cannot be overridden.
    refused(
        &base(
            "publish_delivery_day",
            &["--arg", "calendar=uk", "--arg", "tzdb_version=2020a"],
        ),
        "a generated value is never overridden",
    );
    refused(
        &base("publish_day", &[]),
        "transformation `publish_day` not found",
    );

    // The names fit but a kind does not: caught by the decoder, with
    // nothing written for the days before it.
    let mistyped = fixture("mistyped_days.morph");
    refused(
        &run(&[
            "generate",
            "local-days",
            mistyped.to_str().unwrap(),
            "publish_day",
            "--actor",
            "cal",
            "--zone",
            "Europe/London",
            "--zoneinfo",
            &zoneinfo,
            "--from",
            "2026-01-01",
            "--until",
            "2026-01-03",
        ]),
        "`tzdb_version` is Decimal",
    );
}

#[test]
fn the_zone_is_a_name_inside_the_directory_and_the_version_must_be_declared() {
    let file = example_23();
    let with = |zone: &str, zoneinfo: &str| {
        run(&[
            "generate",
            "local-days",
            file.to_str().unwrap(),
            "publish_delivery_day",
            "--actor",
            "cal_uk",
            "--arg",
            "calendar=uk",
            "--zone",
            zone,
            "--zoneinfo",
            zoneinfo,
            "--from",
            "2026-01-01",
            "--until",
            "2026-01-01",
        ])
    };
    let good = zoneinfo();
    refused(
        &with("../zoneinfo/Europe/London", &good),
        "is not a zone name",
    );
    refused(&with("/etc/passwd", &good), "is not a zone name");
    refused(&with("Europe/Nowhere", &good), "Europe/Nowhere");

    // No tzdata.zi: refused before the zone is read.
    let bare = tempfile::tempdir().unwrap();
    std::fs::create_dir(bare.path().join("Europe")).unwrap();
    std::fs::copy(
        fixture("zoneinfo/Europe/London"),
        bare.path().join("Europe/London"),
    )
    .unwrap();
    refused(
        &with("Europe/London", bare.path().to_str().unwrap()),
        "cannot determine the tzdb version",
    );
    // A tzdata.zi without the version header: the same refusal.
    std::fs::write(bare.path().join("tzdata.zi"), "# no version here\n").unwrap();
    refused(
        &with("Europe/London", bare.path().to_str().unwrap()),
        "does not start with `# version <release>`",
    );
    std::fs::write(bare.path().join("tzdata.zi"), "# version 2026e\n").unwrap();
    assert_eq!(
        rows(&with("Europe/London", bare.path().to_str().unwrap())).len(),
        1
    );
}

#[test]
fn a_holiday_list_is_one_authority_and_version_with_dates() {
    let file = fixture("business_days.morph");
    let with = |list: &str| {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("holidays.json");
        std::fs::write(&path, list).unwrap();
        run(&[
            "generate",
            "business-days",
            file.to_str().unwrap(),
            "admit_business_day",
            "--actor",
            "ops",
            "--arg",
            "calendar=uk",
            "--holidays",
            path.to_str().unwrap(),
            "--from",
            "2026-01-01",
            "--until",
            "2026-01-07",
        ])
    };
    refused(
        &with(r#"{"authority": "a", "version": "1", "holidays": ["2026-01-01"], "zone": "x"}"#),
        "is not a holiday list",
    );
    refused(
        &with(r#"{"authority": "", "version": "1", "holidays": []}"#),
        "`authority` and `version` must both be given",
    );
    refused(
        &with(r#"{"authority": "a", "version": "1", "holidays": ["2026-01-01", "2026-01-01"]}"#),
        "2026-01-01 is listed twice",
    );
    refused(
        &with(r#"{"authority": "a", "version": "1", "holidays": ["01/01/2026"]}"#),
        "is not a holiday list",
    );
    let week = rows(&with(
        r#"{"authority": "a", "version": "1", "holidays": ["2026-01-01"]}"#,
    ));
    assert_eq!(
        week.iter().map(|r| named(r, "date")).collect::<Vec<_>>(),
        ["2026-01-02", "2026-01-05", "2026-01-06", "2026-01-07"]
    );
}

fn with_db(args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(bin())
        .args(args)
        .args(["--database-url", &database_url()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("cli runs");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn propose(file: &str, transformation: &str, actor: &str, args_named: &str) {
    let output = with_db(
        &[
            "propose",
            file,
            transformation,
            "--actor",
            actor,
            "--args-named",
            args_named,
        ],
        "",
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Every receipt of a batch committed, and there were as many as rows.
fn all_committed(output: &Output, expected: usize) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipts: Vec<Value> = String::from_utf8(output.stdout.clone())
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(receipts.len(), expected);
    for receipt in &receipts {
        assert_eq!(receipt["status"], "committed", "{receipt}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn example_23_admits_the_generated_days_by_batch_and_by_transact() {
    reset_db().await;
    let file = example_23();
    let file = file.to_str().unwrap();
    propose(
        file,
        "appoint_calendar_publisher",
        "ops",
        r#"{"calendar":"uk","publisher":"cal_uk"}"#,
    );
    propose(
        file,
        "appoint_calendar_publisher",
        "ops",
        r#"{"calendar":"uk_atomic","publisher":"cal_uk"}"#,
    );

    // Both clock changes, so the civil-day rule saw 23, 24 and 25 hours.
    let spring = String::from_utf8(local_days("uk", "2026-03-28", "2026-03-30").stdout).unwrap();
    let autumn = String::from_utf8(local_days("uk", "2026-10-24", "2026-10-26").stdout).unwrap();
    let batch = format!("{spring}{autumn}");
    all_committed(&with_db(&["propose", file, "--batch", "-"], &batch), 6);

    // The same rows publish all or nothing through transact.
    let acts =
        String::from_utf8(local_days("uk_atomic", "2026-10-24", "2026-10-26").stdout).unwrap();
    let output = with_db(&["transact", file, "--acts", "-"], &acts);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let outcome: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(outcome["status"], "committed", "{outcome}");
    assert_eq!(outcome["acts"].as_array().unwrap().len(), 3);

    // A day published twice is refused by the programme, not the generator.
    let again = with_db(&["propose", file, "--batch", "-"], &spring);
    let receipts: Vec<Value> = String::from_utf8(again.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert!(
        receipts.iter().all(|r| r["status"] == "rejected"),
        "{receipts:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_business_day_programme_admits_the_generated_year() {
    reset_db().await;
    let file = fixture("business_days.morph");
    let year = String::from_utf8(business_days("2026-01-01", "2026-12-31").stdout).unwrap();
    all_committed(
        &with_db(&["propose", file.to_str().unwrap(), "--batch", "-"], &year),
        253,
    );
}
