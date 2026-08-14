#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
fixture_root="${PERCH_BENCH_FIXTURE_DIR:-$repo_root/target/dynamic-smoke}"
daemon="${PERCH_BENCH_DAEMON:-$repo_root/target/release/perch}"
perry="${PERCH_BENCH_PERRY:-$repo_root/.perry-main/target/perry-dev/perry}"
provider_verification="${PERCH_BENCH_PROVIDER_VERIFICATION:-full_hash}"
gc_reclaim_check_interval="${PERCH_BENCH_GC_RECLAIM_CHECK_INTERVAL:-0}"
gc_reclaim_growth_bytes="${PERCH_BENCH_GC_RECLAIM_GROWTH_BYTES:-262144}"

case "$(uname -s)" in
  Darwin) extension="dylib" ;;
  Linux) extension="so" ;;
  *) echo "unsupported benchmark host: $(uname -s)" >&2; exit 1 ;;
esac
runtime="${PERCH_BENCH_RUNTIME:-$repo_root/var/perch/lib/libperry_runtime.$extension}"
stdlib="${PERCH_BENCH_STDLIB:-$repo_root/var/perch/lib/libperry_stdlib.$extension}"

for required in \
  "$daemon" \
  "$perry" \
  "$runtime" \
  "$stdlib"; do
  if [[ ! -f "$required" ]]; then
    echo "required benchmark input is missing: $required" >&2
    exit 1
  fi
done

deployment="$fixture_root/deployments/test1"
compiled="$fixture_root/compiled"
mkdir -p \
  "$deployment/handlers" \
  "$compiled" \
  "$fixture_root/sockets" \
  "$fixture_root/storage" \
  "$fixture_root/logs" \
  "$fixture_root/acme"
cp "$repo_root/benchmarks/tiny-perry/perch.toml" "$deployment/perch.toml"
cp "$repo_root/benchmarks/tiny-perry/handlers/main.ts" "$deployment/handlers/main.ts"

runtime_config="$fixture_root/runtime.toml"
cat > "$runtime_config" <<EOF
[http]
listen_http = "127.0.0.1:0"

[execution]
mode = "in_process"
provider_verification = "$provider_verification"
gc_reclaim_check_interval = $gc_reclaim_check_interval
gc_reclaim_growth_bytes = $gc_reclaim_growth_bytes

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

log_file="$fixture_root/prepare.log"
"$daemon" --config "$runtime_config" > "$log_file" 2>&1 &
daemon_pid=$!
cleanup() {
  kill "$daemon_pid" 2>/dev/null || true
  wait "$daemon_pid" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

for _ in $(seq 1 240); do
  if grep -Fq 'HTTP listener ready' "$log_file"; then
    app="$(find "$compiled/test1" -mindepth 2 -maxdepth 2 -type f -name "app.$extension" -print -quit 2>/dev/null || true)"
    if [[ -n "$app" && -f "${app%.$extension}.perch-lib.json" ]]; then
      echo "Prepared $app"
      exit 0
    fi
  fi
  if ! kill -0 "$daemon_pid" 2>/dev/null; then
    cat "$log_file" >&2
    echo "Perch exited before preparing the benchmark fixture" >&2
    exit 1
  fi
  sleep 0.25
done

cat "$log_file" >&2
echo "Timed out preparing the benchmark fixture" >&2
exit 1
