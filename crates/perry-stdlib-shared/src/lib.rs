//! Deployable Perry stdlib shared library.
//!
//! Its final link resolves Perry's stateful API against `libperry_runtime`
//! first. Only Rust generic monomorphizations absent from that dylib may be
//! selected from the runtime rlib, so stdlib and every app use the same
//! process-wide runtime/GC state.
//! rustc's generated export list is removed because it otherwise forces every
//! runtime C symbol out of the fallback archive and creates a second runtime.
//! The build verifies that those stateful symbols remain imported.
//! Runtime exports are interposed onto the process-first provider image.
//! The export surface stays bounded for fast process startup.
//! Packaging is deterministic for a pinned Perry revision.

extern crate perry_stdlib;

use perry_ffi::{read_string, JsString, StringHeader};
use std::cell::Cell;
use std::sync::OnceLock;

extern "C" {
    fn js_gc_init();
}

#[used]
static PIN_STDLIB: extern "C" fn() -> i32 = perry_stdlib::common::js_stdlib_process_pending;

/// Packaging assertion: after runtime-first loading this must equal the
/// `js_gc_init` address returned from `libperry_runtime`.
#[no_mangle]
pub extern "C" fn perch_stdlib_runtime_probe() -> usize {
    js_gc_init as *const () as usize
}

/// Synchronous, host-owned queue enqueue callback. It is installed once by
/// Perch before any application is initialized. The current deployment is a
/// host-assigned opaque ID held in executor TLS, never an application string.
pub type PerchQueueEnqueueCallback = unsafe extern "C" fn(
    deployment_id: u64,
    queue: *const u8,
    queue_len: usize,
    payload: *const u8,
    payload_len: usize,
    delay_ms: u64,
) -> i32;

static QUEUE_ENQUEUE_CALLBACK: OnceLock<PerchQueueEnqueueCallback> = OnceLock::new();

thread_local! {
    static DEPLOYMENT_CONTEXT: Cell<u64> = const { Cell::new(0) };
}

/// Register the process-wide Perch queue gateway. Repeated registration is
/// accepted only for the exact same function pointer.
#[no_mangle]
pub extern "C" fn perch_host_register_queue_enqueue_callback(
    callback: PerchQueueEnqueueCallback,
) -> i32 {
    if let Some(existing) = QUEUE_ENQUEUE_CALLBACK.get() {
        return i32::from(std::ptr::fn_addr_eq(*existing, callback));
    }
    i32::from(QUEUE_ENQUEUE_CALLBACK.set(callback).is_ok())
}

/// Set the opaque deployment identity on one Perry executor thread.
#[no_mangle]
pub extern "C" fn perch_host_set_deployment_context(deployment_id: u64) {
    DEPLOYMENT_CONTEXT.with(|current| current.set(deployment_id));
}

/// Clear the deployment identity before tearing down executor TLS.
#[no_mangle]
pub extern "C" fn perch_host_clear_deployment_context() {
    DEPLOYMENT_CONTEXT.with(|current| current.set(0));
}

/// Provider entry called by the tiny application-linked wrapper. String
/// contents are copied/consumed synchronously before returning.
#[no_mangle]
pub unsafe extern "C" fn js_perch_queue_enqueue(
    queue: *const StringHeader,
    payload: *const StringHeader,
    delay_ms: u64,
) -> i32 {
    let Some(queue) = read_string(JsString::from_raw(queue.cast_mut())) else {
        return -3;
    };
    let Some(payload) = read_string(JsString::from_raw(payload.cast_mut())) else {
        return -4;
    };
    enqueue_bytes(queue.as_bytes(), payload.as_bytes(), delay_ms)
}

/// Raw-buffer variant used by `queue.sendRaw()`. Perry's `buffer+len` native
/// descriptor lowers one Buffer argument to these pointer/length slots, so no
/// JSON or Base64 transform occurs on the application/provider boundary.
#[no_mangle]
pub unsafe extern "C" fn js_perch_queue_enqueue_raw(
    queue: *const StringHeader,
    payload: *const u8,
    payload_len: usize,
    delay_ms: u64,
) -> i32 {
    let Some(queue) = read_string(JsString::from_raw(queue.cast_mut())) else {
        return -3;
    };
    if payload.is_null() && payload_len != 0 {
        return -4;
    }
    let payload = if payload_len == 0 {
        &[]
    } else {
        std::slice::from_raw_parts(payload, payload_len)
    };
    enqueue_bytes(queue.as_bytes(), payload, delay_ms)
}

fn enqueue_bytes(queue: &[u8], payload: &[u8], delay_ms: u64) -> i32 {
    let Some(callback) = QUEUE_ENQUEUE_CALLBACK.get().copied() else {
        return -1;
    };
    let deployment_id = DEPLOYMENT_CONTEXT.with(Cell::get);
    if deployment_id == 0 {
        return -2;
    }
    unsafe {
        callback(
            deployment_id,
            queue.as_ptr(),
            queue.len(),
            payload.as_ptr(),
            payload.len(),
            delay_ms,
        )
    }
}
