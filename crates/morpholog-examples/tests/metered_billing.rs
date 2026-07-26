//! Integration tests for the metered billing example
//! (`examples/15_metered_billing/`).
//!
//! The `round(x, quantum)` forcing example: the payable figure on
//! every charge line IS the recomputed rate-times-volume rounded to
//! the nearest penny, exact halves away from zero. These tests pin
//! the teaching points - the 1p tamper refusal, the away-from-zero
//! half boundary, the VAT totality companion closing the vacuity
//! hole, and the per-line-then-sum convention refusing the rival
//! round-the-aggregate figure.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use std::sync::OnceLock;

use common::{Example, claim_instance, dec_str, subj, test_actor};
use morpholog_core::{EvalError, Outcome, RejectionReason, State};
use morpholog_examples::metered_billing;

fn ex() -> &'static Example {
    static EX: OnceLock<Example> = OnceLock::new();
    EX.get_or_init(|| Example::new(&metered_billing::program()))
}

/// The GB reduced rate, declared as law before any billing.
fn with_vat_rate() -> State {
    State::from_claims(vec![claim_instance(
        "VatRate",
        &[subj("vat_reduced"), dec_str("0.05")],
    )])
}

/// Propose one fully-computed charge line.
fn add_line(
    line: &str,
    rate_p: &str,
    volume: &str,
    net: &str,
    vat_rate: &str,
    vat: &str,
    state: &State,
) -> Result<Outcome, EvalError> {
    ex().propose_as(
        &metered_billing::add_charge_line(),
        vec![
            subj(line),
            subj("inv_1"),
            dec_str(rate_p),
            dec_str(volume),
            dec_str(net),
            subj(vat_rate),
            dec_str(vat),
        ],
        test_actor(),
        state,
    )
}

fn rejected_by(outcome: &Result<Outcome, EvalError>, rule: &str) -> bool {
    matches!(
        outcome,
        Ok(Outcome::Rejected {
            reason: RejectionReason::Invariant { name, .. },
            ..
        }) if name.as_str() == rule
    )
}

#[test]
fn an_exactly_rounded_line_is_admitted() {
    // 13.5 p/kWh * 431.7 kWh = 58.2795 GBP -> 58.28; VAT at 5% of
    // 58.28 = 2.914 -> 2.91.
    let outcome = add_line(
        "line_a",
        "13.5",
        "431.7",
        "58.28",
        "vat_reduced",
        "2.91",
        &with_vat_rate(),
    )
    .unwrap();
    assert!(
        matches!(outcome, Outcome::Accepted { .. }),
        "the engine's exact figures must be admitted: {outcome:?}"
    );
}

#[test]
fn a_one_penny_tamper_is_refused_by_name() {
    let outcome = add_line(
        "line_a",
        "13.5",
        "431.7",
        "58.29",
        "vat_reduced",
        "2.91",
        &with_vat_rate(),
    );
    assert!(
        rejected_by(&outcome, "line_net_is_the_rounded_recompute"),
        "one penny of drift must be refused: {outcome:?}"
    );
}

#[test]
fn an_exact_half_rounds_away_from_zero() {
    // 12.5 p/kWh * 1 kWh = 0.125 GBP, dead between 12p and 13p: the
    // convention says 0.13. VAT: 0.13 * 0.05 = 0.0065 -> 0.01.
    let up = add_line(
        "line_h",
        "12.5",
        "1",
        "0.13",
        "vat_reduced",
        "0.01",
        &with_vat_rate(),
    )
    .unwrap();
    assert!(
        matches!(up, Outcome::Accepted { .. }),
        "0.125 must round to 0.13: {up:?}"
    );
    // The round-half-DOWN figure is not this ledger's convention.
    let down = add_line(
        "line_h",
        "12.5",
        "1",
        "0.12",
        "vat_reduced",
        "0.01",
        &with_vat_rate(),
    );
    assert!(
        rejected_by(&down, "line_net_is_the_rounded_recompute"),
        "0.12 is a different convention and must be refused: {down:?}"
    );
}

#[test]
fn a_line_naming_an_undeclared_rate_is_refused_by_the_companion() {
    // Arithmetic all correct - but the named rate was never declared,
    // so the VAT recompute rule would pass EMPTILY. The totality
    // companion is what actually refuses it.
    let outcome = add_line(
        "line_a",
        "13.5",
        "431.7",
        "58.28",
        "vat_missing",
        "2.91",
        &with_vat_rate(),
    );
    assert!(
        rejected_by(&outcome, "every_line_names_a_declared_rate"),
        "an undeclared rate must be refused by the companion: {outcome:?}"
    );
}

/// Two lines of 0.4 pence each: the per-line and round-the-aggregate
/// conventions genuinely diverge on this invoice, and the ledger
/// enforces per-line.
fn two_tiny_lines() -> State {
    let mut claims = with_vat_rate().claims().to_vec();
    claims.push(claim_instance(
        "ChargeLine",
        &[
            subj("line_t1"),
            subj("inv_1"),
            dec_str("0.4"),
            dec_str("1"),
            dec_str("0"),
            subj("vat_reduced"),
            dec_str("0"),
        ],
    ));
    claims.push(claim_instance(
        "ChargeLine",
        &[
            subj("line_t2"),
            subj("inv_1"),
            dec_str("0.4"),
            dec_str("1"),
            dec_str("0"),
            subj("vat_reduced"),
            dec_str("0"),
        ],
    ));
    State::from_claims(claims)
}

fn seal(total: &str, state: &State) -> Result<Outcome, EvalError> {
    ex().propose_as(
        &metered_billing::seal_invoice(),
        vec![subj("inv_1"), dec_str(total)],
        test_actor(),
        state,
    )
}

#[test]
fn the_per_line_convention_total_seals() {
    // 0.004 GBP rounds to 0.00 per line; the invoice total is 0.00.
    let outcome = seal("0", &two_tiny_lines()).unwrap();
    assert!(
        matches!(outcome, Outcome::Accepted { .. }),
        "the per-line total must seal: {outcome:?}"
    );
}

#[test]
fn the_aggregate_convention_total_is_refused() {
    // Summing the raw 0.004s and rounding once gives 0.01 - a real
    // convention, but not this ledger's contract.
    let outcome = seal("0.01", &two_tiny_lines());
    assert!(
        rejected_by(&outcome, "sealed_total_is_the_sum_of_its_lines"),
        "the aggregate-rounded total must be refused: {outcome:?}"
    );
}

#[test]
fn a_negative_tariff_or_volume_is_refused_not_an_accidental_credit() {
    // -13.5 p/kWh * 431.7 kWh recomputes and rounds consistently to
    // -58.28, so the recompute rule alone would admit it - a credit
    // note by accident. The range invariant is what closes the
    // boundary the prose declares out of scope.
    let outcome = add_line(
        "line_neg",
        "-13.5",
        "431.7",
        "-58.28",
        "vat_reduced",
        "-2.91",
        &with_vat_rate(),
    );
    assert!(
        rejected_by(&outcome, "charge_inputs_are_non_negative"),
        "a negative tariff must be refused by the range rule: {outcome:?}"
    );
}

#[test]
fn a_vat_rate_outside_the_unit_interval_is_refused() {
    let outcome = ex().propose_as(
        &metered_billing::declare_vat_rate(),
        vec![subj("vat_wild"), dec_str("1.2")],
        test_actor(),
        &State::default(),
    );
    assert!(
        rejected_by(&outcome, "vat_rate_is_a_fraction"),
        "a rate above 1 must be refused: {outcome:?}"
    );
}

#[test]
fn a_sealed_invoice_takes_no_further_lines() {
    let mut claims = two_tiny_lines().claims().to_vec();
    claims.push(claim_instance(
        "InvoiceSealed",
        &[subj("inv_1"), dec_str("0")],
    ));
    let state = State::from_claims(claims);
    let outcome = add_line(
        "line_late",
        "13.5",
        "431.7",
        "58.28",
        "vat_reduced",
        "2.91",
        &state,
    );
    assert!(
        matches!(outcome, Ok(Outcome::Rejected { .. })),
        "a sealed invoice is closed to new lines: {outcome:?}"
    );
}

/// A refusal names the offending values, not only the rule.
///
/// The operator's question after "line_net_is_the_rounded_recompute
/// violated" is "which figures?" - so the witness carries them: the
/// engine's tampered net alongside the tariff and volume it should have
/// been computed from. Enough to see 13.5 * 431.7 / 100 rounds to 58.28,
/// not the submitted 58.29, without opening the database.
///
/// The line id is deliberately absent: this rule matches
/// `ChargeLine(_, _, ...)`, and a witness can only name what the rule
/// binds. Wildcard the subject and the refusal cannot tell you which row
/// it was - a real cost of writing the rule that way, and the reason
/// `borrowing_base` carries the companion test for a rule that does bind
/// its subject.
#[test]
fn a_refusal_names_the_offending_figures() {
    let outcome = add_line(
        "line_a",
        "13.5",
        "431.7",
        "58.29",
        "vat_reduced",
        "2.91",
        &with_vat_rate(),
    );
    let Ok(Outcome::Rejected {
        reason: RejectionReason::Invariant { witness, .. },
    }) = &outcome
    else {
        panic!("expected an invariant refusal, got {outcome:?}");
    };
    let named: Vec<(&str, String)> = witness
        .iter()
        .map(|w| (w.var.as_str(), format!("{:?}", w.value)))
        .collect();
    assert_eq!(
        named,
        vec![
            ("net_gbp", "Decimal(58.29)".to_string()),
            ("rate_p_per_kwh", "Decimal(13.5)".to_string()),
            ("volume_kwh", "Decimal(431.7)".to_string()),
        ],
        "sorted by variable, so the same refusal always reads the same way"
    );
}
