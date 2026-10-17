# Residue classes: what the record verified, and what it was told

Two textbook exercises: the square of any integer leaves remainder 0 or
1 when divided by 4, and a Pythagorean triple cannot have both legs odd.
Both are about infinitely many integers. Both become finite once you
look at remainders modulo 4 instead of integers: four residues, four
squares, one table. Changing the representation is what makes the
problem easy, and the change itself needs a justification: squaring and
adding have to respect the reduction.

This example governs that change with the language as it is. No kernel
primitive, no new construct. What it adds to the gallery is a record
that keeps three kinds of standing apart and never says more than it
can back:

| Standing | Claim | Who establishes it |
|---|---|---|
| Verified over a finite domain | `Verified(statement, modulus)` | The record. The statements are its own rules, checked exhaustively over a domain whose completeness it has confirmed, against a square table it recomputes entry by entry. |
| Checked outside | `Checked(statement, checker, version, artefact)` | A checker, under a name bound to its own database login. The theorem about every integer is proved whole by Lean, and the record keeps who said so, under which version, about which exact file. It is testimony with a name on it, not a proof the record holds, and the record never re-runs the checker. |
| Standing | `Conclusion(conclusion, statement)` | The programme and the two above. The rules name the two conclusions this programme recognises and the finite statement each rests on; a conclusion is admitted only when its core is verified here and a checker has checked it. A conclusion the programme does not name is refused whatever a checker says of it. |

The read `Standing` lists the conclusions whose check still stands. A
checker that withdraws a check removes that conclusion from the read and
erases nothing: the check, the withdrawal and the conclusion all stay on
the record.

## The programme at a glance

Claims: the domain (`Residue`, `DomainSealed`), the square table
(`SquareOf`), the three standings above, the parties (`Operator`,
`Checker`, and the runtime's two login-binding claims) and `Withdrawn`.
Everything the standings rest on is append only.

| Rule | What it refuses |
|---|---|
| `residue_is_an_integer_in_range`, `sealed_domain_is_complete` | A domain that is not the whole set of residues: a fraction, a value out of range, a residue left out. |
| `square_is_the_recompute` | A square the record does not recompute. The act's own gate refuses a row for a residue the domain does not declare. |
| `verified_domain_is_sealed`, `verified_squares_are_total` | A statement verified over an unsealed domain, or a table with a row missing. |
| `squares_are_zero_or_one` | The first exercise, over the table. Propose it for modulus 8 and the row for 2, whose square is 4, refuses it by name. |
| `no_square_is_a_sum_of_two_odd_squares` | The second exercise, over the table: every pair of odd residues against every square. |
| `only_a_stated_statement_is_verified` | Verifying a statement the rules have no check for. |
| `conclusion_rests_on_its_own_finite_core` | A conclusion the programme does not name, or one paired with the wrong finite statement, even with a checker's report on record for it. |
| `conclusion_rests_on_a_verified_core`, `conclusion_rests_on_a_check` | A conclusion with a ground missing. |

Acts: `appoint_operator` and `appoint_checker` (the operator only),
`declare_domain`, `tabulate_square`, `verify` and `conclude` (anyone;
the rules decide), `record_check` and `withdraw_check` (a checker, under
its own name).

## The theorems, and who checked them

`reduction.lean` proves both theorems over all integers, from Lean 4's
core library alone: `every_square_is_zero_or_one_mod_four` and
`no_pythagorean_triple_has_two_odd_legs`. Each proof reduces modulo 4
(three small lemmas in the same file say that reduction respects
squaring, adding and parity) and then splits into the same four cases
the record checks for itself. The theorem names are the statements the
checker records. Lean is not a dependency of this repository or its CI;
anyone can re-check the file:

```bash
lean examples/22_residue_classes/reduction.lean
```

What the record holds is the checker's report: the name `lean4`, the
version it ran, the file's digest and the theorem's name in it. What the
record verified for itself is the finite core: the two statements over
the residues modulo 4. The pairing of each theorem with its core is a
rule of the programme, not a deduction anyone performs: the record
attributes the theorem, verifies the core, and admits no conclusion it
has not named.

## Run it

```bash
morpholog check -v examples/22_residue_classes/residue_classes.morph
cargo test -p morpholog-examples --test residue_classes
DATABASE_URL=postgres:///morpholog_dev cargo test -p morpholog-cli --test residue_classes_e2e
```

The in-memory tests walk both exercises, the modulo-8 counterexample
and every refusal above. The CLI suite is the historical verification:

1. The explorer declares the domain, tabulates the squares, verifies
   both statements; the checker records both theorems; the explorer
   admits both conclusions.
2. A checkpoint is recorded and a complete-prefix pack exported.
   `audit receipt --derived Standing` states both conclusions over that
   history, and `audit verify-receipt` reproduces the statement offline.
3. The checker withdraws one check. A second checkpoint and pack: the
   same read now lists one conclusion, and the first receipt, verified
   against the later pack, reports `checkpoint: differs`. The receipt
   was a true statement about the earlier history, and still is; what
   stands now is a different question.
4. A login the checker's name is not bound to tries to record a check,
   and is refused before the body runs.

`explore.py` is the explorer: a Python script outside Morpholog that
drives the same story through the client the binary generates. It
enumerates the residues, proposes the table, runs `lean` on the theorem
file and records each check under the checker's name, admits both
conclusions, proposes the modulo-8 counterexample and reads the refusal,
then issues and verifies the receipts. Generate the client first and put it on the
path; the script needs `lean` too, and a disposable database. One login
plays every party.

```bash
morpholog generate python-client examples/22_residue_classes/residue_classes.morph --out /tmp/rc
DATABASE_URL=postgres:///morpholog_dev PYTHONPATH=/tmp/rc python3 examples/22_residue_classes/explore.py
```

## Searching for representations

Nothing above is specific to residues. The shape is: an untrusted
explorer proposes a representation of a problem and a finite table
computed in it; the record recomputes the table, confirms the domain is
complete, and verifies the finite statements exhaustively; the claim
about the original problem is attributed to a named checker; the
programme names which conclusions it recognises and the finite core each
rests on; a conclusion stands only on all three.

The same shape takes an automated search. A search over candidate
representations of an optimisation problem (a partition of the input
space into classes, a reduced scenario set, a compressed set of trades
with the same net exposures, a relaxation with a certificate of its
bound) proposes each candidate as claims. The record refuses every
candidate whose table does not recompute or whose domain is not
complete, and the search is free to be as wrong as it likes on the way.
What survives is not "the search said so" but a finite statement the
record verified, paired by rule with a result somebody with a name checked.
`evaluate` already scores candidate rules against history the same way,
and the receipts give each surviving representation a statement anyone
can recompute later. The search itself, like the proof assistant, stays
outside.

## Deliberately not covered

Checking a proof inside Morpholog, or any inference from one claim to
another: the pairing of a theorem with its core is written in the rules,
and nothing here derives one statement from others. Proving that the
checker actually ran: the record attributes a report to a login and
pins what it was about, nothing more. Infinite domains, which the rules
cannot range over. A general vocabulary of statements: each finite
statement and each conclusion here is named in a rule of this
programme. A conclusion the runtime writes itself: the explorer states
it and the record refuses a wrong one, which is the thesis of this
project rather than a shortcoming.
