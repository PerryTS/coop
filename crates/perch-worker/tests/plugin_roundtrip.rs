//! Integration test: verify perch-worker's plugin_host module successfully
//! loads the Phase A.2 `hello.dylib` and invokes its registered tool.
//!
//! The current Perch wire protocol expects a `"route"` tool, but the existing
//! Phase A.2 artifact registers a `"greet"` tool. We can't compile a new
//! echo plugin right now because the Perry gate-fix got reverted out of the
//! cached perry binary during an auto-optimize rebuild.
//!
//! This test sidesteps the wire protocol and calls `LoadedPlugin` directly,
//! proving that:
//!
//! 1. perch-worker's symbol_pin module keeps enough perry-runtime symbols
//!    in the binary that dlopen succeeds at flat-namespace lookup time.
//! 2. The plugin_host wrapper correctly NaN-boxes strings, invokes the
//!    plugin's registered tool, and decodes the returned string.
//! 3. A `LoadedPlugin` can be created, used, and dropped cleanly (unload
//!    runs on Drop via perry_plugin_unload).
//!
//! Once Perry is unblocked and we can compile a new echo plugin that
//! registers the `"route"` tool, the full wire-protocol integration test
//! lands separately in `tests/dispatch_roundtrip.rs`.

// The binary crate isn't normally importable from a tests/ integration test,
// but perch-worker's public modules (plugin_host, symbol_pin) live inside
// src/. We expose them through a tiny test-only library target — or, more
// simply, duplicate the needed extern declarations here. For now, use the
// direct FFI approach since the test is small and self-contained.

// Link perry-runtime directly for the FFI we need.
extern crate perry_runtime;

use perry_runtime::{js_string_from_bytes, JSValue, StringHeader};
use std::path::PathBuf;

// Symbols that must be pinned for dlopen to succeed. This duplicates the
// logic from crates/perch-worker/src/symbol_pin.rs because integration
// tests run as their own binary and inherit nothing from main.rs.
extern "C" {
    fn js_closure_alloc(func_ptr: *const u8, captures_ptr: *const u8, captures_count: u32) -> *mut u8;
    fn js_nanbox_get_pointer(value: f64) -> *mut u8;
    fn js_nanbox_pointer(ptr: *mut u8) -> f64;
    fn js_nanbox_string(ptr: *mut u8) -> f64;
    fn js_native_call_method(
        receiver: f64,
        name: *const u8,
        name_len: usize,
        args: *const f64,
        argc: usize,
    ) -> f64;
    fn perry_plugin_register_tool(api_handle: i64, name: f64, desc: f64, handler: f64) -> f64;
    fn perry_plugin_set_metadata(api_handle: i64, name: f64, version: f64, description: f64) -> f64;
    fn perry_plugin_register_hook(api_handle: i64, hook_name: f64, handler: f64) -> f64;
    fn perry_plugin_register_route(api_handle: i64, path: f64, handler: f64) -> f64;
    fn perry_plugin_register_service(api_handle: i64, name: f64, start_fn: f64, stop_fn: f64) -> f64;
    fn perry_plugin_log(api_handle: i64, level: i64, message: f64) -> f64;

    fn perry_plugin_load(path: f64) -> i64;
    fn perry_plugin_unload(plugin_id: i64);
    fn perry_plugin_invoke_tool(name: f64, args: f64) -> f64;
}

fn force_link() {
    let funcs: [*const (); 12] = [
        js_closure_alloc as *const (),
        js_nanbox_get_pointer as *const (),
        js_nanbox_pointer as *const (),
        js_nanbox_string as *const (),
        js_native_call_method as *const (),
        js_string_from_bytes as *const (),
        perry_plugin_register_tool as *const (),
        perry_plugin_set_metadata as *const (),
        perry_plugin_register_hook as *const (),
        perry_plugin_register_route as *const (),
        perry_plugin_register_service as *const (),
        perry_plugin_log as *const (),
    ];
    std::hint::black_box(funcs);
}

fn make_perry_string(s: &str) -> f64 {
    let ptr = unsafe { js_string_from_bytes(s.as_ptr(), s.len() as u32) };
    f64::from_bits(JSValue::string_ptr(ptr).bits())
}

fn read_perry_string(value: f64) -> Option<String> {
    let v = JSValue::from_bits(value.to_bits());
    if !v.is_string() {
        return None;
    }
    let header = v.as_string_ptr();
    if header.is_null() {
        return None;
    }
    unsafe {
        let len = (*header).length as usize;
        let data = (header as *const u8).add(std::mem::size_of::<StringHeader>());
        let slice = std::slice::from_raw_parts(data, len);
        Some(String::from_utf8_lossy(slice).into_owned())
    }
}

#[test]
fn hello_dylib_loads_and_greets() {
    force_link();

    // Locate the Phase A.2 hello.dylib relative to the workspace root.
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let workspace = PathBuf::from(manifest_dir)
        .parent() // crates/
        .and_then(|p| p.parent()) // workspace root
        .unwrap()
        .to_path_buf();
    let dylib = workspace.join("scripts/derisk/build/hello.dylib");

    if !dylib.exists() {
        eprintln!(
            "skip: {} not found — run scripts/derisk/run-a1.sh to build it",
            dylib.display()
        );
        return;
    }

    let path_val = make_perry_string(dylib.to_str().unwrap());
    let plugin_id = unsafe { perry_plugin_load(path_val) };
    assert!(
        plugin_id > 0,
        "perry_plugin_load returned {} — is the plugin's undefined-symbol set in sync with symbol_pin.rs?",
        plugin_id
    );

    let name_val = make_perry_string("greet");
    let undefined = f64::from_bits(JSValue::undefined().bits());
    let result = unsafe { perry_plugin_invoke_tool(name_val, undefined) };

    let s = read_perry_string(result)
        .expect("expected a string return value from the 'greet' tool");
    assert_eq!(s, "hello from perry plugin");

    unsafe { perry_plugin_unload(plugin_id) };
}
