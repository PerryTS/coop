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

# Build the production App Route. The handler drives Next's own
# `AppRouteRouteModule.handle` out of `.next/server`, so this output is a
# compiler input, not an optional artifact.
#
# It is BUILT rather than committed on purpose. A `.next-production-bundle/`
# used to live in git, and it had silently drifted from
# `app/api/benchmark/route.ts`: the committed copy parsed
# `nextUrl.searchParams`, clamped iterations, set an `x-perch-benchmark-body`
# header, and was emitted by a different bundler entirely. Coop would have
# been measured against different code than the Node standalone build compiles
# from the same source, which is not a comparison at all.
route_build="$source_root/.next/server/app/api/benchmark/route.js"
if [[ ! -f "$route_build" || "$source_root/app/api/benchmark/route.ts" -nt "$route_build" ]]; then
  echo "building the production Next App Route (source is newer than the build)" >&2
  ( cd "$source_root" && npx --no-install next build >/dev/null ) || \
    fail "next build failed in $source_root" "(cd $source_root && npx next build)"
fi
if [[ ! -f "$route_build" ]]; then
  fail "next build produced no $route_build" \
    "check the Next version and app/api/benchmark/route.ts"
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
# Stage the production build output as ordinary deployment source.
#
# Location is the signal, not extension. `collect_modules.rs` compiles a
# `.js`/`.cjs`/`.mjs` file through the native AOT pipeline exactly like a `.ts`
# file when it is project source, and classifies it as a runtime-JS module only
# when it sits under `node_modules`. There is no V8 fallback any more, so that
# classification is now a refusal.
#
# Two earlier attempts here were wrong in instructive ways:
#
#   * a `.next` SYMLINK beside the handler -- `collect_source_files` skips
#     dot-directories and rejects symlinks outright, so it never reached the
#     compiler at all and Perry reported a missing module.
#   * a copy into `node_modules/.coop-next-bundle/` -- that reached the
#     compiler, but sitting under node_modules gave it the runtime-JS
#     classification, and the leading dot excluded it from
#     `collect_packages_in_node_modules`, so the automatic `compilePackages`
#     `"*"` expansion (the default when no package.json pins the key) never
#     covered it either. Both of those were self-inflicted.
#
# Plain directory, no dot, outside node_modules: `collect_source_files` walks
# it, every `.js` is collected, and Perry compiles the whole bundle natively
# with no opt-in required.
bundle_dir="$deployment/next-build"
rm -rf "$bundle_dir"
mkdir -p "$bundle_dir"
cp -R "$source_root/.next/server" "$bundle_dir/server"

# Assert the route reached the staging tree. The failure this guards is silent
# and costs a full CI cycle: the import resolves to nothing and Perry reports a
# missing module rather than a missing FILE.
staged_route="$bundle_dir/server/app/api/benchmark/route.js"
if [[ ! -f "$staged_route" ]]; then
  fail "the production route did not reach the staged bundle: $staged_route" \
    "check that next build produced .next/server/app/api/benchmark/route.js"
fi

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
  # Two load paths, two log lines. A dedicated worker logs "preloaded on
  # dedicated Perry thread"; an in-daemon load (isolation resolving to
  # "trusted", which is what this fixture gets) logs "preloading application
  # library in daemon". Waiting only for the first meant a perfectly good
  # build sat until the one-shot daemon exited, and the script then reported
  # "Coop exited before publishing" for a fixture it had already published --
  # the log said `compilation succeeded and immutable package was published`
  # three lines above the error.
  #
  # Matching either keeps this honest about what it is waiting for: the app
  # being loaded, not the particular thread it landed on. The published-package
  # check below is what actually validates the result.
  grep -E 'application library preloaded on dedicated Perry thread|preloading application library in daemon' "$log_file" \
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
