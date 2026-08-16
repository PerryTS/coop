//! Force the linker to keep perry-runtime + perry-stdlib symbols that
//! dlopen'd deployment dylibs reference as undefined.
//!
//! Perry-compiled dylibs reference host runtime functions (`js_*`) as
//! undefined symbols resolved at dlopen time via flat-namespace lookup.
//! Cargo's linker dead-strips unreferenced symbols, so we must explicitly
//! reference every symbol a deployment might need.
//!
//! The symbol list is AUTO-GENERATED at build time by build.rs, which
//! scans the perry-runtime + perry-stdlib source trees for every
//! `#[no_mangle] pub extern "C" fn js_*` declaration. This eliminates the
//! whack-a-mole of manually pinning new symbols every Perry update.

include!(concat!(env!("OUT_DIR"), "/perry_symbols_generated.rs"));

#[inline(never)]
pub fn force_link_perry_runtime_symbols() {
    force_link_all_perry_symbols();
}
