#!/usr/bin/env bash
# Phase A.1 — verify Perry can emit a usable .dylib/.so for a TS handler.
#
# Pass criterion: the resulting shared library exists and `nm` shows at
# least one symbol that looks like our exported handler. We don't yet care
# whether the symbol name matches a specific naming convention — we'll lock
# the ABI in Phase B once we know what Perry actually emits.

set -euo pipefail

PERRY="${PERRY:-/Users/amlug/projects/perry/perry/target/release/perry}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SRC="${SCRIPT_DIR}/hello.ts"
OUT_DIR="${SCRIPT_DIR}/build"
mkdir -p "${OUT_DIR}"

if [[ "$(uname -s)" == "Darwin" ]]; then
  EXT=".dylib"
else
  EXT=".so"
fi
OUT="${OUT_DIR}/hello${EXT}"

echo "==> perry version"
"${PERRY}" --version

echo "==> compiling ${SRC} to ${OUT}"
"${PERRY}" compile --output-type dylib --keep-intermediates -o "${OUT}" "${SRC}"

echo "==> checking output exists"
if [[ ! -f "${OUT}" ]]; then
  echo "FAIL: ${OUT} not produced"
  exit 1
fi

echo "==> file ${OUT}"
file "${OUT}"

echo "==> nm symbols (filtered)"
nm -gU "${OUT}" 2>/dev/null || nm -D "${OUT}" 2>/dev/null || nm "${OUT}"

echo
echo "==> Phase A.1 PASS — dylib produced. Inspect symbol list above to choose"
echo "    the entry point name for Phase A.2."
