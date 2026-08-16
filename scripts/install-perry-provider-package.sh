#!/usr/bin/env bash
set -euo pipefail

# Install a fully hashed Perry provider pair into a root-owned immutable
# content namespace. The daemon may then select
# `provider_verification = "root_owned_immutable"` and prove the OS ownership
# boundary in milliseconds instead of rereading roughly 95 MiB every restart.

if [[ "$(uname -s)" != Linux ]]; then
  echo "immutable Perry provider installation currently requires Linux" >&2
  exit 69
fi
if [[ "$(id -u)" != 0 ]]; then
  echo "run this installer as root; the resulting package must be root-owned" >&2
  exit 77
fi
if [[ $# -lt 1 || $# -gt 2 ]]; then
  echo "usage: $0 SOURCE_PROVIDER_DIR [DESTINATION_ROOT]" >&2
  exit 64
fi
for command_name in cmp install jq mktemp readlink sha256sum stat; do
  command -v "$command_name" >/dev/null || {
    echo "$command_name is required to install Perry providers" >&2
    exit 69
  }
done

source_dir="$(readlink -f -- "$1")"
destination_root="${2:-/opt/coop/providers}"
if [[ "$destination_root" != /* ]]; then
  echo "destination root must be absolute" >&2
  exit 64
fi
if [[ ! -d "$source_dir" || -L "$source_dir" ]]; then
  echo "source provider directory must be a real directory: $source_dir" >&2
  exit 66
fi
manifest="$source_dir/perry-libraries.json"
if [[ ! -f "$manifest" || -L "$manifest" ]]; then
  echo "source provider manifest is missing or symbolic: $manifest" >&2
  exit 66
fi

runtime_file="$(jq -er '.runtime_file' "$manifest")"
stdlib_file="$(jq -er '.stdlib_file' "$manifest")"
runtime_sha256="$(jq -er '.runtime_sha256' "$manifest")"
stdlib_sha256="$(jq -er '.stdlib_sha256' "$manifest")"
runtime_size="$(jq -er '.runtime_size' "$manifest")"
stdlib_size="$(jq -er '.stdlib_size' "$manifest")"
for filename in "$runtime_file" "$stdlib_file"; do
  case "$filename" in
    ""|.|..|*/*|*\\*)
      echo "provider filenames must be plain path components: $filename" >&2
      exit 65
      ;;
  esac
done
for digest in "$runtime_sha256" "$stdlib_sha256"; do
  [[ "$digest" =~ ^[0-9a-f]{64}$ ]] || {
    echo "provider manifest contains an invalid SHA-256 digest" >&2
    exit 65
  }
done
for size in "$runtime_size" "$stdlib_size"; do
  [[ "$size" =~ ^[1-9][0-9]*$ ]] || {
    echo "provider manifest contains an invalid file size" >&2
    exit 65
  }
done

verify_file() {
  local path="$1"
  local expected_size="$2"
  local expected_sha256="$3"
  local label="$4"
  if [[ ! -f "$path" || -L "$path" ]]; then
    echo "$label is missing or symbolic: $path" >&2
    return 1
  fi
  local actual_size
  actual_size="$(stat -c %s -- "$path")"
  if [[ "$actual_size" != "$expected_size" ]]; then
    echo "$label size mismatch: expected $expected_size, got $actual_size" >&2
    return 1
  fi
  local actual_sha256
  actual_sha256="$(sha256sum -- "$path" | awk '{print $1}')"
  if [[ "$actual_sha256" != "$expected_sha256" ]]; then
    echo "$label SHA-256 mismatch" >&2
    return 1
  fi
}

verify_installed_permissions() {
  local path="$1"
  local expected_kind="$2"
  local label="$3"
  local actual_kind
  actual_kind="$(stat -c %F -- "$path")"
  if [[ "$actual_kind" != "$expected_kind" \
    || "$(stat -c %u -- "$path")" != 0 ]] \
    || (( (8#$(stat -c %a -- "$path") & 8#022) != 0 )); then
    echo "$label must be root-owned, not group/other writable, and a $expected_kind: $path" >&2
    return 1
  fi
}

verify_file "$source_dir/$runtime_file" "$runtime_size" "$runtime_sha256" "Perry runtime"
verify_file "$source_dir/$stdlib_file" "$stdlib_size" "$stdlib_sha256" "Perry stdlib"
manifest_sha256="$(sha256sum -- "$manifest" | awk '{print $1}')"

existing="$destination_root"
while [[ ! -e "$existing" && ! -L "$existing" ]]; do
  parent="$(dirname -- "$existing")"
  if [[ "$parent" == "$existing" ]]; then
    echo "could not resolve a safe destination ancestor" >&2
    exit 65
  fi
  existing="$parent"
done
if [[ -L "$existing" || ! -d "$existing" ]]; then
  echo "existing destination ancestor must be a real directory: $existing" >&2
  exit 65
fi
cursor="$existing"
while :; do
  if [[ "$(stat -c %u "$cursor")" != 0 ]] \
    || (( (8#$(stat -c %a "$cursor") & 8#022) != 0 )); then
    echo "destination ancestor must be root-owned and not group/other writable: $cursor" >&2
    exit 65
  fi
  [[ "$cursor" == / ]] && break
  cursor="$(dirname -- "$cursor")"
done
install -d -o root -g root -m 0755 "$destination_root"
if [[ -L "$destination_root" || ! -d "$destination_root" ]]; then
  echo "destination root must be a real directory: $destination_root" >&2
  exit 65
fi
if [[ "$(stat -c %u "$destination_root")" != 0 ]] \
  || (( (8#$(stat -c %a "$destination_root") & 8#022) != 0 )); then
  echo "destination root must be root-owned and not group/other writable" >&2
  exit 65
fi

destination="$destination_root/$manifest_sha256"
if [[ -e "$destination" ]]; then
  if [[ ! -d "$destination" || -L "$destination" ]]; then
    echo "existing provider destination is not a real directory: $destination" >&2
    exit 65
  fi
  cmp --silent "$manifest" "$destination/perry-libraries.json" || {
    echo "existing provider package manifest differs: $destination" >&2
    exit 65
  }
  verify_installed_permissions "$destination" directory "installed provider package"
  verify_installed_permissions "$destination/perry-libraries.json" "regular file" \
    "installed provider manifest"
  verify_installed_permissions "$destination/$runtime_file" "regular file" \
    "installed Perry runtime"
  verify_installed_permissions "$destination/$stdlib_file" "regular file" \
    "installed Perry stdlib"
  verify_file "$destination/$runtime_file" "$runtime_size" "$runtime_sha256" "installed Perry runtime"
  verify_file "$destination/$stdlib_file" "$stdlib_size" "$stdlib_sha256" "installed Perry stdlib"
  printf '%s\n' "$destination"
  exit 0
fi

staging="$(mktemp -d "$destination_root/.staging-$manifest_sha256.XXXXXXXX")"
cleanup() {
  if [[ -n "${staging:-}" && -d "$staging" ]]; then
    rm -rf -- "$staging"
  fi
}
trap cleanup EXIT INT TERM
install -o root -g root -m 0444 "$manifest" "$staging/perry-libraries.json"
install -o root -g root -m 0444 "$source_dir/$runtime_file" "$staging/$runtime_file"
install -o root -g root -m 0444 "$source_dir/$stdlib_file" "$staging/$stdlib_file"
if [[ "$(sha256sum -- "$staging/perry-libraries.json" | awk '{print $1}')" != "$manifest_sha256" ]]; then
  echo "provider manifest changed during installation" >&2
  exit 74
fi
verify_file "$staging/$runtime_file" "$runtime_size" "$runtime_sha256" "staged Perry runtime"
verify_file "$staging/$stdlib_file" "$stdlib_size" "$stdlib_sha256" "staged Perry stdlib"
chmod 0555 "$staging"
mv --no-target-directory -- "$staging" "$destination"
staging=""
trap - EXIT INT TERM
printf '%s\n' "$destination"
