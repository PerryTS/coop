#!/usr/bin/env bash
# Phase A.2 — verify a Rust binary can act as a Perry plugin host.
#
# Builds the host-rust crate (which depends on perry-runtime as a path
# dependency), then runs it against the .dylib produced by run-a1.sh.
#
# Pass criterion: the host prints "hello from perry plugin" to stdout.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PLUGIN_DIR="${SCRIPT_DIR}/build"
HOST_DIR="${SCRIPT_DIR}/host-rust"

if [[ "$(uname -s)" == "Darwin" ]]; then
  EXT=".dylib"
else
  EXT=".so"
fi
PLUGIN="${PLUGIN_DIR}/hello${EXT}"

if [[ ! -f "${PLUGIN}" ]]; then
  echo "==> plugin not built yet, running run-a1.sh first"
  "${SCRIPT_DIR}/run-a1.sh"
fi

echo "==> building Rust host (this links perry-runtime)"
(cd "${HOST_DIR}" && cargo build --release)

HOST_BIN="${HOST_DIR}/target/release/coop-derisk-host"

echo "==> running ${HOST_BIN} ${PLUGIN}"
"${HOST_BIN}" "${PLUGIN}"
