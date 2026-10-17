#!/usr/bin/env bash
# The generated Python client under a pinned ruff: the templates, their
# tests and the committed example package pass Morpholog's baseline, the
# default rules at this version, with no suppressions. An embedder's
# stricter configuration is its own. `--isolated` ignores any
# configuration above these paths.
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

RUFF_VERSION=0.14.0
if ruff --version 2>/dev/null | grep -q "^ruff $RUFF_VERSION"; then
    ruff=(ruff)
elif python3 -m ruff --version 2>/dev/null | grep -q "^ruff $RUFF_VERSION"; then
    ruff=(python3 -m ruff)
elif command -v uvx >/dev/null 2>&1; then
    ruff=(uvx "ruff@$RUFF_VERSION")
else
    echo "ruff $RUFF_VERSION is needed: pip install ruff==$RUFF_VERSION, or install uv" >&2
    exit 1
fi
"${ruff[@]}" check --isolated --target-version py312 \
    crates/morpholog-cli/templates/python_client \
    examples/etrm_embedder/morpholog_client \
    examples/22_residue_classes/explore.py
# The emitted models keep to the formatter's width and shape, so an embedder
# who formats the package has nothing to change: the committed example
# package is checked as the formatter would leave it.
"${ruff[@]}" format --check --isolated --line-length 88 \
    examples/etrm_embedder/morpholog_client/models.py
