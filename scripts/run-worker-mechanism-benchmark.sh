#!/usr/bin/env bash
set -euo pipefail

# Compare the same URL/query, 100-iteration integer checksum, and JSON response
# across Perry, plain Node, and (optionally) celld. Compilation/deployment is
# completed before measurement. Every Perry trial is a restart over the exact
# eagerly activated immutable package; celld reports listener-ready and lazy
# first-cell activation separately through the common harness.

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
fixture_root="${COOP_WORKER_BENCH_ROOT:-$repo_root/target/worker-mechanism-benchmark}"
perry_fixture="${COOP_BENCH_PERRY_FIXTURE:-$repo_root/benchmarks/worker-perry}"
daemon="${COOP_BENCH_DAEMON:-$repo_root/target/release/coop}"
perry="${COOP_BENCH_PERRY:-$repo_root/.perry-main/target/perry-dev/perry}"
runtime="${COOP_BENCH_RUNTIME:-$repo_root/var/coop/lib/libperry_runtime.so}"
stdlib="${COOP_BENCH_STDLIB:-$repo_root/var/coop/lib/libperry_stdlib.so}"
perry_port="${COOP_WORKER_PERRY_PORT:-4580}"
node_port="${COOP_WORKER_NODE_PORT:-4581}"
trials="${COOP_BENCH_TRIALS:-3}"
requests="${COOP_BENCH_REQUESTS:-20000}"
concurrency="${COOP_BENCH_CONCURRENCY:-50}"
include_celld="${COOP_BENCH_INCLUDE_CELLD:-0}"
provider_verification="${COOP_BENCH_PROVIDER_VERIFICATION:-full_hash}"
gc_reclaim_check_interval="${COOP_BENCH_GC_RECLAIM_CHECK_INTERVAL:-0}"
gc_reclaim_growth_bytes="${COOP_BENCH_GC_RECLAIM_GROWTH_BYTES:-262144}"

if [[ "$(uname -s)" == Darwin ]]; then
  runtime="${COOP_BENCH_RUNTIME:-$repo_root/var/coop/lib/libperry_runtime.dylib}"
  stdlib="${COOP_BENCH_STDLIB:-$repo_root/var/coop/lib/libperry_stdlib.dylib}"
fi

for required in \
  "$daemon" \
  "$perry" \
  "$runtime" \
  "$stdlib" \
  "$perry_fixture/coop.toml" \
  "$perry_fixture/handlers/main.ts" \
  "$repo_root/benchmarks/worker-node/server.mjs"; do
  if [[ ! -f "$required" ]]; then
    echo "required Worker benchmark input is missing: $required" >&2
    exit 1
  fi
done
for command_name in curl node; do
  command -v "$command_name" >/dev/null || {
    echo "$command_name is required for the Worker mechanism benchmark" >&2
    exit 1
  }
done
for positive in "$perry_port" "$node_port" "$trials" "$requests" "$concurrency"; do
  [[ "$positive" =~ ^[1-9][0-9]*$ ]] || {
    echo "Worker benchmark counts and ports must be positive integers" >&2
    exit 1
  }
done
if [[ "$perry_port" == "$node_port" ]]; then
  echo "Perry and Node benchmark ports must differ" >&2
  exit 1
fi
if [[ "$include_celld" != 0 && "$include_celld" != 1 ]]; then
  echo "COOP_BENCH_INCLUDE_CELLD must be 0 or 1" >&2
  exit 1
fi
if [[ "$provider_verification" != full_hash \
  && "$provider_verification" != root_owned_immutable ]]; then
  echo "COOP_BENCH_PROVIDER_VERIFICATION must be full_hash or root_owned_immutable" >&2
  exit 1
fi
if ! [[ "$gc_reclaim_check_interval" =~ ^[0-9]+$ ]] \
  || ! [[ "$gc_reclaim_growth_bytes" =~ ^[1-9][0-9]*$ ]]; then
  echo "Perry GC reclaim interval must be non-negative and growth bytes positive" >&2
  exit 1
fi

sha256_file() {
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    sha256sum "$1" | awk '{print $1}'
  fi
}

mkdir -p \
  "$fixture_root/deployments/worker-benchmark/handlers" \
  "$fixture_root/compiled" \
  "$fixture_root/sockets" \
  "$fixture_root/storage" \
  "$fixture_root/logs" \
  "$fixture_root/acme"
cp "$perry_fixture/coop.toml" \
  "$fixture_root/deployments/worker-benchmark/coop.toml"
cp "$perry_fixture/handlers/main.ts" \
  "$fixture_root/deployments/worker-benchmark/handlers/main.ts"

config="$fixture_root/runtime.toml"
cat > "$config" <<EOF
[http]
listen_http = "127.0.0.1:$perry_port"

[execution]
mode = "in_process"
provider_verification = "$provider_verification"
compile_concurrency = 1
compile_march = "generic"
watch_deployments = false
artifact_retention_count = 3
artifact_retention_days = 0
gc_reclaim_check_interval = $gc_reclaim_check_interval
gc_reclaim_growth_bytes = $gc_reclaim_growth_bytes

[paths]
deployments_dir = "$fixture_root/deployments"
compiled_dir = "$fixture_root/compiled"
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

# Reset only this generated benchmark deployment's active pointer. Otherwise a
# previous run with different source/configuration can restore its old package
# while `coop build` merely prebuilds (but does not activate) the new one.
# Package bytes and compiler caches remain available for exact reuse.
active_state="$fixture_root/compiled/worker-benchmark/.coop-deployment-state.json"
rm -f -- "$active_state"

# Build once, then start and stop once to publish the exact current package.
# Priming is outside the measured restart trials and validates the exact oracle.
"$daemon" --config "$config" build worker-benchmark
prime_log="$fixture_root/prime.log"
"$daemon" --config "$config" > "$prime_log" 2>&1 &
prime_pid=$!
cleanup_prime() {
  if kill -0 "$prime_pid" 2>/dev/null; then
    kill "$prime_pid" 2>/dev/null || true
  fi
  wait "$prime_pid" 2>/dev/null || true
}
trap cleanup_prime EXIT INT TERM
expected='{"runtime":"perry","iterations":100,"checksum":3726872593}'
observed=""
for _ in $(seq 1 600); do
  if ! kill -0 "$prime_pid" 2>/dev/null; then
    cat "$prime_log" >&2
    echo "Perry benchmark daemon exited during priming" >&2
    exit 1
  fi
  observed="$(curl --silent --show-error --max-time 1 \
    --header 'Host: benchmark.local' \
    "http://127.0.0.1:$perry_port/api/benchmark?iterations=100" 2>/dev/null || true)"
  if [[ "$observed" == "$expected" ]]; then
    break
  fi
  sleep 0.05
done
if [[ "$observed" != "$expected" ]]; then
  cat "$prime_log" >&2
  echo "Perry benchmark priming returned: $observed" >&2
  exit 1
fi
cleanup_prime
trap - EXIT INT TERM

if [[ ! -f "$active_state" ]]; then
  echo "Perry benchmark did not publish active immutable package state" >&2
  exit 1
fi
package="$(node -e 'const fs=require("fs"); const s=JSON.parse(fs.readFileSync(process.argv[1])); process.stdout.write(s.active.package_sha256)' "$active_state")"
case "$package" in
  ""|*[!0-9a-f]*)
    echo "invalid active Perry package identity: $package" >&2
    exit 1
    ;;
esac

printf 'worker_oracle=iterations:100,checksum:3726872593,json\n'
printf 'perry_package_sha256=%s\n' "$package"
printf 'perry_provider_verification=%s\n' "$provider_verification"
printf 'perry_gc_reclaim_check_interval=%s\n' "$gc_reclaim_check_interval"
printf 'perry_gc_reclaim_growth_bytes=%s\n' "$gc_reclaim_growth_bytes"
printf 'perry_runtime_path=%s\n' "$(readlink -f -- "$runtime")"
printf 'perry_stdlib_path=%s\n' "$(readlink -f -- "$stdlib")"
printf 'perry_runtime_sha256=%s\n' "$(sha256_file "$runtime")"
printf 'perry_stdlib_sha256=%s\n' "$(sha256_file "$stdlib")"
printf 'coop_daemon_sha256=%s\n' "$(sha256_file "$daemon")"
printf 'perry_fixture_sha256=%s\n' "$(sha256_file "$perry_fixture/handlers/main.ts")"
printf 'node_fixture_sha256=%s\n' "$(sha256_file "$repo_root/benchmarks/worker-node/server.mjs")"
printf 'node_version=%s\n' "$(node --version)"
printf 'kernel=%s\n' "$(uname -srvmo)"

node "$repo_root/benchmarks/server-benchmark.mjs" \
  --name perry-worker \
  --trials "$trials" \
  --requests "$requests" \
  --concurrency "$concurrency" \
  --port "$perry_port" \
  --expected-runtime perry \
  --path "/api/benchmark?iterations=100" \
  -- "$daemon" --config "$config"

node "$repo_root/benchmarks/server-benchmark.mjs" \
  --name node-worker \
  --trials "$trials" \
  --requests "$requests" \
  --concurrency "$concurrency" \
  --port "$node_port" \
  --expected-runtime node \
  --path "/api/benchmark?iterations=100" \
  --env "PORT={port}" \
  -- node "$repo_root/benchmarks/worker-node/server.mjs"

if [[ "$include_celld" == 1 ]]; then
  "$repo_root/scripts/run-celld-mechanism-benchmark.sh"
fi
