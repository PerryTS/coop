#!/usr/bin/env bash
set -euo pipefail

# Satisfy Perry runtime references from its Rust dylib first, then leave the
# rlib available only for generic monomorphizations that the runtime dylib
# cannot pre-export. This keeps the runtime's stateful API and C ABI in one
# process-wide image while allowing perry-stdlib's Rust-level API calls.
# Both artifacts must come from the exact same Perry revision and Rust toolchain.
runtime_dylib="${PERRY_RUNTIME_DYLIB:?set PERRY_RUNTIME_DYLIB to libperry_runtime.dylib}"
if [[ ! -f "$runtime_dylib" ]]; then
  echo "Perry runtime dylib does not exist: $runtime_dylib" >&2
  exit 1
fi

args=()
host_os="$(uname -s)"
skip_export_list_value=false
original_export_list=""
saw_runtime_rlib=false
for arg in "$@"; do
  if [[ "$skip_export_list_value" == "true" ]]; then
    original_export_list="${arg#-Wl,}"
    skip_export_list_value=false
    continue
  fi

  case "$arg" in
    *libperry_runtime-*.rlib)
      saw_runtime_rlib=true
      if [[ "$host_os" == "Linux" ]]; then
        # rustc places Rust rlibs inside a -Bstatic group. A shared-object path
        # inherits that mode, so GNU ld rejects it as an attempted static link.
        # Select and retain the process-wide runtime dynamically, then restore
        # static mode for the fallback rlib and the remainder of Rust's group.
        args+=(
          "-Wl,-Bdynamic"
          "-Wl,--no-as-needed"
          "$runtime_dylib"
          "-Wl,--as-needed"
          "-Wl,-Bstatic"
          "$arg"
        )
      else
        args+=("$runtime_dylib" "$arg")
      fi
      ;;
    -Wl,-exported_symbols_list)
      skip_export_list_value=true
      ;;
    -Wl,-exported_symbols_list,*)
      original_export_list="${arg#-Wl,-exported_symbols_list,}"
      ;;
    *) args+=("$arg") ;;
  esac
done

custom_export_list=""
if [[ -n "$original_export_list" ]]; then
  if [[ "$saw_runtime_rlib" == "true" ]]; then
    custom_export_list="$(mktemp "${TMPDIR:-/tmp}/perch-perry-exports.XXXXXX")"
    # Keep stdlib's intended C exports plus every public symbol from the runtime
    # dylib. The latter makes Rust-level runtime calls interposable too, without
    # exporting hundreds of thousands of unrelated dependency symbols.
    {
      cat "$original_export_list"
      nm -gU "$runtime_dylib" | awk 'NF >= 3 { print $3 }'
    } | sort -u > "$custom_export_list"
    args+=("-Wl,-exported_symbols_list" "-Wl,$custom_export_list")
  else
    args+=("-Wl,-exported_symbols_list" "-Wl,$original_export_list")
  fi
fi

if [[ "$saw_runtime_rlib" == "true" && "$host_os" == "Darwin" ]]; then
  # stdlib's fallback generic code may contain duplicate public runtime
  # definitions. Flat/interposable binding guarantees every reference selects
  # the process-first runtime image loaded by Perch.
  args+=("-Wl,-rpath,@loader_path" "-Wl,-flat_namespace" "-Wl,-interposable")
elif [[ "$saw_runtime_rlib" == "true" ]]; then
  args+=('-Wl,-rpath,$ORIGIN' '-Wl,-soname,libperry_stdlib.so')
fi

set +e
/usr/bin/cc "${args[@]}"
status=$?
set -e
if [[ -n "$custom_export_list" ]]; then
  rm -f "$custom_export_list"
fi
exit "$status"
