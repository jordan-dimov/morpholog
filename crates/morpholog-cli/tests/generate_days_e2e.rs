//! `morpholog generate local-days` and `generate business-days` end to
//! end: the rows across a clock change, the inclusive range, whole-run
//! validation with nothing on stdout when refused, the fail-closed tzdb
//! version, and the rows admitted by example 23 and by a minimal
//! business-day programme through both `propose --batch` and `transact`.
//! The zone is read from a committed zoneinfo fixture, so the dates and
//! the version stamp do not depend on the machine's tzdata.

#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "helper functions here fail fast on a broken fixture, and clippy exempts only `#[test]` functions"
)]

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
    // 261 weekdays in 2026, less the eight listed holidays, all on weekdays.
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
        "transformation `publish_day` is not declared",
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
        &with(
            r#"{"authority": "a", "version": "1", "from": "2026-01-01", "until": "2026-12-31", "holidays": ["2026-01-01"], "zone": "x"}"#,
        ),
        "is not a holiday list",
    );
    // The covered dates are required, not assumed.
    refused(
        &with(r#"{"authority": "a", "version": "1", "holidays": ["2026-01-01"]}"#),
        "is not a holiday list",
    );
    refused(
        &with(
            r#"{"authority": "a", "version": "1", "from": "2026-12-31", "until": "2026-01-01", "holidays": []}"#,
        ),
        "which is empty",
    );
    refused(
        &with(
            r#"{"authority": "a", "version": "1", "from": "2026-01-01", "until": "2026-06-30", "holidays": ["2026-12-25"]}"#,
        ),
        "2026-12-25 is outside the dates the list covers",
    );
    // A range the list does not cover: a weekday absent from an
    // incomplete list is not a business day.
    refused(
        &with(
            r#"{"authority": "a", "version": "1", "from": "2026-01-01", "until": "2026-01-06", "holidays": []}"#,
        ),
        "the range 2026-01-01 to 2026-01-07 is not inside it",
    );
    refused(
        &with(
            r#"{"authority": "", "version": "1", "from": "2026-01-01", "until": "2026-12-31", "holidays": []}"#,
        ),
        "`authority` and `version` must both be given",
    );
    refused(
        &with(
            r#"{"authority": "a", "version": "1", "from": "2026-01-01", "until": "2026-12-31", "holidays": ["2026-01-01", "2026-01-01"]}"#,
        ),
        "2026-01-01 is listed twice",
    );
    refused(
        &with(
            r#"{"authority": "a", "version": "1", "from": "2026-01-01", "until": "2026-12-31", "holidays": ["01/01/2026"]}"#,
        ),
        "is not a holiday list",
    );
    let week = rows(&with(
        r#"{"authority": "a", "version": "1", "from": "2026-01-01", "until": "2026-12-31", "holidays": ["2026-01-01"]}"#,
    ));
    assert_eq!(
        week.iter().map(|r| named(r, "date")).collect::<Vec<_>>(),
        ["2026-01-02", "2026-01-05", "2026-01-06", "2026-01-07"]
    );
}

fn local_days_in(zone: &str, zoneinfo: &str, from: &str, until: &str) -> Output {
    let file = example_23();
    run(&[
        "generate",
        "local-days",
        file.to_str().unwrap(),
        "publish_delivery_day",
        "--actor",
        "cal",
        "--arg",
        "calendar=c",
        "--zone",
        zone,
        "--zoneinfo",
        zoneinfo,
        "--from",
        from,
        "--until",
        until,
    ])
}

/// Samoa skipped 30 December 2011 by moving across the date line, so
/// the date has no instants: refused whole, never a zero-length day.
#[test]
fn a_date_the_zone_skips_is_refused_not_emitted_empty() {
    let zoneinfo = zoneinfo();
    refused(
        &local_days_in("Pacific/Apia", &zoneinfo, "2011-12-29", "2011-12-31"),
        "2011-12-30 does not exist in Pacific/Apia",
    );
    let before = rows(&local_days_in(
        "Pacific/Apia",
        &zoneinfo,
        "2011-12-29",
        "2011-12-29",
    ));
    let after = rows(&local_days_in(
        "Pacific/Apia",
        &zoneinfo,
        "2011-12-31",
        "2011-12-31",
    ));
    assert_eq!(named(&before[0], "starts_at"), "2011-12-29T10:00:00Z");
    assert_eq!(named(&before[0], "ends_at"), "2011-12-30T10:00:00Z");
    assert_eq!(named(&after[0], "starts_at"), "2011-12-30T10:00:00Z");
    assert_eq!(named(&after[0], "ends_at"), "2011-12-31T10:00:00Z");
}

/// A TZif file with one transition, built here so the case needs no
/// real zone: at 23:30 the clock moves forward an hour to 00:30, so
/// midnight of the next date falls inside the gap.
fn tzif_with_a_gap_across_midnight() -> Vec<u8> {
    let transition_at: i64 = 946_683_000; // 1999-12-31T23:30:00Z
    let header = |counts: [u32; 6]| {
        let mut h = b"TZif2".to_vec();
        h.extend([0u8; 15]);
        for c in counts {
            h.extend(c.to_be_bytes());
        }
        h
    };
    let types = |mut block: Vec<u8>| {
        block.push(1); // the transition moves to type 1
        for (utoff, abbrind) in [(0i32, 0u8), (3600, 4)] {
            block.extend(utoff.to_be_bytes());
            block.push(0);
            block.push(abbrind);
        }
        block.extend(b"+00\0+01\0");
        block
    };
    // isutcnt, isstdcnt, leapcnt, timecnt, typecnt, charcnt
    let counts = [0, 0, 0, 1, 2, 8];
    let mut file = header(counts);
    #[expect(
        clippy::cast_possible_truncation,
        reason = "version 1 of the format stores 32 bits, and this instant fits"
    )]
    file.extend(types((transition_at as i32).to_be_bytes().to_vec()));
    file.extend(header(counts));
    file.extend(types(transition_at.to_be_bytes().to_vec()));
    file.extend(b"\n<+01>-1\n");
    file
}

#[test]
fn a_day_whose_midnight_is_skipped_starts_at_the_clock_change() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("tzdata.zi"), "# version test\n").unwrap();
    std::fs::create_dir(dir.path().join("Test")).unwrap();
    std::fs::write(
        dir.path().join("Test/Gap"),
        tzif_with_a_gap_across_midnight(),
    )
    .unwrap();
    let days = rows(&local_days_in(
        "Test/Gap",
        dir.path().to_str().unwrap(),
        "1999-12-31",
        "2000-01-01",
    ));
    let bounds: Vec<(&str, &str)> = days
        .iter()
        .map(|r| (named(r, "starts_at"), named(r, "ends_at")))
        .collect();
    // The new year begins at the change itself, 00:30 local, not an
    // hour after a midnight that never happened.
    assert_eq!(
        bounds,
        [
            ("1999-12-31T00:00:00Z", "1999-12-31T23:30:00Z"),
            ("1999-12-31T23:30:00Z", "2000-01-01T23:00:00Z"),
        ]
    );
    assert_eq!(named(&days[0], "tzdb_version"), "test");
}

#[test]
fn a_fixed_argument_takes_the_json_shape_its_kind_needs() {
    let file = fixture("flagged_days.morph");
    let zoneinfo = zoneinfo();
    let with = |extra: &[&str]| {
        let mut args = vec![
            "generate",
            "local-days",
            file.to_str().unwrap(),
            "publish",
            "--actor",
            "cal",
            "--zone",
            "Europe/London",
            "--zoneinfo",
            &zoneinfo,
            "--from",
            "2026-01-01",
            "--until",
            "2026-01-01",
        ];
        args.extend_from_slice(extra);
        run(&args)
    };
    let day = rows(&with(&[
        "--arg",
        "provisional=true",
        "--arg",
        r#"tags=["a", "b"]"#,
    ]));
    assert_eq!(day[0]["args_named"]["provisional"], serde_json::json!(true));
    assert_eq!(day[0]["args_named"]["tags"], serde_json::json!(["a", "b"]));
    refused(
        &with(&["--arg", "provisional=maybe", "--arg", r#"tags=["a"]"#]),
        "`provisional` is Bool",
    );
    refused(
        &with(&["--arg", "provisional=true", "--arg", "tags=a"]),
        "`tags`",
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

/// `local_days_in` with extra flags after the range.
fn local_days_with(zone: &str, from: &str, until: &str, extra: &[&str]) -> Output {
    let file = example_23();
    let zoneinfo = zoneinfo();
    let mut args = vec![
        "generate",
        "local-days",
        file.to_str().unwrap(),
        "publish_delivery_day",
        "--actor",
        "cal",
        "--arg",
        "calendar=c",
        "--zone",
        zone,
        "--zoneinfo",
        &zoneinfo,
        "--from",
        from,
        "--until",
        until,
    ];
    args.extend(extra);
    run(&args)
}

fn bounds(rows: &[Value]) -> Vec<(String, String)> {
    rows.iter()
        .map(|r| {
            (
                named(r, "starts_at").to_string(),
                named(r, "ends_at").to_string(),
            )
        })
        .collect()
}

/// A GB gas day runs from 05:00 to 05:00 London time, so it is 23 hours
/// on the spring change and 25 on the autumn one, and starts at 04:00 UTC
/// all summer, not at 05:00 UTC.
#[test]
fn a_day_starting_at_a_local_hour_follows_the_clock_changes() {
    let gas_days = |from, until| {
        bounds(&rows(&local_days_with(
            "Europe/London",
            from,
            until,
            &["--at", "05:00"],
        )))
    };
    let pairs = |p: &[(&str, &str)]| -> Vec<(String, String)> {
        p.iter()
            .map(|(a, b)| ((*a).to_string(), (*b).to_string()))
            .collect()
    };
    assert_eq!(
        gas_days("2026-03-28", "2026-03-29"),
        pairs(&[
            ("2026-03-28T05:00:00Z", "2026-03-29T04:00:00Z"),
            ("2026-03-29T04:00:00Z", "2026-03-30T04:00:00Z"),
        ])
    );
    assert_eq!(
        gas_days("2026-10-24", "2026-10-25"),
        pairs(&[
            ("2026-10-24T04:00:00Z", "2026-10-25T05:00:00Z"),
            ("2026-10-25T05:00:00Z", "2026-10-26T05:00:00Z"),
        ])
    );
}

/// The GB gas day and the continental one are the same span of time:
/// 05:00 London and 06:00 Amsterdam, on every day since GB moved to
/// 05:00 on 1 October 2015, under the committed zone files.
#[test]
fn london_at_five_and_amsterdam_at_six_bound_the_same_days() {
    let london = rows(&local_days_with(
        "Europe/London",
        "2015-10-01",
        "2035-12-31",
        &["--at", "05:00"],
    ));
    let amsterdam = rows(&local_days_with(
        "Europe/Amsterdam",
        "2015-10-01",
        "2035-12-31",
        &["--at", "06:00"],
    ));
    assert_eq!(london.len(), amsterdam.len());
    assert_eq!(bounds(&london), bounds(&amsterdam));
}

/// Naming midnight is the default, byte for byte.
#[test]
fn midnight_named_is_midnight_by_default() {
    let plain = local_days_with("Europe/London", "2026-03-27", "2026-10-26", &[]);
    let named_midnight = local_days_with(
        "Europe/London",
        "2026-03-27",
        "2026-10-26",
        &["--at", "00:00"],
    );
    assert!(plain.status.success() && named_midnight.status.success());
    assert_eq!(plain.stdout, named_midnight.stdout);
}

/// 01:30 occurs twice in London on 25 October 2026. The day starts at
/// the occurrence named, the first by default, or the run is refused.
#[test]
fn a_repeated_start_time_takes_the_occurrence_named() {
    let start = |extra: &[&str]| {
        let mut flags = vec!["--at", "01:30"];
        flags.extend(extra);
        named(
            &rows(&local_days_with(
                "Europe/London",
                "2026-10-25",
                "2026-10-25",
                &flags,
            ))[0],
            "starts_at",
        )
        .to_string()
    };
    assert_eq!(start(&[]), "2026-10-25T00:30:00Z");
    assert_eq!(start(&["--repeated", "first"]), "2026-10-25T00:30:00Z");
    assert_eq!(start(&["--repeated", "second"]), "2026-10-25T01:30:00Z");
    refused(
        &local_days_with(
            "Europe/London",
            "2026-10-25",
            "2026-10-25",
            &["--at", "01:30", "--repeated", "refuse"],
        ),
        "2026-10-25 01:30:00 occurs twice in Europe/London",
    );
}

/// 01:30 does not exist in London on 29 March 2026. By default the day
/// starts at the change; refusing stops the run with nothing written.
#[test]
fn a_skipped_start_time_starts_at_the_change_or_is_refused() {
    let day = rows(&local_days_with(
        "Europe/London",
        "2026-03-29",
        "2026-03-29",
        &["--at", "01:30"],
    ));
    assert_eq!(named(&day[0], "starts_at"), "2026-03-29T01:00:00Z");
    refused(
        &local_days_with(
            "Europe/London",
            "2026-03-28",
            "2026-03-30",
            &["--at", "01:30", "--skipped", "refuse"],
        ),
        "2026-03-29 01:30:00 does not exist in Europe/London",
    );
}

/// A transformation that declares `tzdb_digest` gets the sha256 of the
/// zone file's bytes, so the rules that produced the instants are pinned,
/// not only the release the directory declares. One that does not
/// declare it is unchanged (example 23 above).
#[test]
fn a_declared_digest_pins_the_zone_file_read() {
    use sha2::{Digest as _, Sha256};
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("digest.morph");
    std::fs::write(
        &file,
        "program digested
predicate GasDay(day: Date, starts_at: Timestamp, ends_at: Timestamp, tzdb_version: Subject, tzdb_digest: Subject)
transformation publish(local_date, starts_at, ends_at, tzdb_version, tzdb_digest):
    admit GasDay(local_date, starts_at, ends_at, tzdb_version, tzdb_digest)
",
    )
    .unwrap();
    let zoneinfo = zoneinfo();
    let digest_of = |zone: &str| {
        let out = run(&[
            "generate",
            "local-days",
            file.to_str().unwrap(),
            "publish",
            "--actor",
            "cal",
            "--zone",
            zone,
            "--zoneinfo",
            &zoneinfo,
            "--from",
            "2026-01-01",
            "--until",
            "2026-01-01",
            "--at",
            "05:00",
        ]);
        named(&rows(&out)[0], "tzdb_digest").to_string()
    };
    let expected = |zone: &str| {
        let bytes = std::fs::read(fixture("zoneinfo").join(zone)).unwrap();
        let hex: String = Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        format!("sha256:{hex}")
    };
    assert_eq!(digest_of("Europe/London"), expected("Europe/London"));
    assert_eq!(digest_of("Europe/Amsterdam"), expected("Europe/Amsterdam"));
    assert_ne!(digest_of("Europe/London"), digest_of("Europe/Amsterdam"));
}

/// The generated client's `local_days` and `business_days` return the
/// rows the binary prints, unparsed and unchanged, across a clock change
/// and with fixed values of every codec shape. The rows commit through
/// the client's own `transact`, and a refusal raises with the binary's
/// reason.
#[tokio::test(flavor = "current_thread")]
async fn the_generated_client_returns_the_rows_the_binary_prints() {
    reset_db().await;
    let out = tempfile::tempdir().unwrap();
    let file = example_23();
    let file = file.to_str().unwrap();
    let generated = run(&[
        "generate",
        "python-client",
        file,
        "--out",
        out.path().to_str().unwrap(),
    ]);
    assert!(generated.status.success(), "{generated:?}");
    propose(
        file,
        "appoint_calendar_publisher",
        "ops",
        r#"{"calendar":"uk","publisher":"cal_uk"}"#,
    );
    let probe = r#"
import json, sys
out, file, url, binary, zoneinfo, business, holidays, flagged = sys.argv[1:9]
sys.path.insert(0, out)
from morpholog_client import Morpholog, MorphologError
client = Morpholog(file, url, binary=binary)
days = client.local_days(
    "publish_delivery_day", "cal_uk", first="2026-03-28", until="2026-03-30",
    zone="Europe/London", zoneinfo=zoneinfo, fixed={"calendar": "uk"},
)
print(json.dumps(days))
outcome = client.transact(days)
print(json.dumps([type(outcome).__name__, len(outcome.acts)]))
print(json.dumps(Morpholog(business, url, binary=binary).business_days(
    "admit_business_day", "ops", first="2026-12-21", until="2026-12-31",
    holidays=holidays, fixed={"calendar": "uk"},
)))
print(json.dumps(Morpholog(flagged, url, binary=binary).local_days(
    "publish", "cal", first="2026-01-01", until="2026-01-01",
    zone="Europe/London", zoneinfo=zoneinfo,
    fixed={"provisional": True, "tags": ["a", "b"]},
)))
try:
    client.local_days(
        "publish_delivery_day", "cal_uk", first="2026-03-28", until="2026-03-30",
        zone="Nowhere/Atlantis", zoneinfo=zoneinfo, fixed={"calendar": "uk"},
    )
    print(json.dumps("WRONG no error"))
except MorphologError as exc:
    print(json.dumps(str(exc)))
"#;
    let output = Command::new("python3")
        .args([
            "-c",
            probe,
            out.path().to_str().unwrap(),
            file,
            &database_url(),
            bin(),
            &zoneinfo(),
            fixture("business_days.morph").to_str().unwrap(),
            fixture("uk_holidays_2026.json").to_str().unwrap(),
            fixture("flagged_days.morph").to_str().unwrap(),
        ])
        .output()
        .expect("python3 runs the emitted package");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(
        lines.len(),
        5,
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let days = rows(&local_days("uk", "2026-03-28", "2026-03-30"));
    assert_eq!(lines[0], Value::Array(days));
    assert_eq!(lines[1], serde_json::json!(["AtomicCommitted", 3]));
    assert_eq!(
        lines[2],
        Value::Array(rows(&business_days("2026-12-21", "2026-12-31")))
    );
    assert_eq!(lines[3][0]["args_named"]["provisional"], true);
    assert_eq!(
        lines[3][0]["args_named"]["tags"],
        serde_json::json!(["a", "b"])
    );
    let refusal = lines[4].as_str().unwrap();
    assert!(refusal.contains("Nowhere/Atlantis"), "{refusal}");
}
