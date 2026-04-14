// Build script for perch-worker.
//
// On Linux, we need -rdynamic (aka --export-dynamic) so that all symbols
// from perry-runtime are visible in the binary's dynamic symbol table.
// Without this, dlopen'd deployment dylibs can't resolve their undefined
// perry-runtime symbols (js_gc_init, js_string_from_bytes, etc.) from
// the host binary.
//
// On macOS this isn't needed because dylibs are built with
// -flat_namespace -undefined dynamic_lookup which resolves from the
// process's flat namespace. But adding -rdynamic on macOS is harmless.

fn main() {
    #[cfg(target_os = "linux")]
    {
        println!("cargo:rustc-link-arg=-rdynamic");
    }
}
