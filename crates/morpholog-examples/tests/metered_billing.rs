//! Integration tests for the metered billing example
//! (`examples/15_metered_billing/`).
//!
//! Each charge line's payable figure IS rate times volume, rounded to the
//! penny with exact halves away from zero. Covered: a 1p tamper is refused,
//! halves round away from zero, an undeclared VAT rate is refused rather
//! than passing emptily, the invoice total rounds per line, then sums, and
//! a credit note is the billed line negated, computed rather than proposed.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use std::sync::OnceLock;

use common::{Example, claim_instance, dec_str, subj, test_actor};
use morpholog_core::{EvalError, Outcome, RejectionReason, State};
use morpholog_examples::metered_billing;
use morpholog_test_support::eval_invariant;

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
    // The arithmetic is right, but the named rate was never declared, so the
    // VAT recompute rule would pass EMPTILY. The totality rule refuses it.
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
    // -13.5 p/kWh * 431.7 kWh rounds consistently to -58.28, so the recompute
    // rule alone would admit an accidental credit note. The range invariant
    // refuses it.
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

/// One charge line on a sealed invoice: a billed line.
fn billed_line() -> State {
    let mut claims = with_vat_rate().claims().to_vec();
    claims.push(claim_instance(
        "ChargeLine",
        &[
            subj("line_a"),
            subj("inv_1"),
            dec_str("13.5"),
            dec_str("431.7"),
            dec_str("58.28"),
            subj("vat_reduced"),
            dec_str("2.91"),
        ],
    ));
    claims.push(claim_instance(
        "InvoiceSealed",
        &[subj("inv_1"), dec_str("61.19")],
    ));
    State::from_claims(claims)
}

fn credit_note(credit: &str, net: &str, vat: &str) -> morpholog_core::ClaimInstance {
    claim_instance(
        "CreditNote",
        &[subj(credit), subj("line_a"), dec_str(net), dec_str(vat)],
    )
}

/// Propose a credit against a line, naming only the line.
fn credit(line: &str, state: &State) -> Result<Outcome, EvalError> {
    ex().propose_as(
        &metered_billing::credit_line(),
        vec![subj("credit_1"), subj(line)],
        test_actor(),
        state,
    )
}

#[test]
fn a_credit_note_is_the_billed_line_negated_to_the_penny() {
    let outcome = credit("line_a", &billed_line()).unwrap();
    let Outcome::Accepted {
        asserted_claims, ..
    } = &outcome
    else {
        panic!("a billed line can be credited: {outcome:?}");
    };
    assert_eq!(
        asserted_claims,
        &vec![credit_note("credit_1", "-58.28", "-2.91")],
        "the credit's figures are the line's own, sign turned, no re-rounding"
    );
}

#[test]
fn a_line_not_yet_billed_cannot_be_credited() {
    let mut claims = billed_line().claims().to_vec();
    claims.retain(|c| c.predicate.as_str() != "InvoiceSealed");
    let outcome = credit("line_a", &State::from_claims(claims));
    assert!(
        matches!(
            outcome,
            Ok(Outcome::Rejected {
                reason: RejectionReason::Require { .. },
            })
        ),
        "an unsealed invoice has nothing to credit: {outcome:?}"
    );
}

#[test]
fn a_line_is_reversed_at_most_once() {
    let mut claims = billed_line().claims().to_vec();
    claims.push(credit_note("credit_0", "-58.28", "-2.91"));
    let outcome = credit("line_a", &State::from_claims(claims));
    assert!(
        matches!(outcome, Ok(Outcome::Rejected { .. })),
        "a second credit against one line is refused: {outcome:?}"
    );
}

/// A credit written by hand that re-rounds on its own, or forgets the
/// sign, breaks the copy-negation rule. No transformation can state the
/// figures, so the rule is asked directly.
#[test]
fn a_credit_that_recomputes_or_forgets_the_sign_breaks_the_rule() {
    for (net, vat) in [("-58.3", "-2.9"), ("58.28", "2.91")] {
        let mut claims = billed_line().claims().to_vec();
        claims.push(credit_note("credit_x", net, vat));
        let holds = eval_invariant(
            &metered_billing::program(),
            &metered_billing::a_credit_copy_negates_its_line(),
            &State::from_claims(claims),
            None,
        )
        .unwrap();
        assert!(
            !holds,
            "a credit of ({net}, {vat}) against 58.28/2.91 must break the rule"
        );
    }
}

/// A refusal names which line, and by how much.
///
/// The witness carries the line and invoice, and the tampered net beside the
/// tariff and volume it should come from: enough to see 13.5 * 431.7 / 100
/// rounds to 58.28, not the submitted 58.29, without opening the database.
///
/// `line` and `invoice` appear only because the rule names them; the
/// arithmetic uses neither. Figures alone cannot identify a row, since two
/// lines can share them.
#[test]
fn a_refusal_names_which_line_and_by_how_much() {
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
            ("invoice", "Subject(Subject(\"inv_1\"))".to_string()),
            ("line", "Subject(Subject(\"line_a\"))".to_string()),
            ("net_gbp", "Decimal(58.29)".to_string()),
            ("rate_p_per_kwh", "Decimal(13.5)".to_string()),
            ("volume_kwh", "Decimal(431.7)".to_string()),
        ],
        "sorted by variable, so the same refusal always reads the same way"
    );
}
