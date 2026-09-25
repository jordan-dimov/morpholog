//! Generators and observers for differential tests: two subsystems answer the same question
//! over many generated cases and must agree (scoped vs full state, traced vs untraced).
//!
//! Values derive from an integer salt, never a clock or an RNG, so a failure replays exactly
//! from its printed case.

use morpholog_core::{
    ClaimInstance, EvalError, EvalValue, Outcome, ParamKind, PredicateArgKind, Program, State,
    Transformation, TransformationName, ValidatedProgram, transformation_param_kinds,
};

use crate::{bool_, coll, date, dec, dur, qty, subj, ts};

/// A deterministic value of the given kind. `None` only for a calendar span, which no
/// declaration can carry.
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

/// A state with `witnesses` claims per declared predicate, plus one claim of an undeclared
/// predicate: noise a correct load scope must be free to drop.
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

/// Deterministic arguments for a transformation, from its inferred parameter kinds.
/// `None` if the programme does not validate or a kind does not resolve. Callers should bound
/// their skips so a broken generator fails loudly.
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
        // No single lawful kind, so skip rather than guess.
        ParamKind::Ambiguous(_) => None,
        ParamKind::Collection(element) => Some(EvalValue::Collection(vec![
            sample_param(element, salt.wrapping_mul(5))?,
            sample_param(element, salt.wrapping_mul(5).wrapping_add(1))?,
        ])),
    }
}

/// The comparable text of a proposal result, with fresh subjects renamed by
/// [`normalize_uuids`].
///
/// `candidate_state` is left out: it lawfully differs between a full and a scoped run.
/// Rejections render with `{:?}`, not `Display`, because the display string omits the witness;
/// comparing it would pass a run that rejects the same rule on a different witness.
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

/// Replace each distinct UUID in the text with `<fresh-N>`, numbered in order of first
/// appearance.
///
/// `new Subject()` mints a fresh UUID on every run, so two lawful runs differ exactly there.
/// No other fixture value looks like a UUID.
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

// ============================================================
// Boundary argument cases
// ============================================================

/// Every shared-subject witness uses this name, so two parameters naming the same subject is
/// among the cases tried.
const SHARED_SUBJECT: &str = "shared";

/// The largest exact decimal, to push arithmetic past the representable range.
const DECIMAL_MAX: &str = "79228162514264337593543950335";

/// One generated proposal. `permits_range_refusal` is set only when the vector carries a
/// range-extreme value; on every other vector, any kernel error is a failure.
pub struct ArgumentCase {
    pub args: Vec<EvalValue>,
    pub permits_range_refusal: bool,
}

/// Whether a kernel error is an out-of-range refusal, which is expected (not a bug) on an
/// `ArgumentCase` with `permits_range_refusal`.
pub fn is_permitted_range_error(e: &EvalError) -> bool {
    matches!(
        e,
        EvalError::ArithOutOfRange(_) | EvalError::RoundOutOfRange { .. }
    )
}

/// Boundary argument vectors for one transformation: a baseline, then the baseline with one
/// parameter at a time set to a boundary value (zero, negative and maximum numbers, `false`,
/// empty collections, a shared subject, the calendar's ends). A probe, not a proof.
pub fn boundary_argument_cases(
    validated: &ValidatedProgram<'_>,
    name: &TransformationName,
) -> Vec<ArgumentCase> {
    let kinds: Vec<(String, ParamKind)> = transformation_param_kinds(validated, name)
        .expect("parameter kinds resolve for a validated programme's transformation")
        .into_iter()
        .map(|(v, k)| (v.to_string(), k))
        .collect();
    let base: Vec<EvalValue> = kinds.iter().map(|(n, k)| baseline(k, n)).collect();
    let mut vectors = vec![ArgumentCase {
        args: base.clone(),
        permits_range_refusal: false,
    }];
    for (i, (name, kind)) in kinds.iter().enumerate() {
        for witness in boundary_witnesses(kind, name) {
            let mut varied = base.clone();
            varied[i] = witness.value;
            vectors.push(ArgumentCase {
                args: varied,
                permits_range_refusal: witness.extreme,
            });
        }
    }
    vectors
}

/// The baseline argument for one parameter. Subjects are named after the parameter so values
/// join across transformations.
fn baseline(kind: &ParamKind, name: &str) -> EvalValue {
    match kind {
        ParamKind::Concrete(k) => baseline_concrete(k, name),
        ParamKind::Collection(inner) => crate::coll(vec![baseline(inner, name)]),
        ParamKind::Polymorphic | ParamKind::Unconstrained => crate::subj(name),
        ParamKind::Ambiguous(kinds) => kinds
            .first()
            .map(|k| baseline_concrete(k, name))
            .unwrap_or_else(|| crate::subj(name)),
    }
}

fn baseline_concrete(kind: &PredicateArgKind, name: &str) -> EvalValue {
    match kind {
        PredicateArgKind::Subject | PredicateArgKind::Any => crate::subj(name),
        PredicateArgKind::Decimal => crate::dec(1),
        PredicateArgKind::Date => crate::date("2026-07-01"),
        PredicateArgKind::Timestamp => crate::ts("2026-07-01T12:00:00Z"),
        PredicateArgKind::Duration => crate::dur("PT1H"),
        PredicateArgKind::Bool => crate::bool_(true),
        PredicateArgKind::Quantity(unit) => crate::qty("1", unit.as_str()),
        PredicateArgKind::Collection => crate::coll(vec![crate::subj(name)]),
        // No parameter can have this kind; the stand-in keeps the match total.
        PredicateArgKind::CalendarSpan => crate::subj(name),
    }
}

/// One boundary value. `extreme` marks a value for which out-of-range refusals are expected.
struct Witness {
    value: EvalValue,
    extreme: bool,
}

fn ordinary(value: EvalValue) -> Witness {
    Witness {
        value,
        extreme: false,
    }
}

/// The boundary values for one parameter. Zero and negatives probe division and range checks,
/// the shared subject probes equality joins, the empty collection probes loops over nothing,
/// and the maximum decimal probes overflow.
fn boundary_witnesses(kind: &ParamKind, name: &str) -> Vec<Witness> {
    match kind {
        ParamKind::Concrete(k) => boundary_concrete(k, name),
        ParamKind::Collection(_) => vec![ordinary(crate::coll(vec![]))],
        ParamKind::Polymorphic | ParamKind::Unconstrained => {
            vec![ordinary(crate::subj(SHARED_SUBJECT))]
        }
        ParamKind::Ambiguous(kinds) => kinds
            .first()
            .map(|k| boundary_concrete(k, name))
            .unwrap_or_default(),
    }
}

fn boundary_concrete(kind: &PredicateArgKind, _name: &str) -> Vec<Witness> {
    match kind {
        PredicateArgKind::Subject | PredicateArgKind::Any => {
            vec![ordinary(crate::subj(SHARED_SUBJECT))]
        }
        PredicateArgKind::Decimal => vec![
            ordinary(crate::dec(0)),
            ordinary(crate::dec(-1)),
            Witness {
                value: crate::dec_str(DECIMAL_MAX),
                extreme: true,
            },
        ],
        PredicateArgKind::Quantity(unit) => {
            vec![
                ordinary(crate::qty("0", unit.as_str())),
                ordinary(crate::qty("-1", unit.as_str())),
                // The baseline at another scale: equal in the kernel.
                ordinary(crate::qty("1.0", unit.as_str())),
                Witness {
                    value: crate::qty(DECIMAL_MAX, unit.as_str()),
                    extreme: true,
                },
            ]
        }
        PredicateArgKind::Bool => vec![ordinary(crate::bool_(false))],
        PredicateArgKind::Collection => vec![ordinary(crate::coll(vec![]))],
        // Dates at either end of the calendar, so date arithmetic can overflow.
        PredicateArgKind::Date => vec![
            Witness {
                value: crate::date("-009999-01-01"),
                extreme: true,
            },
            Witness {
                value: crate::date("9999-12-31"),
                extreme: true,
            },
        ],
        // A second value lets two parameters of this kind disagree; the baseline's
        // nanosecond neighbours order only at the kernel's precision; the calendar's
        // ends and year zero probe the text forms the codec trims and signs.
        PredicateArgKind::Timestamp => vec![
            ordinary(crate::ts("2026-07-02T09:30:00Z")),
            ordinary(crate::ts("2026-07-01T12:00:00.000000001Z")),
            ordinary(crate::ts("2026-07-01T11:59:59.999999999Z")),
            ordinary(crate::ts("0000-01-01T00:00:00Z")),
            Witness {
                value: crate::ts("-009999-01-02T01:59:59Z"),
                extreme: true,
            },
            Witness {
                value: crate::ts("9999-12-30T22:00:00.999999999Z"),
                extreme: true,
            },
        ],
        PredicateArgKind::Duration => vec![ordinary(crate::dur("PT2H30M"))],
        // No parameter can have this kind.
        PredicateArgKind::CalendarSpan => vec![],
    }
}
