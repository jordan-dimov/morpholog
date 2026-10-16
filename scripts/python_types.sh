#!/usr/bin/env bash
# The generated client, checked whole with a pinned mypy: the committed
# example package and the worked embedder with its type-contract probe
# (a generated request model is accepted, anything else refused), and a
# package generated from a programme with every value kind the generator
# supports. Every module is named, so none is checked only by import.
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

MYPY_VERSION=1.19.1
if python3 -m mypy --version 2>/dev/null | grep -q "^mypy $MYPY_VERSION "; then
    mypy=(python3 -m mypy)
elif command -v uvx >/dev/null 2>&1; then
    mypy=(uvx "mypy@$MYPY_VERSION")
else
    echo "mypy $MYPY_VERSION is needed: pip install mypy==$MYPY_VERSION, or install uv" >&2
    exit 1
fi
check=("${mypy[@]}" --python-version 3.12 --warn-unused-ignores --cache-dir "$root/target/mypy-cache")

every_kind="$root/target/python-types/every_kind"
rm -rf "$every_kind"
cargo run -q -p morpholog-cli -- generate python-client \
    "$root/crates/morpholog-cli/tests/fixtures/every_generated_kind.morph" \
    --out "$every_kind" >/dev/null

(cd "$root/examples/etrm_embedder" && "${check[@]}" \
    morpholog_client/*.py etrm_lifecycle.py type_contract.py)
(cd "$every_kind" && "${check[@]}" morpholog_client/*.py)

residue="$root/target/python-types/residue_classes"
rm -rf "$residue"
cargo run -q -p morpholog-cli -- generate python-client \
    "$root/examples/22_residue_classes/residue_classes.morph" \
    --out "$residue" >/dev/null
(cd "$root/examples/22_residue_classes" && MYPYPATH="$residue" "${check[@]}" explore.py)
