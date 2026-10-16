#!/usr/bin/env python3
"""The explorer: a program outside Morpholog that changes the representation
of a problem and asks the record to govern the result.

It enumerates the residues modulo 4, proposes them and their squares,
asks the record to verify two finite statements, runs Lean on the lemma
that carries them to all integers, records that check under the
checker's name, deduces, and proposes a false statement (squares modulo
8) to read the refusal. Then the historical part: a checkpoint and a
receipt over what stands, a withdrawn check, a second checkpoint, and
the first receipt verified against both histories.

One login plays every party here, which the tests keep apart. It talks
to the binary through the client the binary generates for this
programme, written to a temporary directory first. Needs `morpholog` and
`lean` on the path and an initialised, empty, disposable database:

    DATABASE_URL=postgres:///morpholog_dev python3 examples/22_residue_classes/explore.py
"""

import getpass
import hashlib
import os
import shutil
import subprocess
import sys
import tempfile
from decimal import Decimal
from pathlib import Path

HERE = Path(__file__).resolve().parent
MORPH = HERE / "residue_classes.morph"
LEMMA = HERE / "reduction.lean"

EXPLORER = "explorer"
OPERATOR = "operator"
CHECKER = "lean4"
LEMMA_NAME = "reduction_respects_arithmetic"
SQUARES = "squares_are_zero_or_one"
ODD_LEGS = "no_square_is_a_sum_of_two_odd_squares"


def login_role() -> str:
    for key in ("PGUSER", "USER", "LOGNAME"):
        if os.environ.get(key):
            return os.environ[key]
    return getpass.getuser()


def lean_version(lean: str) -> str:
    """`Lean (version 4.35.0, ...)` as the checker's version label."""
    banner = subprocess.run([lean, "--version"], capture_output=True, text=True, check=True).stdout
    version = banner.split("version", 1)[1].split(",", 1)[0].strip()
    return f"lean4:v{version}"


def main() -> int:
    database_url = os.environ.get("DATABASE_URL")
    if not database_url:
        print("DATABASE_URL must name a disposable, initialised database", file=sys.stderr)
        return 2
    binary = os.environ.get("MORPHOLOG_BIN", "morpholog")
    lean = shutil.which("lean")
    if lean is None:
        print(
            "lean is not on the path: nothing can check reduction.lean, so no checker "
            "could record it and every deduction would be refused. Install Lean 4 "
            "(elan) and run again.",
            file=sys.stderr,
        )
        return 2

    workdir = tempfile.mkdtemp(prefix="residue_classes_client_")
    subprocess.run(
        [binary, "generate", "python-client", str(MORPH), "--out", workdir],
        check=True,
        capture_output=True,
    )
    sys.path.insert(0, workdir)
    from morpholog_client import envelopes, models, open_client
    from morpholog_client.adapter import Request

    morph = open_client(str(MORPH), database_url, binary=binary)

    def act(request: Request, actor: str) -> envelopes.Committed | envelopes.Rejected:
        return morph.propose(request.TRANSFORMATION, actor, request.to_args_named())

    def must(request: Request, actor: str) -> envelopes.Committed:
        outcome = act(request, actor)
        if not isinstance(outcome, envelopes.Committed):
            print(f"refused: {outcome.reason}", file=sys.stderr)
            raise SystemExit(1)
        return outcome

    print("1. the parties: the operator appoints itself and the checker, each bound to a login")
    login = login_role()
    must(models.AppointOperatorRequest(operator=OPERATOR, login_role=login), OPERATOR)
    must(models.AppointCheckerRequest(checker=CHECKER, login_role=login), OPERATOR)

    print("2. the representation: the residues modulo 4 and the table of their squares")
    must(models.DeclareDomainRequest(modulus=Decimal(4), residues=[Decimal(r) for r in range(4)]), EXPLORER)
    for r in range(4):
        must(models.TabulateSquareRequest(modulus=Decimal(4), r=Decimal(r), square=Decimal(r * r % 4)), EXPLORER)

    print("3. verified by the record, exhaustively over the four residues")
    for statement in (SQUARES, ODD_LEGS):
        must(models.VerifyRequest(statement=statement, modulus=Decimal(4)), EXPLORER)
        print(f"   Verified({statement}, 4)")

    print("4. checked outside: Lean on the lemma that carries the table to every integer")
    subprocess.run([lean, str(LEMMA)], check=True, capture_output=True)
    version = lean_version(lean)
    digest = "sha256:" + hashlib.sha256(LEMMA.read_bytes()).hexdigest()
    must(models.RecordCheckRequest(statement=LEMMA_NAME, version=version, artefact=digest), CHECKER)
    print(f"   Checked({LEMMA_NAME}, {CHECKER}, {version}, {digest[:19]}...)")

    print("5. deduced: two conclusions about all integers, each on its two grounds")
    must(models.DeduceRequest(conclusion="every_square_is_zero_or_one_mod_four", statement=SQUARES,
                              modulus=Decimal(4), lemma=LEMMA_NAME), EXPLORER)
    must(models.DeduceRequest(conclusion="no_pythagorean_triple_has_two_odd_legs", statement=ODD_LEGS,
                              modulus=Decimal(4), lemma=LEMMA_NAME), EXPLORER)
    print(f"   Grounded: {len(morph.derived('Grounded'))}")

    print("6. a false statement: squares modulo 8 are not all 0 or 1")
    must(models.DeclareDomainRequest(modulus=Decimal(8), residues=[Decimal(r) for r in range(8)]), EXPLORER)
    for r in range(8):
        must(models.TabulateSquareRequest(modulus=Decimal(8), r=Decimal(r), square=Decimal(r * r % 8)), EXPLORER)
    refusal = act(models.VerifyRequest(statement=SQUARES, modulus=Decimal(8)), EXPLORER)
    assert isinstance(refusal, envelopes.Rejected), refusal
    witness = ", ".join(f"{b.var} = {b.value}" for b in refusal.witness)
    print(f"   refused by {refusal.rule} ({witness})")

    print("7. a receipt over what stands, verified offline against its own history")
    first = morph.audit_checkpoint()
    pack_one = os.path.join(workdir, "pack1.ndjson")
    receipt_one = os.path.join(workdir, "receipt1.json")
    morph.audit_export(pack_one)
    receipt = morph.audit_receipt("Grounded", pack_one, receipt_one)
    report = morph.audit_verify_receipt(receipt_one, pack_one)
    print(f"   checkpoint {first.checkpoint.tree_size}: {len(receipt.answer)} grounded, passes = {report.passes}")

    print("8. the checker withdraws its check; the conclusions leave the read and nothing is erased")
    must(models.WithdrawCheckRequest(statement=LEMMA_NAME, reason="proof_regressed"), CHECKER)
    second = morph.audit_checkpoint()
    pack_two = os.path.join(workdir, "pack2.ndjson")
    receipt_two = os.path.join(workdir, "receipt2.json")
    morph.audit_export(pack_two)
    later = morph.audit_receipt("Grounded", pack_two, receipt_two)
    print(f"   checkpoint {second.checkpoint.tree_size}: {len(later.answer)} grounded, "
          f"Checked rows still on the record: {len(morph.claims('Checked'))}")
    stale = morph.audit_verify_receipt(receipt_one, pack_two)
    checkpoint = "differs" if isinstance(stale.checkpoint, envelopes.CheckpointDiffers) else stale.checkpoint
    print(f"   the first receipt against the later history: passes = {stale.passes}, checkpoint = {checkpoint}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
