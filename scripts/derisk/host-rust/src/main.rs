// Phase A.2 — Rust plugin host derisk experiment.
//
// Goal: prove that a normal Rust binary can act as a Perry plugin host —
// linking perry-runtime as a crate, dlopen-ing a Perry-compiled .dylib, and
// invoking the tools the plugin's `activate()` registered.
//
// If this works, the Perch architecture is unblocked: perch-worker is just
// a Rust binary that links perry-runtime, dlopens deployment plugins, and
// dispatches HTTP requests by invoking registered routes/tools.
//
// Pass criterion: this prints "hello from perry plugin" to stdout.

use perry_runtime::{
    js_string_from_bytes, JSValue, StringHeader,
};
use std::env;
use std::process::ExitCode;

// perry_plugin_load and perry_plugin_invoke_tool are #[no_mangle] pub extern "C"
// in perry-runtime/src/plugin.rs but not re-exported via lib.rs. We declare them
// as extern "C" here and let the linker resolve them from the linked rlib.
extern "C" {
    fn perry_plugin_load(path: f64) -> i64;
    fn perry_plugin_unload(plugin_id: i64);
    fn perry_plugin_invoke_tool(name: f64, args: f64) -> f64;

    // The plugin .dylib has these as UNDEFINED symbols. They need to be
    // present in the host binary's symbol table at dlopen time, otherwise
    // dlopen fails with "symbol not found in flat namespace". The Rust
    // linker dead-strips unreferenced perry-runtime symbols, so we must
    // explicitly reference them here. force_link() below holds the pointers
    // through std::hint::black_box so the optimizer can't elide them.
    fn js_closure_alloc(func_ptr: *const u8, captures_ptr: *const u8, captures_count: u32) -> *mut u8;
    fn js_nanbox_get_pointer(value: f64) -> *mut u8;
    fn js_nanbox_pointer(ptr: *mut u8) -> f64;
    fn js_nanbox_string(ptr: *mut u8) -> f64;
    fn js_native_call_method(receiver: f64, name: *const u8, name_len: usize, args: *const f64, argc: usize) -> f64;
    fn perry_plugin_register_tool(api_handle: i64, name: f64, desc: f64, handler: f64) -> f64;
    fn perry_plugin_set_metadata(api_handle: i64, name: f64, version: f64, description: f64) -> f64;
    fn perry_plugin_register_hook(api_handle: i64, hook_name: f64, handler: f64) -> f64;
    fn perry_plugin_register_route(api_handle: i64, path: f64, handler: f64) -> f64;
    fn perry_plugin_register_service(api_handle: i64, name: f64, start_fn: f64, stop_fn: f64) -> f64;
}

/// Force the linker to keep the perry-runtime symbols that Perry plugins
/// reference as undefined. Without this, Cargo's release linker dead-strips
/// them from the host binary, and dlopen of the plugin fails with
/// "symbol not found in flat namespace".
#[inline(never)]
fn force_link_perry_runtime_symbols() {
    let funcs: [*const (); 11] = [
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
    ];
    std::hint::black_box(funcs);
}

/// Build a NaN-boxed Perry string from a Rust &str.
fn make_perry_string(s: &str) -> f64 {
    let ptr = unsafe { js_string_from_bytes(s.as_ptr(), s.len() as u32) };
    f64::from_bits(JSValue::string_ptr(ptr).bits())
}

/// Read the contents of a NaN-boxed Perry string back into a Rust String.
/// Returns None if the value isn't a string.
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

fn main() -> ExitCode {
    force_link_perry_runtime_symbols();

    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
        eprintln!("usage: {} <path-to-plugin.dylib>", args[0]);
        return ExitCode::from(2);
    }
    let plugin_path = &args[1];

    println!("==> loading plugin: {}", plugin_path);
    let path_val = make_perry_string(plugin_path);
    let plugin_id = unsafe { perry_plugin_load(path_val) };

    if plugin_id <= 0 {
        eprintln!("FAIL: perry_plugin_load returned {} (expected > 0)", plugin_id);
        return ExitCode::from(1);
    }
    println!("==> plugin loaded with id={}", plugin_id);

    println!("==> invoking tool 'greet'");
    let name_val = make_perry_string("greet");
    let undefined = f64::from_bits(JSValue::undefined().bits());
    let result = unsafe { perry_plugin_invoke_tool(name_val, undefined) };

    let s = match read_perry_string(result) {
        Some(s) => s,
        None => {
            eprintln!("FAIL: tool result was not a string (raw bits: 0x{:016x})", result.to_bits());
            unsafe { perry_plugin_unload(plugin_id) };
            return ExitCode::from(1);
        }
    };
    println!("==> tool returned: {:?}", s);

    let expected = "hello from perry plugin";
    if s != expected {
        eprintln!("FAIL: expected {:?}, got {:?}", expected, s);
        unsafe { perry_plugin_unload(plugin_id) };
        return ExitCode::from(1);
    }

    println!("==> Phase A.2 PASS — Rust host successfully invoked Perry plugin tool");
    unsafe { perry_plugin_unload(plugin_id) };
    ExitCode::SUCCESS
}
