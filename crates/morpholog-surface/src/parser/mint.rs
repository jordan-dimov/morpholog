//! The naming pass for an inline `new Subject()`. An `admit` or `emit` argument may mint a
//! subject where it is written down; the statement parser leaves the [`MINT`] placeholder in
//! the slot, and this pass turns each into `let <name> = new Subject()` before the statement,
//! in slot order, with a name no other variable of the transformation has. The IR, the kernel,
//! the draw record and replay see the two-line form, so a programme written the short way
//! hashes as its long form does.

use std::collections::BTreeSet;

use morpholog_core::{Stmt, Term, Var};

use super::expr::MINT;
use super::walk::{names_in_stmt, vars_in_stmt};

/// Name every placeholder in `body`, a transformation's top-level statements with their
/// spans. A `let` inserted before a statement carries that statement's span. A generated name
/// is one the transformation has nowhere: not a parameter, not a name a `let` or `for`
/// introduces, not a binder, not a variable it reads.
pub(super) fn name_the_mints<S: Clone>(
    parameters: &[String],
    body: Vec<(Stmt, S)>,
) -> Vec<(Stmt, S)> {
    if !body.iter().any(|(stmt, _)| holds_a_mint(stmt)) {
        return body;
    }
    let mut taken: BTreeSet<String> = parameters.iter().cloned().collect();
    for (stmt, _) in &body {
        names_in_stmt(stmt, &mut taken);
    }
    let mut names = Names { taken, next: 1 };
    let mut out = Vec::with_capacity(body.len());
    for (stmt, span) in body {
        for named in name_in(stmt, &mut names) {
            out.push((named, span.clone()));
        }
    }
    out
}

/// Generated names, `_subject_1` onwards, skipping any the transformation already uses.
struct Names {
    taken: BTreeSet<String>,
    next: usize,
}

impl Names {
    fn fresh(&mut self) -> Var {
        loop {
            let candidate = format!("_subject_{}", self.next);
            self.next += 1;
            if self.taken.insert(candidate.clone()) {
                return Var::from(candidate);
            }
        }
    }
}

fn holds_a_mint(stmt: &Stmt) -> bool {
    let mut vars = BTreeSet::new();
    vars_in_stmt(stmt, &mut vars);
    vars.contains(MINT)
}

/// One statement, with its placeholders named: the lets it needs, then itself.
fn name_in(stmt: Stmt, names: &mut Names) -> Vec<Stmt> {
    let is_mint = |term: &Term| matches!(term, Term::Var(v) if v.as_str() == MINT);
    let mut name_args = |args: &mut Vec<Term>, lets: &mut Vec<Stmt>| {
        for arg in args.iter_mut().filter(|arg| is_mint(arg)) {
            let name = names.fresh();
            lets.push(Stmt::LetNewSubject { name: name.clone() });
            *arg = Term::Var(name);
        }
    };
    let mut lets = Vec::new();
    let stmt = match stmt {
        Stmt::Assert(mut claim) => {
            name_args(&mut claim.args, &mut lets);
            Stmt::Assert(claim)
        }
        Stmt::Emit(mut intent) => {
            name_args(&mut intent.args, &mut lets);
            Stmt::Emit(intent)
        }
        Stmt::For {
            binding,
            collection,
            body,
        } => Stmt::For {
            binding,
            collection,
            body: body
                .into_iter()
                .flat_map(|inner| name_in(inner, names))
                .collect(),
        },
        other => other,
    };
    lets.push(stmt);
    lets
}
