#!/usr/bin/env bash
# The generated client's typed seam, checked with a pinned mypy over the
# committed example package: the one-shot and session clients themselves,
# and a probe of what an embedder writes against them (a generated
# request model is accepted, anything else refused). Modules they import
# are analysed for their types but their own errors are not reported yet.
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root/examples/etrm_embedder"

MYPY_VERSION=1.19.1
if python3 -m mypy --version 2>/dev/null | grep -q "^mypy $MYPY_VERSION "; then
    mypy=(python3 -m mypy)
elif command -v uvx >/dev/null 2>&1; then
    mypy=(uvx "mypy@$MYPY_VERSION")
else
    echo "mypy $MYPY_VERSION is needed: pip install mypy==$MYPY_VERSION, or install uv" >&2
    exit 1
fi
"${mypy[@]}" --python-version 3.10 --follow-imports=silent --warn-unused-ignores \
    --cache-dir "$root/target/mypy-cache" \
    morpholog_client/adapter.py morpholog_client/session.py type_contract.py
