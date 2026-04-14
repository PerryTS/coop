// Build script for perch-worker.
//
// On both platforms we need every perry-runtime symbol to be available
// for dlopen'd deployment dylibs to resolve at flat-namespace lookup time.
// Cargo's release linker dead-strips unreferenced symbols by default;
// since perry-runtime has 800+ exports and any of them might be needed
// by a future deployment, we'd be playing whack-a-mole forever if we
// only kept ones referenced by main.rs.
//
// Solution:
// - Linux: -rdynamic (puts all symbols in dynamic table) +
//   --no-as-needed (link the whole rlib even if no symbols are used).
// - macOS: by default symbols are in the dynamic table, but the linker
//   dead-strips. -Wl,-export_dynamic + force loading via the symbol_pin
//   module gets us most of the way; the remaining symbols are pinned
//   manually in symbol_pin.rs (which is now a hand-curated list of the
//   symbols our deployments actually use).
//
// The "right" fix for total robustness is to build perry-runtime as a
// staticlib and force-load it via -Wl,-force_load (macOS) or
// -Wl,--whole-archive (Linux). That requires a separate cargo invocation
// to produce the .a file. For now we accept the maintenance burden of
// the symbol_pin list and update it when new deployments need new
// symbols.

fn main() {
    #[cfg(target_os = "linux")]
    {
        // Export all symbols to the dynamic symbol table.
        println!("cargo:rustc-link-arg=-rdynamic");
        // Don't drop sections that aren't directly referenced.
        println!("cargo:rustc-link-arg=-Wl,--no-gc-sections");
    }

    // macOS: no extra flags needed at the build.rs level. The
    // symbol_pin manual list covers the symbols our deployments need,
    // and macOS's default flat-namespace lookup at dlopen time finds
    // anything pinned. If a deployment hits a missing symbol, add it
    // to symbol_pin.rs.
}
