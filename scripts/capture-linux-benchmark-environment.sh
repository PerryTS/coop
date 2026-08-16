#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != Linux ]]; then
  echo "Linux benchmark environment capture requires Linux" >&2
  exit 69
fi

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo_root"

emit() {
  local key="$1"
  local value="${2//$'\n'/,}"
  printf '%s=%s\n' "$key" "$value"
}

first_line() {
  "$@" 2>&1 | sed -n '1p'
}

file_identity() {
  local label="$1"
  local path="$2"
  if [[ -f "$path" ]]; then
    emit "${label}_path" "$path"
    emit "${label}_bytes" "$(stat -c %s -- "$path")"
    emit "${label}_sha256" "$(sha256sum -- "$path" | awk '{print $1}')"
  else
    emit "${label}_path" "missing"
  fi
}

workspace_files() {
  if git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    while IFS= read -r -d '' path; do
      case "$path" in
        benchmarks/results/*) continue ;;
      esac
      printf '%s\0' "$path"
    done < <(git ls-files --cached --others --exclude-standard -z) | sort -z
  else
    find . -type f \
      ! -path './.perry-main/*' \
      ! -path './.perry-candidate-main/*' \
      ! -path './.celld-main/*' \
      ! -path './target/*' \
      ! -path './var/*' \
      ! -path './benchmarks/results/*' \
      ! -path '*/node_modules/*' \
      ! -path '*/.next/*' \
      ! -path '*/.cache/*' \
      -print0 | sort -z
  fi
}

workspace_content_sha256="$({
  while IFS= read -r -d '' path; do
    if [[ -f "$path" ]]; then
      sha256sum -- "$path"
    else
      printf 'missing  %s\n' "$path"
    fi
  done < <(workspace_files)
} | sha256sum | awk '{print $1}')"

emit captured_at_utc "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
emit kernel "$(uname -srvmo)"
emit os_release "$(. /etc/os-release; printf '%s %s' "$NAME" "$VERSION_ID")"
emit architecture "$(uname -m)"
emit cpu_vendor "$(lscpu | sed -n 's/^Vendor ID:[[:space:]]*//p')"
emit cpu_model "$(lscpu | sed -n 's/^Model name:[[:space:]]*//p')"
emit logical_cpus "$(getconf _NPROCESSORS_ONLN)"
cpu_governors="$(find /sys/devices/system/cpu -path '*/cpufreq/scaling_governor' -type f -exec cat {} + 2>/dev/null | sort -u | paste -sd, -)"
emit cpu_governors "${cpu_governors:-unavailable}"
emit memory_total_kib "$(awk '$1 == "MemTotal:" {print $2}' /proc/meminfo)"
emit swap_total_kib "$(awk '$1 == "SwapTotal:" {print $2}' /proc/meminfo)"
emit load_average "$(tr ' ' ',' < /proc/loadavg)"
emit cgroup_filesystem "$(stat -fc %T /sys/fs/cgroup)"
emit cgroup_membership "$(awk -F: '$1 == "0" {print $3}' /proc/self/cgroup)"
emit cgroup_root_controllers "$(tr ' ' ',' < /sys/fs/cgroup/cgroup.controllers)"
emit delegated_cgroup_root "${COOP_DELEGATED_CGROUP_ROOT:-unset}"
emit benchmark_cgroup_root "${COOP_BENCH_CGROUP_ROOT:-unset}"

if git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  emit workspace_git_head "$(git rev-parse HEAD)"
  emit workspace_git_status_entries "$(git status --porcelain=v1 --untracked-files=all | wc -l | tr -d ' ')"
else
  emit workspace_git_head unavailable
  emit workspace_git_status_entries unavailable
fi
emit workspace_content_sha256 "$workspace_content_sha256"
file_identity cargo_lock Cargo.lock
file_identity perry_lock perry-main.lock
file_identity celld_lock celld-main.lock

if [[ -d .perry-main/.git || -f .perry-main/.git ]]; then
  emit perry_git_head "$(git -C .perry-main rev-parse HEAD)"
  emit perry_git_status_entries "$(git -C .perry-main status --porcelain=v1 --untracked-files=all | wc -l | tr -d ' ')"
fi
if [[ -d .celld-main/.git || -f .celld-main/.git ]]; then
  emit celld_git_head "$(git -C .celld-main rev-parse HEAD)"
  emit celld_git_status_entries "$(git -C .celld-main status --porcelain=v1 --untracked-files=all | wc -l | tr -d ' ')"
fi

emit rustc "$(first_line rustc --version --verbose)"
emit cargo "$(first_line cargo --version)"
emit node "$(first_line node --version)"
if command -v clang >/dev/null; then
  emit clang "$(first_line clang --version)"
elif command -v clang-22 >/dev/null; then
  emit clang "$(first_line clang-22 --version)"
else
  emit clang missing
fi
if command -v llvm-config-22 >/dev/null; then
  emit llvm "$(first_line llvm-config-22 --version)"
elif command -v llvm-config >/dev/null; then
  emit llvm "$(first_line llvm-config --version)"
else
  emit llvm missing
fi
if command -v docker >/dev/null; then
  emit docker "$(first_line docker version --format '{{.Client.Version}}/{{.Server.Version}}')"
else
  emit docker missing
fi
if [[ -n "${CELLD_ESBUILD:-}" && -x "${CELLD_ESBUILD}" ]]; then
  emit esbuild "$(first_line "$CELLD_ESBUILD" --version)"
else
  emit esbuild unset
fi

provider_runtime="${COOP_BENCH_RUNTIME:-var/coop/lib/libperry_runtime.so}"
provider_stdlib="${COOP_BENCH_STDLIB:-var/coop/lib/libperry_stdlib.so}"
provider_package_dir="$(dirname -- "$provider_runtime")"
provider_manifest="$provider_package_dir/perry-libraries.json"
emit provider_verification "${COOP_BENCH_PROVIDER_VERIFICATION:-full_hash}"
emit provider_package_dir "$(readlink -f -- "$provider_package_dir")"
file_identity provider_manifest "$provider_manifest"
if [[ -f "$provider_manifest" ]]; then
  file_identity perry_runtime "$provider_runtime"
  file_identity perry_stdlib "$provider_stdlib"
  emit provider_package_metadata "$(stat -c '%u:%g:%a:%F' -- "$provider_package_dir")"
  emit provider_manifest_metadata "$(stat -c '%u:%g:%a:%F' -- "$provider_manifest")"
  emit perry_runtime_metadata "$(stat -c '%u:%g:%a:%F' -- "$provider_runtime")"
  emit perry_stdlib_metadata "$(stat -c '%u:%g:%a:%F' -- "$provider_stdlib")"
fi
file_identity coop_daemon target/release/coop
file_identity coop_worker target/release/coop-worker
if [[ -n "${COOP_CELLD_BINARY:-}" ]]; then
  file_identity celld_binary "$COOP_CELLD_BINARY"
else
  emit celld_binary_path unset
fi
