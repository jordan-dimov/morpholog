# Decision record: a two-sort IR (`Prop` and `ValueExpr`)

Status: proposed. Pre-implementation. Once shipped this folds into
[`design-history.md`](design-history.md) and is removed.

## Decision

Split `Expr` into two sorts. Make the predicate-vs-value boundary a *type*,
not a runtime error plus a static checker.

This sits **under the constitution** in
[`scope-and-ambition.md`](scope-and-ambition.md): `Prop` and `ValueExpr` are
*supporting machinery* - the body grammar of invariants and transformations -
never a user-facing product concept. The split does not drift from "only
invariants and transformations are first-class"; it *restores* that
discipline by giving their bodies an honest, unambiguous shape instead of an
overloaded `Expr` blob that lets anything sit anywhere.

## Doctrine (the why)

Morpholog has two internal expression sorts:

- **Propositions** (`Prop`) **search governed state and produce binding
  witnesses** - zero, one, or many satisfying binding contexts. A
  proposition is *relational, not boolean*.
- **Value expressions** (`ValueExpr`) **compute exactly one value from a
  binding context** (or a structural error).

Today one `Expr` does both jobs - its own doc says a node yields "a
truth-witness (predicate position) or a value (value position)." That
conflation forces a *dynamic repair* for a *static* mistake: `find_matches`
rejects `Term`/`Add`/`Sub`/`Sum`/`ValueOf` as `NotPredicate`; `eval_value`
mirrors it with `NotValue`; and `check.rs`/`validate.rs` re-police the same
boundary statically (`ExpectedValueExpression`, `ExpectedPredicateExpression`,
`short_expr_shape`). Two sorts delete all of that by construction.

## The partition

```
Prop      = Claim | And | Or | Not | Implies | Exists | Forall | Pre
          | Eq | Neq | Compare | In
ValueExpr = Term | Add | Sub | Sum | ValueOf
```

The sorts are mutually recursive, and the cross-references are the whole
point - they encode "a comparison relates two values", "a sum ranges over a
proposition":

- `Prop::{Eq, Neq, Compare}` operands are `ValueExpr`.
- `Prop::In(Term, Term)` stays term-level (membership).
- `Prop::{Not, Pre}` wrap a `Prop`; `And`/`Or` hold `Vec<Prop>`;
  `Implies`/`Exists`/`Forall` compose `Prop`s (`Forall.source` and `.body`
  are both `Prop`).
- `ValueExpr::Sum { value: Term, body: Box<Prop> }` - the comprehension
  ranges over a proposition.
- `ValueExpr::ValueOf { predicate, args: Vec<Term>, default: Option<Box<ValueExpr>> }`.

`Pre` stays `Prop::Pre(Box<Prop>)` - we do **not** open `pre(value_of(...))`
without an example forcing it.

## What binds variables (unchanged semantics, restated)

`Prop::Claim` (arg unification), `Prop::In` (generates its element when
unbound), `Prop::{Exists, Forall}` (their `binding`), and
`ValueExpr::Sum` (its comprehension variable, scoped to `body`). The
statement-level quartet (`require`/`bind`/`let`/`for`) export rules are
unchanged - that is `Stmt`, not touched here.

## Evaluator signatures

```rust
fn find_matches(prop: &Prop, ctx: &EvalContext) -> Result<Vec<Bindings>, EvalError>
fn eval_value(expr: &ValueExpr, ctx: &EvalContext) -> Result<EvalValue, EvalError>
```

Both become **total** over their sort - no wrong-shape arm.

## What deletes

- `EvalError::NotPredicate`, `EvalError::NotValue`.
- The two wrong-shape fallthrough arms in `find_matches` / `eval_value`.
- The static shape mirror in `check.rs`/`validate.rs`:
  `ExpectedValueExpression`, `ExpectedPredicateExpression`, and
  `short_expr_shape`'s shape role.

## What stays (this is not a kind-checker rewrite)

Kind/type checking still matters and still lives in `check.rs`, now phrased
over the two sorts: comparator/arithmetic operand kinds (Decimal vs Date),
equality kind-strictness, variable kind refinement. Also unchanged:
`UnboundVariable`, `TypeMismatch`, `ValueOfZeroMatches`,
`ValueOfMultipleMatches`, `UnboundActor`, `PreStateUnavailable`,
`NestingTooDeep`.

## Parser emission

The parser already knows its position (an invariant/`require` body is a
proposition; a `let` value, a comparator operand, a `sum` target are
values), so each production emits the right sort directly. `expr_as_term`
(the term-only downconvert for `In` and claim args) stays.

**The `parse_expression` boundary (resolved).** `parse_expression -> Prop`.
That keeps the public/default parser entry programme-facing: invariant and
`require` bodies are propositions, and a standalone `a + b` is not a valid
Morpholog body - value expressions only appear nested. A narrower
`parse_value_expr -> ValueExpr` serves the arithmetic-parsing tests and any
parser-internal value-position production.

We do **not** introduce a `ParsedExpr { Prop, Value }` union: that would
preserve the one-expression ambiguity the split exists to destroy, at the
boundary under a new name, and push the "which sort was this?" decision onto
every caller. A later rename of `parse_expression` to `parse_prop` is
possible once the split has landed; deferred.

## `ir_builder` changes

The builders split by return sort: `claim`/`and`/`or`/`not`/`implies`/
`exists`/`forall`/`pre`/`eq`/`neq`/`le`/`date_le`/`in_` return `Prop`;
`term`/`add`/`sub`/`sum`/`value_of` return `ValueExpr`; `var`/`dec`/`subj`/
`wildcard`/`actor` return `Term`. Adversarial tests pick the right builder
by what they are constructing. Mechanical.

## Staged migration (mechanically faithful first)

1. Define `Prop` and `ValueExpr` in `ir.rs` (clean swap, no transitional
   `Expr`).
2. `eval.rs`: `find_matches(&Prop)`, `eval_value(&ValueExpr)`; delete the
   wrong-shape arms and `NotPredicate`/`NotValue`.
3. Parser: split productions, emit the two sorts; resolve the
   `parse_expression` boundary per the decision above.
4. `format`, `check`, `validate`, `analysis`, `derive`, `explain`,
   `ir_builder`: split the walkers across the two sorts.
5. Update tests.
6. **Only after green**, delete the now-dead shape-checking in
   `check.rs`/`validate.rs`.

No semantic additions in this PR. No new operators.

## Explicit non-goals (this PR)

- **Do not split `Term`** (`PatternTerm`/`ValueTerm`). `Wildcard` valid in
  match positions but not as a resolved value stays handled as today - a
  separate later cleanup, not this tarpit.
- No `Decl` unification, no `Term::Actor` rework - those are tidy-ups; this
  is the architectural unlock and goes alone.

## The principle

First make the ontology true. Then subtract everything that only existed
because the ontology was false.
