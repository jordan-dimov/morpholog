//! Deterministic generators and observers for the semantic-differential
//! harnesses: tests that make two subsystems answer the same question
//! over many generated cases and require agreement (scoped loading vs
//! full state, traced vs untraced execution).
//!
//! Everything here is deterministic - values derive from an integer
//! salt, never from a clock or an RNG - so a differential failure
//! replays exactly from its printed case.

use morpholog_core::{
    ClaimInstance, EvalError, EvalValue, Outcome, ParamKind, PredicateArgKind, Program, State,
    Transformation, transformation_param_kinds,
};

use crate::{bool_, coll, date, dec, dur, qty, subj, ts};

/// A deterministic value of the given declared kind. `None` only for
/// kinds no declaration can carry (a calendar span), so a generated
/// state or argument vector is total over real programmes.
pub fn sample_value(kind: &PredicateArgKind, salt: u64) -> Option<EvalValue> {
    Some(match kind {
        PredicateArgKind::Subject => subj(&format!("s{}", salt % 5)),
        PredicateArgKind::Decimal => dec((salt % 7) as i64),
        PredicateArgKind::Date => date(&format!("2026-03-{:02}", 1 + salt % 27)),
        PredicateArgKind::Timestamp => ts(&format!("2026-03-01T{:02}:00:00Z", salt % 24)),
        PredicateArgKind::Duration => dur(&format!("PT{}H", 1 + salt % 40)),
        PredicateArgKind::Bool => bool_(salt.is_multiple_of(2)),
        PredicateArgKind::Collection => coll(vec![
            subj(&format!("c{}", salt % 3)),
            subj(&format!("c{}", (salt + 1) % 3)),
        ]),
        PredicateArgKind::Quantity(unit) => qty(&format!("{}", salt % 9), unit.as_str()),
        PredicateArgKind::Any => subj(&format!("any{}", salt % 4)),
        PredicateArgKind::CalendarSpan => return None,
    })
}

/// A state with `witnesses` deterministic claims per declared
/// predicate, plus one claim of a predicate the programme does not
/// declare - noise a correct load scope must be free to drop.
pub fn sample_state(program: &Program, witnesses: u64, salt: u64) -> State {
    let mut claims = Vec::new();
    for (p_idx, decl) in program.predicates.iter().enumerate() {
        for w in 0..witnesses {
            let args: Option<Vec<EvalValue>> = decl
                .args
                .iter()
                .enumerate()
                .map(|(a_idx, arg)| {
                    sample_value(
                        &arg.kind,
                        salt.wrapping_mul(31)
                            .wrapping_add(p_idx as u64 * 13)
                            .wrapping_add(w * 7)
                            .wrapping_add(a_idx as u64),
                    )
                })
                .collect();
            if let Some(args) = args {
                claims.push(ClaimInstance {
                    predicate: decl.name.clone(),
                    args,
                });
            }
        }
    }
    claims.push(ClaimInstance {
        predicate: "ZZ_DifferentialNoise".into(),
        args: vec![subj("noise")],
    });
    State::from_claims(claims)
}

/// Deterministic arguments for a transformation, derived from its
/// inferred parameter kinds. `None` when the programme does not
/// validate or a parameter's kind resolution fails - callers count
/// and bound their skips so a generator collapse fails loudly.
pub fn sample_args(program: &Program, t: &Transformation, salt: u64) -> Option<Vec<EvalValue>> {
    let validated = program.validated().ok()?;
    let kinds = transformation_param_kinds(&validated, &t.name).ok()?;
    kinds
        .iter()
        .enumerate()
        .map(|(i, (_, kind))| sample_param(kind, salt.wrapping_add(i as u64 * 3)))
        .collect()
}

fn sample_param(kind: &ParamKind, salt: u64) -> Option<EvalValue> {
    match kind {
        ParamKind::Concrete(k) => sample_value(k, salt),
        ParamKind::Polymorphic | ParamKind::Unconstrained => {
            Some(subj(&format!("poly{}", salt % 4)))
        }
        // An ambiguous parameter has no single lawful kind; the case
        // is skipped rather than guessed (callers bound their skips).
        ParamKind::Ambiguous(_) => None,
        ParamKind::Collection(element) => Some(EvalValue::Collection(vec![
            sample_param(element, salt.wrapping_mul(5))?,
            sample_param(element, salt.wrapping_mul(5).wrapping_add(1))?,
        ])),
    }
}

/// The comparable face of a proposal result: outcome variant, the
/// asserted/retracted claims, the emitted intents, and the rejection
/// reason or error text - with `candidate_state` deliberately
/// excluded (it lawfully differs between a full and a projected run)
/// and fresh subjects alpha-normalised (see [`normalize_uuids`]).
///
/// The rejection arm renders the STRUCTURAL reason (`{:?}`), not its
/// `Display`: the pinned wire string deliberately omits the invariant
/// version and the witness bindings, and a differential that compared
/// it would bless a run that rejects the same rule on a DIFFERENT
/// witness - exactly the divergence a dropped predicate produces.
pub fn observable(result: &Result<Outcome, EvalError>) -> String {
    let raw = match result {
        Ok(Outcome::Accepted {
            asserted_claims,
            retracted_claims,
            emitted_intents,
            candidate_state: _,
        }) => format!(
            "accepted asserted={asserted_claims:?} retracted={retracted_claims:?} \
             emitted={emitted_intents:?}"
        ),
        Ok(Outcome::Rejected { reason }) => format!("rejected {reason:?}"),
        Err(e) => format!("error {e}"),
    };
    normalize_uuids(&raw)
}

/// Replace each distinct UUID in the text with `<fresh-N>` in first-
/// occurrence order. `let x = new Subject()` mints a fresh UUIDv7 per
/// execution, so two lawful runs of one proposal differ exactly in
/// those identifiers; no other test fixture value is UUID-shaped, so
/// this normalisation is precise. See the characterisation test for
/// why the raw identifiers must never be compared.
pub fn normalize_uuids(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut seen: Vec<String> = Vec::new();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        if let Some(candidate) = text.get(i..i + 36)
            && is_uuid(candidate)
        {
            let n = match seen.iter().position(|s| s == candidate) {
                Some(n) => n,
                None => {
                    seen.push(candidate.to_string());
                    seen.len() - 1
                }
            };
            out.push_str(&format!("<fresh-{n}>"));
            i += 36;
        } else {
            // Advance one whole character, not one byte.
            let ch = text[i..].chars().next().expect("in-bounds char");
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}
