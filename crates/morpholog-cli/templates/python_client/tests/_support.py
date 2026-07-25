"""Shared scaffolding for the template test suites: the sys.path
shim onto the templates dir, the golden dir both languages pin, and
the argv-recording context the adapter tests share."""

import contextlib
import json
import os
import sys
import tempfile
from pathlib import Path

TEMPLATES_DIR = Path(__file__).resolve().parents[2]
GOLDEN_DIR = Path(__file__).resolve().parents[3] / "tests" / "golden" / "envelopes"


def add_client_to_path() -> None:
    sys.path.insert(0, str(TEMPLATES_DIR))


def golden(name: str):
    return json.loads((GOLDEN_DIR / name).read_text())


@contextlib.contextmanager
def recording_argv(case):
    """Point the stub's STUB_ARGV_FILE at a temp file and yield an
    `argv_after(call)` that runs `call` and returns the recorded argv
    lines. `case` supplies addCleanup for the env var."""
    with tempfile.NamedTemporaryFile(mode="r", suffix=".argv") as record:
        os.environ["STUB_ARGV_FILE"] = record.name
        case.addCleanup(os.environ.pop, "STUB_ARGV_FILE", None)

        def argv_after(call):
            call()
            with open(record.name) as handle:
                return handle.read().splitlines()

        yield argv_after
