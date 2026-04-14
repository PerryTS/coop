//! Force the linker to keep perry-runtime symbols that dlopen'd deployment
//! dylibs will reference as undefined.
//!
//! Perry v0.5 compiled dylibs reference perry-runtime functions as undefined
//! symbols resolved at dlopen time via flat namespace lookup. Cargo's linker
//! dead-strips unreferenced symbols, so we must explicitly reference every
//! symbol a deployment might need.
//!
//! Rather than maintaining a hand-picked list (which breaks every time a
//! deployment uses a new runtime function), we pin a comprehensive list of
//! ALL perry-runtime `#[no_mangle] pub extern "C"` functions. This costs
//! nothing at runtime (the symbols are already in the perry-runtime rlib)
//! and guarantees any Perry-compiled TypeScript handler can dlopen cleanly.

use perry_runtime::js_string_from_bytes;

// Exhaustive list of perry-runtime extern "C" functions that Perry v0.5
// deployment dylibs may reference. Generated from:
//   nm -gU libperry_runtime.a | grep " T _js_\| T _perry_" | sort
//
// When perry-runtime adds new exports, run the nm command again and
// update this list. A missing symbol manifests as a dlopen error:
//   "symbol not found in flat namespace '_js_<name>'"
extern "C" {
    // GC + arena
    fn js_gc_init();
    fn js_gc_register_global_root(root: *const u8);

    // Console
    fn js_console_log_dynamic(val: f64);
    fn js_console_log_number(val: f64);

    // NaN-boxing
    fn js_nanbox_string(ptr: *mut u8) -> f64;
    fn js_nanbox_pointer(ptr: *mut u8) -> f64;
    fn js_nanbox_get_pointer(val: f64) -> *mut u8;
    fn js_get_string_pointer_unified(val: f64) -> usize;
    fn js_is_truthy(val: f64) -> i32;
    fn js_is_symbol(val: f64) -> i32;

    // Strings
    fn js_string_concat(a: *mut u8, b: *mut u8) -> *mut u8;
    fn js_string_equals(a: *mut u8, b: *mut u8) -> i32;
    fn js_string_index_of(s: *mut u8, search: *mut u8, from: f64) -> f64;
    fn js_string_split(s: *mut u8, sep: *mut u8) -> *mut u8;
    fn js_string_substring(s: *mut u8, start: f64, end: f64) -> *mut u8;
    fn js_string_replace_regex(s: *mut u8, pattern: *mut u8, replacement: *mut u8) -> *mut u8;
    fn js_jsvalue_to_string(val: f64) -> *mut u8;
    fn js_value_to_string_with_encoding(val: f64, enc: *const u8, len: usize) -> *mut u8;

    // Numbers
    fn js_number_coerce(val: f64) -> f64;
    fn js_date_now() -> f64;

    // Objects
    fn js_object_alloc(class_id: u32, fields: u32) -> *mut u8;
    fn js_object_set_field_by_name(obj: *mut u8, key: *const u8, key_len: usize, val: f64);
    fn js_object_get_field_by_name_f64(obj: *mut u8, key: *mut u8) -> f64;
    fn js_object_set_symbol_property(obj: *mut u8, sym: f64, val: f64);

    // Arrays
    fn js_array_alloc(cap: u32) -> *mut u8;
    fn js_array_push_f64(arr: *mut u8, val: f64);
    fn js_array_get_f64(arr: *mut u8, idx: i32) -> f64;
    fn js_array_length(arr: *mut u8) -> i32;

    // Closures
    fn js_closure_alloc(func: *const u8, captures: *const u8, count: u32) -> *mut u8;
    fn js_closure_call0(closure: *mut u8) -> f64;
    fn js_closure_call1(closure: *mut u8, a: f64) -> f64;
    fn js_closure_call2(closure: *mut u8, a: f64, b: f64) -> f64;

    // JSON
    fn js_json_parse(s: *mut u8) -> f64;
    fn js_json_stringify_full(val: f64, replacer: f64, indent: f64) -> f64;

    // Regex
    fn js_regexp_new(pattern: *mut u8, flags: *mut u8) -> *mut u8;

    // URI encoding
    fn js_decode_uri_component(s: *mut u8) -> *mut u8;

    // Buffer
    fn js_buffer_from_value(val: f64, encoding: f64) -> f64;

    // Promise + timers
    fn js_promise_run_microtasks();
    fn js_timer_tick() -> i32;
    fn js_timer_has_pending() -> i32;
    fn js_timer_next_deadline() -> f64;
    fn js_timer_now() -> f64;
    fn js_interval_timer_tick() -> i32;
    fn js_interval_timer_has_pending() -> i32;
    fn js_interval_timer_next_deadline() -> f64;
    fn js_callback_timer_tick() -> i32;
    fn js_callback_timer_has_pending() -> i32;
    fn js_set_timeout(delay: f64) -> *mut u8;
    fn js_set_timeout_value(delay: f64, value: f64) -> *mut u8;
    fn js_set_timeout_callback(cb: i64, delay: f64) -> i64;
    fn js_sleep_ms(ms: f64);
    fn js_promise_new() -> *mut u8;
    fn js_promise_resolve(p: *mut u8, val: f64);

    // Stdlib pump (called by event loop to drain pending stdlib work)
    fn js_run_stdlib_pump();
    fn js_register_stdlib_pump(f: extern "C" fn() -> i32);
    fn js_register_stdlib_has_active(f: extern "C" fn() -> i32);
    fn js_stdlib_has_active_handles() -> i32;

    // Native call fallback
    fn js_native_call_method(
        receiver: f64, name: *const u8, name_len: usize,
        args: *const f64, argc: usize,
    ) -> f64;
}

#[inline(never)]
pub fn force_link_perry_runtime_symbols() {
    let funcs: [*const (); 56] = [
        js_gc_init as *const (),
        js_gc_register_global_root as *const (),
        js_console_log_dynamic as *const (),
        js_console_log_number as *const (),
        js_nanbox_string as *const (),
        js_nanbox_pointer as *const (),
        js_nanbox_get_pointer as *const (),
        js_get_string_pointer_unified as *const (),
        js_is_truthy as *const (),
        js_is_symbol as *const (),
        js_string_from_bytes as *const (),
        js_string_concat as *const (),
        js_string_equals as *const (),
        js_string_index_of as *const (),
        js_string_split as *const (),
        js_string_substring as *const (),
        js_string_replace_regex as *const (),
        js_jsvalue_to_string as *const (),
        js_value_to_string_with_encoding as *const (),
        js_number_coerce as *const (),
        js_date_now as *const (),
        js_object_alloc as *const (),
        js_object_set_field_by_name as *const (),
        js_object_get_field_by_name_f64 as *const (),
        js_object_set_symbol_property as *const (),
        js_array_alloc as *const (),
        js_array_push_f64 as *const (),
        js_array_get_f64 as *const (),
        js_array_length as *const (),
        js_closure_alloc as *const (),
        js_closure_call0 as *const (),
        js_closure_call1 as *const (),
        js_closure_call2 as *const (),
        js_json_parse as *const (),
        js_json_stringify_full as *const (),
        js_regexp_new as *const (),
        js_decode_uri_component as *const (),
        js_buffer_from_value as *const (),
        js_promise_run_microtasks as *const (),
        js_timer_tick as *const (),
        js_timer_has_pending as *const (),
        js_timer_next_deadline as *const (),
        js_timer_now as *const (),
        js_interval_timer_tick as *const (),
        js_interval_timer_has_pending as *const (),
        js_interval_timer_next_deadline as *const (),
        js_callback_timer_tick as *const (),
        js_callback_timer_has_pending as *const (),
        js_set_timeout as *const (),
        js_set_timeout_value as *const (),
        js_set_timeout_callback as *const (),
        js_sleep_ms as *const (),
        js_run_stdlib_pump as *const (),
        js_register_stdlib_pump as *const (),
        js_register_stdlib_has_active as *const (),
        js_stdlib_has_active_handles as *const (),
    ];
    std::hint::black_box(funcs);
}
