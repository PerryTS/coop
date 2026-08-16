#!/usr/bin/env bash
# Rebuild the Next.js application fixture through Coop's own compile
# pipeline. The daemon owns compilation, identity, boundary validation, and
# publication, so the artifact this prints is a real immutable package rather
# than a hand-built library that silently rots against the pinned Perry.
#
# Deliberately not folded into prepare-resource-benchmark.sh: that fixture is
# a dependency-free tiny app used by Linux CI, while this one needs the Next
# dependency tree installed under benchmarks/next-small.
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
fixture_root="${COOP_NEXT_FIXTURE_DIR:-$repo_root/target/next-benchmark/coop-run}"
source_root="${COOP_NEXT_SOURCE_DIR:-$repo_root/benchmarks/next-small}"
perry="${COOP_BENCH_PERRY:-$repo_root/.perry-main/target/perry-dev/perry}"
provider_verification="${COOP_BENCH_PROVIDER_VERIFICATION:-full_hash}"
timeout_seconds="${COOP_NEXT_PREPARE_TIMEOUT:-1200}"

case "$(uname -s)" in
  Darwin) extension="dylib" ;;
  Linux) extension="so" ;;
  *) echo "unsupported benchmark host: $(uname -s)" >&2; exit 1 ;;
esac
runtime="${COOP_BENCH_RUNTIME:-$repo_root/var/coop/lib/libperry_runtime.$extension}"
stdlib="${COOP_BENCH_STDLIB:-$repo_root/var/coop/lib/libperry_stdlib.$extension}"

daemon="${COOP_BENCH_DAEMON:-}"
if [[ -z "$daemon" ]]; then
  for candidate in "$repo_root/target/release/coop" "$repo_root/target/debug/coop"; do
    if [[ -x "$candidate" ]]; then
      daemon="$candidate"
      break
    fi
  done
fi

fail() {
  echo "cannot rebuild the Next benchmark fixture: $1" >&2
  echo "  fix: $2" >&2
  exit 1
}

if [[ -z "$daemon" || ! -x "$daemon" ]]; then
  fail "no coop daemon binary" "cargo build --release -p coop-daemon"
fi
if [[ ! -x "$perry" ]]; then
  fail "pinned Perry compiler is missing at $perry" \
    "scripts/sync-perry-main.sh && build the pinned Perry compiler"
fi
for provider in "$runtime" "$stdlib"; do
  if [[ ! -f "$provider" ]]; then
    fail "Perry provider library is missing at $provider" "scripts/build-perry-libraries.sh"
  fi
done
if [[ ! -d "$source_root/node_modules/next" ]]; then
  fail "the Next dependency tree is not installed under $source_root" \
    "(cd $source_root && npm ci)"
fi
for required in \
  "$source_root/coop/coop.toml" \
  "$source_root/coop/coop-handler.ts" \
  "$source_root/app/api/benchmark/route.ts"; do
  if [[ ! -f "$required" ]]; then
    fail "fixture source is missing: $required" "restore it from version control"
  fi
done

deployment="$fixture_root/deployments/next-bench"
compiled="$fixture_root/compiled"
# Stage exactly the compiler inputs the daemon requires. Coop refuses
# symlinked source files, so every module is copied; only the dependency tree
# is linked, which the daemon dereferences into its own private snapshot.
rm -rf "$deployment"
mkdir -p \
  "$deployment/handlers" \
  "$deployment/app/api/benchmark" \
  "$compiled" \
  "$fixture_root/sockets" \
  "$fixture_root/storage" \
  "$fixture_root/logs" \
  "$fixture_root/acme"
cp "$source_root/coop/coop.toml" "$deployment/coop.toml"
# handlers/main.ts keeps the handler one directory below the deployment root,
# so its "../app/api/benchmark/route" import resolves exactly as it does in
# the Next project.
cp "$source_root/coop/coop-handler.ts" "$deployment/handlers/main.ts"
cp "$source_root/app/api/benchmark/route.ts" "$deployment/app/api/benchmark/route.ts"
ln -sfn "$source_root/node_modules" "$deployment/node_modules"

# The pre-2026-08-14 hand-built fixture lived at this mutable path. Coop no
# longer publishes there, and leaving it behind lets a stale library outlive a
# Perry pin bump.
rm -f "$compiled/next-bench.$extension" "$compiled/next-bench.coop-lib.json"

# Discard every previously published package and the activation state that
# points at it, so this run cannot "succeed" by finding an artifact somebody
# else left behind. Perry's object cache is deliberately kept: it makes the
# forced recompile cheap without ever standing in for one. A package whose
# digest no longer verifies also makes the daemon refuse to load the
# deployment at all, which would otherwise wedge the fixture permanently.
rm -rf "$compiled/next-bench" "$compiled/.staging"
rm -f "$fixture_root"/state.sqlite "$fixture_root"/state.sqlite-*

# Two configs on purpose. runtime.toml keeps the documented benchmark port so
# benchmarks/server-benchmark.mjs can serve this fixture unchanged; the
# short-lived build below binds an ephemeral port so preparing the fixture can
# never contend with a running benchmark server.
write_runtime_config() {
  cat > "$1" <<EOF
[http]
listen_http = "$2"

[execution]
mode = "in_process"
provider_verification = "$provider_verification"

[paths]
deployments_dir = "$fixture_root/deployments"
compiled_dir = "$compiled"
sockets_dir = "$fixture_root/sockets"
storage_dir = "$fixture_root/storage"
logs_dir = "$fixture_root/logs"
acme_cache_dir = "$fixture_root/acme"
state_db = "$fixture_root/state.sqlite"
perry_binary = "$perry"
perry_runtime_library = "$runtime"
perry_stdlib_library = "$stdlib"

[tls]
mode = "off"
EOF
}

write_runtime_config "$fixture_root/runtime.toml" "${COOP_NEXT_LISTEN_HTTP:-127.0.0.1:4580}"
build_config="$fixture_root/prepare.toml"
write_runtime_config "$build_config" "127.0.0.1:0"

log_file="$fixture_root/prepare.log"
: > "$log_file"
"$daemon" --config "$build_config" >> "$log_file" 2>&1 &
daemon_pid=$!
cleanup() {
  kill "$daemon_pid" 2>/dev/null || true
  wait "$daemon_pid" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

# Success requires proof that the daemon itself compiled, published, and then
# loaded this deployment. An artifact merely existing on disk is not evidence:
# an earlier run once satisfied a naive file check with a package the daemon
# had just refused.
loaded() {
  grep -F 'application library preloaded on dedicated Perry thread' "$log_file" \
    | grep -Fq 'next-bench'
}

deadline=$(( $(date +%s) + timeout_seconds ))
while true; do
  if grep -Fq 'failed to load deployment during initial scan' "$log_file"; then
    cat "$log_file" >&2
    echo "Coop could not build the next-bench deployment" >&2
    exit 1
  fi
  if loaded; then
    app="$(find "$compiled/next-bench" -mindepth 2 -maxdepth 2 -type f -name "app.$extension" -print -quit 2>/dev/null || true)"
    if [[ -z "$app" || ! -f "${app%.$extension}.coop-lib.json" ]]; then
      cat "$log_file" >&2
      echo "Coop loaded next-bench but published no application library" >&2
      exit 1
    fi
    echo "$app"
    exit 0
  fi
  if ! kill -0 "$daemon_pid" 2>/dev/null; then
    cat "$log_file" >&2
    echo "Coop exited before publishing the Next benchmark fixture" >&2
    exit 1
  fi
  if (( $(date +%s) >= deadline )); then
    cat "$log_file" >&2
    echo "Timed out after ${timeout_seconds}s preparing the Next benchmark fixture" >&2
    exit 1
  fi
  sleep 0.25
done
