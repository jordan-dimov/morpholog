"""The explorer: a program outside Morpholog that changes the representation
of a problem and asks the record to govern the result.

It enumerates the residues modulo 4 and proposes them with their squares,
asks the record to verify two finite statements, runs Lean on the lemma
that carries them to every integer and records that check under the
checker's name, deduces, and proposes a false statement (squares modulo
8) to read the refusal. Then the historical part: a checkpoint and a
receipt over what stands, a withdrawn check, a second checkpoint, and
the first receipt verified against both histories.

One login plays every party here; the tests keep them apart. The script
speaks to the binary through the client the binary generates for this
programme, so generate that first and put it on the path:

    morpholog generate python-client examples/22_residue_classes/residue_classes.morph --out /tmp/rc
    DATABASE_URL=postgres:///morpholog_dev PYTHONPATH=/tmp/rc python3 examples/22_residue_classes/explore.py

Needs `morpholog` and `lean` on the path and an initialised, empty,
disposable database.
"""

from __future__ import annotations

import getpass
import hashlib
import os
import shutil
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from decimal import Decimal
from pathlib import Path

from morpholog_client import Morpholog, Request, envelopes, models, open_client

HERE = Path(__file__).resolve().parent
PROGRAMME = HERE / "residue_classes.morph"
LEMMA_FILE = HERE / "reduction.lean"

EXPLORER = "explorer"
OPERATOR = "operator"
CHECKER = "lean4"
LEMMA = "reduction_respects_arithmetic"
SQUARES = "squares_are_zero_or_one"
ODD_LEGS = "no_square_is_a_sum_of_two_odd_squares"


class Refused(Exception):
    """An act the record refused where the story needed it to commit."""

    def __init__(self, request: Request, outcome: envelopes.Rejected) -> None:
        super().__init__(f"{request.TRANSFORMATION}: {outcome.reason}")


@dataclass(frozen=True)
class LemmaCheck:
    """What the checker records: its version and the file it checked."""

    version: str
    artefact: str


def login_role() -> str:
    """The login this process connects as, resolved the way the binary does."""
    for key in ("PGUSER", "USER", "LOGNAME"):
        if os.environ.get(key):
            return os.environ[key]
    return getpass.getuser()


def check_lemma() -> LemmaCheck | None:
    """Run Lean on the lemma file; None when Lean is not installed."""
    lean = shutil.which("lean")
    if lean is None:
        return None
    subprocess.run([lean, str(LEMMA_FILE)], check=True, capture_output=True)
    banner = subprocess.run(
        [lean, "--version"], check=True, capture_output=True, text=True
    )
    version = banner.stdout.split("version", 1)[1].split(",", 1)[0].strip()
    digest = hashlib.sha256(LEMMA_FILE.read_bytes()).hexdigest()
    return LemmaCheck(version=f"lean4:v{version}", artefact=f"sha256:{digest}")


def propose(
    morph: Morpholog, actor: str, request: Request
) -> envelopes.Committed | envelopes.Rejected:
    return morph.propose(request.TRANSFORMATION, actor, request.to_args_named())


def commit(morph: Morpholog, actor: str, request: Request) -> None:
    outcome = propose(morph, actor, request)
    if isinstance(outcome, envelopes.Rejected):
        raise Refused(request, outcome)


def tabulate(morph: Morpholog, modulus: int) -> None:
    """Declare the residues modulo `modulus` and the table of their squares."""
    residues = [Decimal(r) for r in range(modulus)]
    commit(
        morph,
        EXPLORER,
        models.DeclareDomainRequest(modulus=Decimal(modulus), residues=residues),
    )
    for r in range(modulus):
        row = models.TabulateSquareRequest(
            modulus=Decimal(modulus), r=Decimal(r), square=Decimal(r * r % modulus)
        )
        commit(morph, EXPLORER, row)


def deduce(morph: Morpholog, conclusion: str, statement: str) -> None:
    request = models.DeduceRequest(
        conclusion=conclusion, statement=statement, modulus=Decimal(4), lemma=LEMMA
    )
    commit(morph, EXPLORER, request)


def grounded(morph: Morpholog) -> int:
    return len(morph.derived("Grounded"))


@dataclass(frozen=True)
class Receipt:
    """A checkpoint, the pack covering it, and a receipt over `Grounded`."""

    tree_size: int
    pack: Path
    receipt: Path
    stated: int

    @staticmethod
    def issue(morph: Morpholog, workdir: Path, name: str) -> Receipt:
        checkpoint = morph.audit_checkpoint().checkpoint
        pack = workdir / f"{name}.ndjson"
        receipt = workdir / f"{name}.receipt.json"
        morph.audit_export(str(pack))
        issued = morph.audit_receipt("Grounded", str(pack), str(receipt))
        return Receipt(checkpoint.tree_size, pack, receipt, len(issued.answer))


def verify(morph: Morpholog, receipt: Receipt, against: Receipt) -> str:
    """Verify one receipt against another checkpoint's pack, in a few words."""
    report = morph.audit_verify_receipt(str(receipt.receipt), str(against.pack))
    if isinstance(report.checkpoint, envelopes.CheckpointDiffers):
        return f"passes = {report.passes}, checkpoint = differs"
    return f"passes = {report.passes}, checkpoint = {report.checkpoint}"


def explore(morph: Morpholog, lemma: LemmaCheck, workdir: Path) -> None:
    print(
        "1. the parties: the operator appoints itself and the checker, bound to logins"
    )
    login = login_role()
    commit(
        morph,
        OPERATOR,
        models.AppointOperatorRequest(operator=OPERATOR, login_role=login),
    )
    commit(
        morph, OPERATOR, models.AppointCheckerRequest(checker=CHECKER, login_role=login)
    )

    print("2. the representation: the residues modulo 4 and the table of their squares")
    tabulate(morph, 4)

    print("3. verified by the record, exhaustively over the four residues")
    for statement in (SQUARES, ODD_LEGS):
        commit(
            morph,
            EXPLORER,
            models.VerifyRequest(statement=statement, modulus=Decimal(4)),
        )
        print(f"   Verified({statement}, 4)")

    print(
        "4. checked outside: Lean on the lemma that carries the table to every integer"
    )
    check = models.RecordCheckRequest(
        statement=LEMMA, version=lemma.version, artefact=lemma.artefact
    )
    commit(morph, CHECKER, check)
    print(f"   Checked({LEMMA}, {CHECKER}, {lemma.version}, {lemma.artefact[:19]}...)")

    print("5. deduced: two conclusions about all integers, each on its two grounds")
    deduce(morph, "every_square_is_zero_or_one_mod_four", SQUARES)
    deduce(morph, "no_pythagorean_triple_has_two_odd_legs", ODD_LEGS)
    print(f"   Grounded: {grounded(morph)}")

    print("6. a false statement: squares modulo 8 are not all 0 or 1")
    tabulate(morph, 8)
    refusal = propose(
        morph, EXPLORER, models.VerifyRequest(statement=SQUARES, modulus=Decimal(8))
    )
    if isinstance(refusal, envelopes.Rejected):
        witness = ", ".join(
            f"{binding.var} = {binding.value}" for binding in refusal.witness
        )
        print(f"   refused by {refusal.rule} ({witness})")

    print("7. a receipt over what stands, verified offline against its own history")
    first = Receipt.issue(morph, workdir, "first")
    print(
        f"   checkpoint {first.tree_size}: {first.stated} grounded, "
        f"{verify(morph, first, first)}"
    )

    print("8. the checker withdraws its check: the read empties and nothing is erased")
    commit(
        morph,
        CHECKER,
        models.WithdrawCheckRequest(statement=LEMMA, reason="proof_regressed"),
    )
    second = Receipt.issue(morph, workdir, "second")
    checks = len(morph.claims("Checked"))
    print(
        f"   checkpoint {second.tree_size}: {second.stated} grounded, "
        f"Checked rows still on the record: {checks}"
    )
    print(
        f"   the first receipt against the later history: "
        f"{verify(morph, first, second)}"
    )


def main() -> int:
    database_url = os.environ.get("DATABASE_URL")
    if not database_url:
        print(
            "DATABASE_URL must name a disposable, initialised database", file=sys.stderr
        )
        return 2
    lemma = check_lemma()
    if lemma is None:
        print(
            "lean is not on the path, so nothing can check reduction.lean; install Lean 4",
            file=sys.stderr,
        )
        return 2
    morph = open_client(str(PROGRAMME), database_url)
    workdir = Path(tempfile.mkdtemp(prefix="residue_classes_"))
    try:
        explore(morph, lemma, workdir)
    except Refused as refused:
        print(f"refused: {refused}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
