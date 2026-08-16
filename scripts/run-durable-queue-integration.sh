#!/usr/bin/env bash
set -euo pipefail

# Start an exact disposable PostgreSQL image and run the complete durable queue
# suite, including a real container stop/start while the daemon remains live.

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
postgres_image="${COOP_TEST_POSTGRES_IMAGE:-postgres@sha256:f3bd19c606e442c3d7bdfa8002e03fe260a1023351e0ea4598032022b68dd6e3}"
postgres_port="${COOP_TEST_POSTGRES_PORT:-15432}"
container="coop-durable-queue-postgres-$$"
password="coop-test-password"
database="coop_test"

cleanup() {
  docker rm -f "$container" >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

command -v docker >/dev/null || {
  echo "docker is required for the durable queue integration gate" >&2
  exit 1
}
[[ "$postgres_port" =~ ^[1-9][0-9]*$ ]] || {
  echo "COOP_TEST_POSTGRES_PORT must be a positive integer" >&2
  exit 1
}
case "$(uname -s)" in
  Darwin) library_extension=dylib ;;
  Linux) library_extension=so ;;
  *) echo "unsupported durable queue host: $(uname -s)" >&2; exit 1 ;;
esac
for provider in \
  "$repo_root/.perry-main/target/perry-dev/perry" \
  "$repo_root/var/coop/lib/libperry_runtime.$library_extension" \
  "$repo_root/var/coop/lib/libperry_stdlib.$library_extension"; do
  if [[ ! -f "$provider" ]]; then
    echo "required durable queue input is missing: $provider" >&2
    exit 1
  fi
done

docker pull "$postgres_image" >/dev/null
docker run --detach \
  --name "$container" \
  --publish "127.0.0.1:$postgres_port:5432" \
  --env "POSTGRES_PASSWORD=$password" \
  --env "POSTGRES_DB=$database" \
  --health-cmd "pg_isready -U postgres -d $database" \
  --health-interval 250ms \
  --health-timeout 2s \
  --health-retries 120 \
  "$postgres_image" >/dev/null

for _ in $(seq 1 240); do
  health="$(docker inspect --format '{{.State.Health.Status}}' "$container" 2>/dev/null || true)"
  if [[ "$health" == healthy ]]; then
    break
  fi
  if [[ "$health" == unhealthy ]]; then
    docker logs "$container" >&2
    echo "disposable PostgreSQL became unhealthy" >&2
    exit 1
  fi
  sleep 0.25
done
if [[ "$(docker inspect --format '{{.State.Health.Status}}' "$container")" != healthy ]]; then
  docker logs "$container" >&2
  echo "timed out waiting for disposable PostgreSQL" >&2
  exit 1
fi

printf 'postgres_image=%s\n' "$postgres_image"
printf 'postgres_container=%s\n' "$container"

COOP_TEST_POSTGRES_URL="postgresql://postgres:$password@127.0.0.1:$postgres_port/$database" \
COOP_TEST_POSTGRES_CONTAINER="$container" \
cargo test -p coop-daemon --test durable_queue \
  runtime_send_is_tenant_bound_durable_and_consumed \
  -- --ignored --nocapture --test-threads=1
