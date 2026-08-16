#!/usr/bin/env bash
set -euo pipefail

# Run the equivalent Worker-shaped compute fixture against the exact pinned
# celld revision. MinIO is an external development object store for this
# mechanism benchmark; its memory and CPU are deliberately not charged to the
# celld server cgroup. This is not a durability or production-storage claim.

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
celld_root="${COOP_CELLD_DIR:-$repo_root/.celld-main}"
celld_binary="${COOP_CELLD_BINARY:-$celld_root/target/release/celld}"
fixture="${COOP_CELLD_FIXTURE:-$repo_root/benchmarks/celld-small}"
esbuild="${CELLD_ESBUILD:-$(command -v esbuild || true)}"
expected_esbuild="${COOP_CELLD_ESBUILD_VERSION:-0.28.0}"
trials="${COOP_BENCH_TRIALS:-3}"
requests="${COOP_BENCH_REQUESTS:-20000}"
concurrency="${COOP_BENCH_CONCURRENCY:-50}"
listen_port="${COOP_CELLD_PORT:-4582}"
s3_port="${COOP_CELLD_S3_PORT:-19000}"
bucket="${COOP_CELLD_BUCKET:-coop-celld-benchmark}"
minio_image="${COOP_CELLD_MINIO_IMAGE:-minio/minio@sha256:14cea493d9a34af32f524e538b8346cf79f3321eff8e708c1e2960462bd8936e}"
mc_image="${COOP_CELLD_MC_IMAGE:-minio/mc@sha256:a7fe349ef4bd8521fb8497f55c6042871b2ae640607cf99d9bede5e9bdf11727}"
access_key="coopbenchmark"
secret_key="coopbenchmark-secret"
container="coop-celld-benchmark-$$"
temporary_base="${TMPDIR:-/tmp}"
work_dir="$(mktemp -d "$temporary_base/coop-celld-benchmark.XXXXXX")"

cleanup() {
  docker rm -f "$container" >/dev/null 2>&1 || true
  case "$work_dir" in
    "$temporary_base"/coop-celld-benchmark.*)
      rm -rf "$work_dir"
      ;;
    *)
      echo "refusing to remove unexpected celld benchmark directory: $work_dir" >&2
      ;;
  esac
}
trap cleanup EXIT INT TERM

for required in "$celld_binary" "$fixture/index.js" "$fixture/wrangler.jsonc"; do
  if [[ ! -f "$required" ]]; then
    echo "required celld benchmark input is missing: $required" >&2
    exit 1
  fi
done
if [[ ! -x "$celld_binary" ]]; then
  echo "celld benchmark binary is not executable: $celld_binary" >&2
  exit 1
fi
if [[ -z "$esbuild" || ! -x "$esbuild" ]]; then
  echo "esbuild is required; set CELLD_ESBUILD to an exact executable" >&2
  exit 1
fi
for command_name in curl docker git node; do
  command -v "$command_name" >/dev/null || {
    echo "$command_name is required for the celld mechanism benchmark" >&2
    exit 1
  }
done
for positive in "$trials" "$requests" "$concurrency" "$listen_port" "$s3_port"; do
  [[ "$positive" =~ ^[1-9][0-9]*$ ]] || {
    echo "celld benchmark counts and ports must be positive integers" >&2
    exit 1
  }
done
[[ "$bucket" =~ ^[a-z0-9][a-z0-9.-]+[a-z0-9]$ ]] || {
  echo "COOP_CELLD_BUCKET must be a valid lowercase S3 bucket name" >&2
  exit 1
}

locked_commit="$(sed -n 's/^commit = "\([0-9a-f]*\)"/\1/p' "$repo_root/celld-main.lock")"
actual_commit="$(git -C "$celld_root" rev-parse HEAD)"
if [[ -z "$locked_commit" || "$actual_commit" != "$locked_commit" ]]; then
  echo "celld checkout does not match celld-main.lock" >&2
  exit 1
fi
if [[ -n "$(git -C "$celld_root" status --short)" ]]; then
  echo "refusing to benchmark a dirty celld checkout: $celld_root" >&2
  exit 1
fi
actual_esbuild="$($esbuild --version)"
if [[ "$actual_esbuild" != "$expected_esbuild" ]]; then
  echo "esbuild version mismatch: expected $expected_esbuild, got $actual_esbuild" >&2
  exit 1
fi

sha256_file() {
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    sha256sum "$1" | awk '{print $1}'
  fi
}

docker pull "$minio_image" >/dev/null
docker pull "$mc_image" >/dev/null
docker run --detach --rm \
  --name "$container" \
  --tmpfs /data:rw,size=512m \
  --publish "127.0.0.1:$s3_port:9000" \
  --env "MINIO_ROOT_USER=$access_key" \
  --env "MINIO_ROOT_PASSWORD=$secret_key" \
  "$minio_image" server /data --address :9000 --console-address :9001 >/dev/null

for _ in $(seq 1 120); do
  if curl --fail --silent "http://127.0.0.1:$s3_port/minio/health/ready" >/dev/null; then
    break
  fi
  if ! docker inspect "$container" >/dev/null 2>&1; then
    echo "MinIO exited before becoming ready" >&2
    exit 1
  fi
  sleep 0.1
done
curl --fail --silent "http://127.0.0.1:$s3_port/minio/health/ready" >/dev/null

docker run --rm --network "container:$container" \
  --env "MC_HOST_bench=http://$access_key:$secret_key@127.0.0.1:9000" \
  "$mc_image" \
  mb --ignore-existing "bench/$bucket" >/dev/null

export AWS_ACCESS_KEY_ID="$access_key"
export AWS_SECRET_ACCESS_KEY="$secret_key"
export AWS_REGION=us-east-1
export AWS_DEFAULT_REGION=us-east-1
export AWS_EC2_METADATA_DISABLED=true
export CELLD_ESBUILD="$esbuild"

"$celld_binary" deploy "$fixture" \
  --bucket "s3://$bucket" \
  --endpoint "http://127.0.0.1:$s3_port" \
  --region us-east-1

printf 'celld_commit=%s\n' "$actual_commit"
printf 'celld_version=%s\n' "$($celld_binary --version)"
printf 'celld_binary_sha256=%s\n' "$(sha256_file "$celld_binary")"
printf 'esbuild_version=%s\n' "$actual_esbuild"
printf 'minio_image=%s\n' "$minio_image"
printf 'mc_image=%s\n' "$mc_image"
printf 'node_version=%s\n' "$(node --version)"
printf 'kernel=%s\n' "$(uname -srvmo)"
printf 'object_store_accounting=external-development-dependency\n'

node "$repo_root/benchmarks/server-benchmark.mjs" \
  --name celld-worker \
  --trials "$trials" \
  --requests "$requests" \
  --concurrency "$concurrency" \
  --port "$listen_port" \
  --expected-runtime celld \
  --path "/api/benchmark?iterations=100" \
  --env "CELLD_WATCH=$work_dir/watch-{trial}" \
  --env CELLD_MAX_RSS_MB=0 \
  --env RUST_LOG=warn \
  -- "$celld_binary" \
  --bucket "s3://$bucket" \
  --endpoint "http://127.0.0.1:$s3_port" \
  --region us-east-1 \
  --listen "127.0.0.1:{port}" \
  --internal-listen 127.0.0.1:0
