#!/usr/bin/env bash
set -euo pipefail

# Build the two process-wide Perry provider files from the pinned latest-main
# worktree. Perry currently publishes rlibs, so the runtime crate-type change is
# applied only for this build and is restored even if cargo fails.

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
perry_root="${PERRY_MAIN_DIR:-$repo_root/.perry-main}"
output_dir="${PERRY_LIBRARY_DIR:-$repo_root/var/perch/lib}"
arena_block_size_bytes="${PERRY_ARENA_BLOCK_SIZE_BYTES:-131072}"
arena_fresh_block_min_used_bytes=$((arena_block_size_bytes / 4))
provider_allocator="${PERRY_PROVIDER_ALLOCATOR:-system}"

if ! [[ "$arena_block_size_bytes" =~ ^[0-9]+$ ]] \
  || (( arena_block_size_bytes < 131072 )) \
  || (( arena_block_size_bytes > 1048576 )) \
  || (( (arena_block_size_bytes & (arena_block_size_bytes - 1)) != 0 )); then
  echo "PERRY_ARENA_BLOCK_SIZE_BYTES must be a power of two from 131072 through 1048576" >&2
  exit 1
fi
if [[ "$provider_allocator" != "system" && "$provider_allocator" != "mimalloc" ]]; then
  echo "PERRY_PROVIDER_ALLOCATOR must be system or mimalloc" >&2
  exit 1
fi

host_os="$(uname -s)"
host_arch="$(uname -m)"
case "$host_os" in
  Darwin)
    library_extension="dylib"
    runtime_filename="libperry_runtime.dylib"
    stdlib_filename="libperry_stdlib.dylib"
    case "$host_arch" in
      arm64) cargo_linker_env="CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER"; target_arch="aarch64" ;;
      x86_64) cargo_linker_env="CARGO_TARGET_X86_64_APPLE_DARWIN_LINKER"; target_arch="x86_64" ;;
      *) echo "unsupported macOS architecture: $host_arch" >&2; exit 1 ;;
    esac
    target_os="macos"
    ;;
  Linux)
    library_extension="so"
    runtime_filename="libperry_runtime.so"
    stdlib_filename="libperry_stdlib.so"
    case "$host_arch" in
      aarch64) cargo_linker_env="CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER"; target_arch="aarch64" ;;
      x86_64) cargo_linker_env="CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER"; target_arch="x86_64" ;;
      *) echo "unsupported Linux architecture: $host_arch" >&2; exit 1 ;;
    esac
    target_os="linux"
    ;;
  *)
    echo "unsupported provider host: $host_os/$host_arch" >&2
    exit 1
    ;;
esac
if [[ ! -d "$perry_root/.git" && ! -f "$perry_root/.git" ]]; then
  echo "Perry latest-main worktree is missing: $perry_root" >&2
  echo "Run scripts/sync-perry-main.sh first." >&2
  exit 1
fi
if [[ -n "$(git -C "$perry_root" status --short)" ]]; then
  echo "Refusing to package a dirty Perry worktree: $perry_root" >&2
  exit 1
fi

locked_version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$repo_root/perry-main.lock")"
locked_commit="$(sed -n 's/^commit = "\([^"]*\)"/\1/p' "$repo_root/perry-main.lock")"
perry_version="$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\([^"]*\)"/\1/p' "$perry_root/Cargo.toml")"
perry_commit="$(git -C "$perry_root" rev-parse HEAD)"
if [[ "$perry_version" != "$locked_version" || "$perry_commit" != "$locked_commit" ]]; then
  echo "Perry worktree does not match perry-main.lock; run scripts/sync-perry-main.sh" >&2
  exit 1
fi

sha256_file() {
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    sha256sum "$1" | awk '{print $1}'
  fi
}

(cd "$perry_root" && cargo build --profile perry-dev -p perry)
compiler_build="$perry_root/target/perry-dev/perry"
compiler_sha256="$(sha256_file "$compiler_build")"

runtime_manifest="$perry_root/crates/perry-runtime/Cargo.toml"
arena_block_source="$perry_root/crates/perry-runtime/src/arena/block.rs"
backup_dir="$(mktemp -d "${TMPDIR:-/tmp}/perch-perry-build.XXXXXX")"
cp "$runtime_manifest" "$backup_dir/perry-runtime.Cargo.toml"
cp "$arena_block_source" "$backup_dir/arena-block.rs"
manifest_restored=false
source_restored=false
restore_manifest() {
  if [[ "$manifest_restored" == "false" ]]; then
    cp "$backup_dir/perry-runtime.Cargo.toml" "$runtime_manifest"
    manifest_restored=true
  fi
}
restore_source() {
  if [[ "$source_restored" == "false" ]]; then
    cp "$backup_dir/arena-block.rs" "$arena_block_source"
    source_restored=true
  fi
}
cleanup() {
  restore_manifest
  restore_source
  rm -rf "$backup_dir"
}
trap cleanup EXIT INT TERM

perl -0pi -e 's/crate-type = \["rlib"\]/crate-type = ["dylib"]/ or die "runtime crate-type marker missing\n"' "$runtime_manifest"
PERCH_ARENA_BLOCK_SIZE="$arena_block_size_bytes" perl -0pi -e \
  's/pub\(crate\) const BLOCK_SIZE: usize = 1024 \* 1024;/pub(crate) const BLOCK_SIZE: usize = $ENV{PERCH_ARENA_BLOCK_SIZE};/ or die "arena block-size marker missing\n"' \
  "$arena_block_source"
PERCH_ARENA_FRESH_BLOCK_MIN_USED_BYTES="$arena_fresh_block_min_used_bytes" perl -0pi -e \
  's/pub\(crate\) const FRESH_GENERAL_BLOCK_MIN_USED_BYTES: usize = 256 \* 1024;/pub(crate) const FRESH_GENERAL_BLOCK_MIN_USED_BYTES: usize = $ENV{PERCH_ARENA_FRESH_BLOCK_MIN_USED_BYTES};/ or die "fresh-block threshold marker missing\n"' \
  "$arena_block_source"

# A runtime provider is never used without the separate stdlib provider. Build
# it with Perry's symbol-suppression `stdlib` feature so runtime-only fallback
# definitions (Request/Headers/fetch, the stdlib pump, WebSocket, readline,
# etc.) are absent. If those stubs are exported by the process-first dylib,
# dyld correctly-but-disastrously binds app calls to them before it can see the
# real implementations in libperry_stdlib.
runtime_feature_args=(--features stdlib)
if [[ "$provider_allocator" == "system" ]]; then
  runtime_feature_args=(
    --no-default-features
    --features
    "full,regex-engine,temporal,url-engine,string-normalize,intl-segmenter,intl-namespace,global-math,global-json,global-reflect,global-atomics,global-url,global-text,global-websocket,global-webcrypto,global-webfetch,proc-ipc,intl-locale,intl-datetime,diagnostics,mod-dgram,mod-http2-constants,mod-node-test,dyn-eval,keepalive-anchors,stdlib"
  )
fi
if [[ "$host_os" == "Darwin" ]]; then
  (cd "$perry_root" && cargo rustc --profile perry-dev -p perry-runtime \
    "${runtime_feature_args[@]}" -- \
    -C 'link-arg=-Wl,-install_name,@rpath/libperry_runtime.dylib' \
    -C link-arg=-framework -C link-arg=CoreFoundation \
    -C link-arg=-framework -C link-arg=Foundation)
else
  (cd "$perry_root" && cargo rustc --profile perry-dev -p perry-runtime \
    "${runtime_feature_args[@]}" -- \
    -C 'link-arg=-Wl,-soname,libperry_runtime.so')
fi

runtime_build="$perry_root/target/perry-dev/libperry_runtime.$library_extension"
if [[ ! -f "$runtime_build" ]]; then
  echo "Perry runtime build did not produce $runtime_build" >&2
  exit 1
fi

# Restore rlib mode before compiling the stdlib wrapper. Its final-link shim
# resolves stateful/public runtime symbols against the dylib first and retains
# the rlib only for Rust generic glue unavailable from a dylib export surface.
restore_manifest

env PERRY_RUNTIME_DYLIB="$runtime_build" \
  "$cargo_linker_env=$repo_root/scripts/perry-stdlib-linker.sh" \
  cargo build --manifest-path "$repo_root/Cargo.toml" \
  --profile perry-shared -p perch-perry-stdlib-shared

stdlib_build="$repo_root/target/perry-shared/libperch_perry_stdlib.$library_extension"
if [[ ! -f "$stdlib_build" ]]; then
  echo "Perry stdlib build did not produce $stdlib_build" >&2
  exit 1
fi

mkdir -p "$output_dir"
runtime_output="$output_dir/$runtime_filename"
stdlib_output="$output_dir/$stdlib_filename"
cp "$runtime_build" "$runtime_output"
cp "$stdlib_build" "$stdlib_output"
chmod 755 "$runtime_output" "$stdlib_output"
if [[ "$host_os" == "Darwin" ]]; then
  install_name_tool -id '@rpath/libperry_runtime.dylib' "$runtime_output"
  install_name_tool -id '@rpath/libperry_stdlib.dylib' "$stdlib_output"
fi
# Deployable providers do not need the local/debug symbol tables retained in
# Cargo's build artifacts. Keep every dynamic export (including Rust symbols
# used across the provider boundary), but remove non-exported metadata before
# these exact bytes are packaged and loaded by Perch.
if [[ "$host_os" == "Darwin" ]]; then
  strip -S -x "$runtime_output" "$stdlib_output"
  if ! otool -L "$stdlib_output" | grep -Fq '@rpath/libperry_runtime.dylib'; then
    echo "Packaged Perry stdlib is not bound to the separate runtime dylib" >&2
    exit 1
  fi
else
  strip --strip-debug --discard-all "$runtime_output" "$stdlib_output"
  if ! readelf -d "$stdlib_output" | grep -Fq "Shared library: [$runtime_filename]"; then
    echo "Packaged Perry stdlib is not bound to the separate runtime shared object" >&2
    exit 1
  fi
fi

rustc_version="$(rustc --version)"
runtime_sha256="$(sha256_file "$runtime_output")"
stdlib_sha256="$(sha256_file "$stdlib_output")"
runtime_size="$(wc -c < "$runtime_output" | tr -d ' ')"
stdlib_size="$(wc -c < "$stdlib_output" | tr -d ' ')"
cat > "$output_dir/perry-libraries.json" <<EOF
{
  "manifest_version": 2,
  "perry_version": "$perry_version",
  "perry_commit": "$perry_commit",
  "compiler_sha256": "$compiler_sha256",
  "rustc_version": "$rustc_version",
  "target": "$target_arch-$target_os",
  "arena_block_size_bytes": $arena_block_size_bytes,
  "allocator": "$provider_allocator",
  "runtime_file": "$runtime_filename",
  "runtime_sha256": "$runtime_sha256",
  "runtime_size": $runtime_size,
  "stdlib_file": "$stdlib_filename",
  "stdlib_sha256": "$stdlib_sha256",
  "stdlib_size": $stdlib_size
}
EOF

echo "Packaged Perry $perry_version ($perry_commit) in $output_dir"
