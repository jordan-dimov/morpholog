//! Evaluating a condition that calls itself.
//!
//! A question is a call with its known arguments filled in, and each
//! question has a table of answers. A body evaluation that meets the
//! call to itself reads that call's table instead of entering the body
//! again, so the stack never grows with the data. A question is
//! evaluated again whenever a table it read has grown, until none grows.
//!
//! A question that read a table nobody has filled yet waits under the
//! questions it read, so a chain is followed to its end before answers
//! flow back, and each question on it is evaluated about twice.
//!
//! Nothing here iterates a hashed collection: questions are evaluated,
//! and dependents re-queued, in the order the matcher met them.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use super::{EvalContext, EvalError, find_matches};
use crate::ir::{Definition, DefinitionName, Prop};
use crate::state::{Bindings, Bound, cmp_value};

/// One value per parameter: known when the call arrived with it bound.
type Known = Vec<Option<Bound>>;

struct Question {
    known: Known,
    answers: Vec<Vec<Bound>>,
    seen: HashSet<Vec<Bound>>,
    /// The questions that read this table, in the order they first did.
    dependents: Vec<usize>,
    evaluated: bool,
    /// A table it read has grown since it was last evaluated.
    stale: bool,
    /// The number of its latest entry on the stack; older entries are
    /// skipped. Zero when it is not waiting.
    waiting: u64,
}

#[derive(Default)]
struct Inner {
    ids: HashMap<Known, usize>,
    questions: Vec<Question>,
    reads: HashSet<(usize, usize)>,
    /// The question whose body is being evaluated.
    current: usize,
    /// Questions the current evaluation read before they were ever
    /// evaluated, in the order it read them.
    unready: Vec<usize>,
    unready_seen: HashSet<usize>,
    /// Questions waiting to be evaluated, the next one last.
    pending: Vec<(usize, u64)>,
    pushes: u64,
}

impl Inner {
    fn ask(&mut self, known: Known) -> usize {
        if let Some(id) = self.ids.get(&known) {
            return *id;
        }
        let id = self.questions.len();
        self.ids.insert(known.clone(), id);
        self.questions.push(Question {
            known,
            answers: Vec::new(),
            seen: HashSet::new(),
            dependents: Vec::new(),
            evaluated: false,
            stale: false,
            waiting: 0,
        });
        id
    }

    /// Put a question on top of the stack, moving it there if it waits
    /// lower down.
    fn push(&mut self, id: usize) {
        self.pushes += 1;
        self.questions[id].waiting = self.pushes;
        self.pending.push((id, self.pushes));
    }

    fn pop(&mut self) -> Option<usize> {
        while let Some((id, entry)) = self.pending.pop() {
            if self.questions[id].waiting == entry {
                self.questions[id].waiting = 0;
                return Some(id);
            }
        }
        None
    }
}

/// The tables of one outer call to one recursive condition.
pub(crate) struct Tables {
    definition: DefinitionName,
    inner: RefCell<Inner>,
}

fn known(def: &Definition, frame: &Bindings) -> Known {
    def.parameters
        .iter()
        .map(|p| frame.get(p).cloned())
        .collect()
}

fn as_bindings(def: &Definition, answer: &[Bound]) -> Bindings {
    def.parameters
        .iter()
        .cloned()
        .zip(answer.iter().cloned())
        .collect()
}

impl Tables {
    pub(crate) fn answers(&self, name: &DefinitionName) -> bool {
        &self.definition == name
    }

    /// What the table of the call with `frame` holds now. The question
    /// being evaluated is recorded as its reader, and a call met for the
    /// first time is queued.
    pub(crate) fn read(&self, def: &Definition, frame: &Bindings) -> Vec<Bindings> {
        let mut inner = self.inner.borrow_mut();
        let id = inner.ask(known(def, frame));
        let reader = inner.current;
        if inner.reads.insert((id, reader)) {
            inner.questions[id].dependents.push(reader);
        }
        if !inner.questions[id].evaluated && inner.unready_seen.insert(id) {
            inner.unready.push(id);
        }
        inner.questions[id]
            .answers
            .iter()
            .map(|answer| as_bindings(def, answer))
            .collect()
    }
}

/// Every answer to the call of `def` with `frame`, as bindings of all its
/// parameters, in the canonical value order.
pub(crate) fn solve(
    def: &Definition,
    body: &Prop,
    frame: &Bindings,
    ctx: &EvalContext<'_>,
) -> Result<Vec<Bindings>, EvalError> {
    let tables = Tables {
        definition: def.name.clone(),
        inner: RefCell::new(Inner::default()),
    };
    let root = {
        let mut inner = tables.inner.borrow_mut();
        let root = inner.ask(known(def, frame));
        inner.push(root);
        root
    };
    loop {
        let question_frame: Bindings = {
            let mut inner = tables.inner.borrow_mut();
            let Some(id) = inner.pop() else {
                break;
            };
            // It waited for tables that then gained nothing.
            if inner.questions[id].evaluated && !inner.questions[id].stale {
                continue;
            }
            inner.questions[id].evaluated = true;
            inner.questions[id].stale = false;
            inner.current = id;
            def.parameters
                .iter()
                .zip(&inner.questions[id].known)
                .filter_map(|(p, value)| Some((p.clone(), value.clone()?)))
                .collect()
        };
        let body_ctx = EvalContext {
            tables: Some(&tables),
            ..ctx.enter_definition(&question_frame)
        };
        let matches = find_matches(body, &body_ctx)?;

        let mut inner = tables.inner.borrow_mut();
        let id = inner.current;
        let mut grew = false;
        for m in matches {
            let answer = def
                .parameters
                .iter()
                .map(|p| {
                    m.get(p)
                        .cloned()
                        .ok_or_else(|| EvalError::UnboundVariable(p.to_string()))
                })
                .collect::<Result<Vec<Bound>, EvalError>>()?;
            let question = &mut inner.questions[id];
            if question.seen.insert(answer.clone()) {
                question.answers.push(answer);
                grew = true;
            }
        }
        if grew {
            let dependents = inner.questions[id].dependents.clone();
            for dependent in dependents.into_iter().rev() {
                inner.questions[dependent].stale = true;
                if inner.questions[dependent].waiting == 0 {
                    inner.push(dependent);
                }
            }
        }
        // What it read unfilled goes on top, the first read first, and
        // it waits beneath to read them again.
        let unready = std::mem::take(&mut inner.unready);
        inner.unready_seen.clear();
        if !unready.is_empty() {
            inner.push(id);
            for read in unready.into_iter().rev() {
                inner.push(read);
            }
        }
    }

    let mut inner = tables.inner.borrow_mut();
    let mut answers = std::mem::take(&mut inner.questions[root].answers);
    answers.sort_by(|a, b| {
        a.iter()
            .zip(b)
            .map(|(l, r)| cmp_value(l, r))
            .find(|o| o.is_ne())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(answers
        .iter()
        .map(|answer| as_bindings(def, answer))
        .collect())
}
