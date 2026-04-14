//! Safe wrapper around perry-runtime + libloading for loading and calling
//! Perry-compiled deployment dylibs.
//!
//! ## v0.5 model (current)
//!
//! Perry v0.5 compiles each TS module to a shared library with exported
//! symbols following the pattern `perry_fn_<module>__<export>`. There is
//! no plugin registry; the host dlopens the dylib and calls the exported
//! functions directly by their symbol names.
//!
//! Before calling any user function, the host must:
//! 1. Call `js_gc_init()` to set up Perry's GC + arena
//! 2. Call `__perry_init_strings_<module>()` to init string constants
//! 3. Then call the user function with NaN-boxed arguments
//!
//! The user function signature is `extern "C" fn(f64) -> f64` where both
//! the argument and return are NaN-boxed Perry strings (STRING_TAG).

use anyhow::{anyhow, Context, Result};
use perry_runtime::{js_string_from_bytes, JSValue, StringHeader};
use std::path::Path;
use std::time::{Duration, Instant};

extern "C" {
    fn js_gc_init();
    // Promise primitives — used to await async handler results.
    fn js_promise_state(promise: *mut u8) -> i32; // 0=pending, 1=fulfilled, 2=rejected
    fn js_promise_value(promise: *mut u8) -> f64;
    fn js_promise_reason(promise: *mut u8) -> f64;
    fn js_promise_run_microtasks();
    fn js_run_stdlib_pump();
    fn js_value_is_promise(value: f64) -> i32;
}

/// Maximum time to wait for an async handler's Promise to resolve.
/// Matches the spec's per-invocation wall clock limit (30s).
const PROMISE_AWAIT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to sleep between Perry event-loop pumps when a Promise is
/// still pending. Small enough to keep latency low; large enough to
/// avoid burning a CPU core spin-waiting.
const PROMISE_POLL_INTERVAL: Duration = Duration::from_millis(1);

/// A loaded v0.5 Perry deployment dylib. Uses raw dlopen/dlsym because
/// we need flat-namespace symbol resolution (the dylib's undefined
/// symbols resolve from the host binary which links perry-runtime).
pub struct LoadedPlugin {
    #[allow(dead_code)]
    lib_handle: *mut libc::c_void,
    name: String,
    /// Pointer to the module's string init function.
    init_strings_fn: Option<extern "C" fn()>,
    /// Pointer to the user's `handle` function.
    handle_fn: Option<extern "C" fn(f64) -> f64>,
    /// Whether GC has been initialized (once per process).
    gc_initialized: bool,
}

// SAFETY: the dlopen handle and function pointers are valid for the
// lifetime of the loaded library. We serialize all calls through the
// DeploymentHost's spawn_blocking.
unsafe impl Send for LoadedPlugin {}
unsafe impl Sync for LoadedPlugin {}

impl LoadedPlugin {
    /// Load a Perry v0.5 deployment dylib.
    ///
    /// `module_name` is the sanitized module name used in Perry's symbol
    /// naming convention. For a file `handlers/contact.ts`, Perry produces
    /// symbols like `perry_fn_handlers_contact_ts__handle`. The module
    /// name here would be `handlers_contact_ts`.
    pub fn load(dylib_path: &Path, module_name: &str) -> Result<Self> {
        let path_cstr = std::ffi::CString::new(
            dylib_path
                .to_str()
                .ok_or_else(|| anyhow!("path not UTF-8: {:?}", dylib_path))?,
        )?;

        let handle = unsafe {
            libc::dlopen(path_cstr.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL)
        };
        if handle.is_null() {
            let err = unsafe {
                let e = libc::dlerror();
                if e.is_null() {
                    "unknown".to_string()
                } else {
                    std::ffi::CStr::from_ptr(e)
                        .to_string_lossy()
                        .into_owned()
                }
            };
            return Err(anyhow!("dlopen {:?} failed: {}", dylib_path, err));
        }

        // Perry v0.5 bakes the SOURCE filename (including the .ts
        // extension, sanitized to `_ts`) into every symbol name:
        //   echo-v5.ts → perry_fn_echo_v5_ts__handle
        //   hello.ts   → perry_fn_hello_ts__handle
        //
        // When the dylib is named differently from the source (e.g.
        // compiled from handlers/contact.ts but deployed as hello.dylib),
        // the module_name derived from the dylib stem won't match. We
        // try multiple candidates: the name as-is, with `_ts` appended,
        // and scan nm-style for any `perry_fn_*__handle` export.
        let candidates = [
            module_name.to_string(),
            format!("{}_ts", module_name),
        ];

        // Look up the string init function (may not exist for modules
        // with no string constants).
        let init_strings_fn = unsafe {
            candidates.iter()
                .find_map(|name| {
                    let sym_name = format!("__perry_init_strings_{}", name);
                    dlsym_checked(handle, &sym_name)
                        .map(|ptr| std::mem::transmute::<*mut libc::c_void, extern "C" fn()>(ptr))
                })
        };

        // Look up the handle function, trying each candidate.
        let handle_fn = unsafe {
            candidates.iter()
                .find_map(|name| {
                    let sym_name = format!("perry_fn_{}__handle", name);
                    let found = dlsym_checked(handle, &sym_name);
                    if found.is_some() {
                        tracing::debug!(symbol = %sym_name, "found handle symbol");
                    }
                    found.map(|ptr| std::mem::transmute::<*mut libc::c_void, extern "C" fn(f64) -> f64>(ptr))
                })
        };

        if handle_fn.is_none() {
            let tried: Vec<_> = candidates.iter().map(|n| format!("perry_fn_{}__handle", n)).collect();
            tracing::warn!(
                module = module_name,
                tried = ?tried,
                "no handle symbol found in dylib — deployment won't respond to HTTP dispatch",
            );
        }

        let name = dylib_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("plugin")
            .to_string();

        Ok(Self {
            lib_handle: handle,
            name,
            init_strings_fn,
            handle_fn,
            gc_initialized: false,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Initialize the perry-runtime GC and the module's string constants.
    /// Must be called once before the first `invoke_handle`. Safe to call
    /// multiple times (idempotent).
    pub fn ensure_initialized(&mut self) {
        if !self.gc_initialized {
            unsafe { js_gc_init() };
            self.gc_initialized = true;
        }
        if let Some(init) = self.init_strings_fn {
            init();
            // Only call once — clear the pointer so we don't call again.
            self.init_strings_fn = None;
        }
    }

    /// Call the deployment's `handle(reqJson: string): string | Promise<string>`
    /// function.
    ///
    /// `args` is a Rust string that will be NaN-boxed and passed to the
    /// Perry function. The return value is decoded back to a Rust String.
    ///
    /// Supports both sync and async handler shapes:
    /// - Sync: `function handle(req): string` returns a string directly
    /// - Async: `async function handle(req): Promise<string>` returns a
    ///   Promise; we await it via Perry's promise primitives, driving
    ///   the event loop (microtasks + stdlib pump) until resolved or
    ///   timed out.
    ///
    /// Returns `None` if no `handle` function was found in the dylib
    /// or the resolved value isn't a string.
    pub fn invoke_handle(&mut self, args: &str) -> Result<Option<String>> {
        self.ensure_initialized();

        let handle_fn = match self.handle_fn {
            Some(f) => f,
            None => return Ok(None),
        };

        let args_val = make_perry_string(args);
        let result = handle_fn(args_val);

        // Detect whether the handler returned a string (sync) or a
        // Promise (async). js_value_is_promise inspects the NaN-box tag
        // and the GC type of the underlying allocation — it correctly
        // returns 0 for STRING_TAG, primitives, etc.
        let is_promise = unsafe { js_value_is_promise(result) } != 0;

        if !is_promise {
            // Sync handler — read the string directly.
            return Ok(read_perry_string(result));
        }

        // Async handler — await the Promise.
        let resolved_value = self.await_promise(result)?;
        Ok(read_perry_string(resolved_value))
    }

    /// Drive Perry's event loop until a Promise resolves or times out.
    ///
    /// While holding the plugin lock (DeploymentHost serializes this),
    /// we periodically:
    /// 1. Run any pending microtasks
    /// 2. Pump perry-stdlib's pending work (drains sqlx/redis/reqwest
    ///    completions back into Perry's promise state machine)
    /// 3. Check the Promise's state — if fulfilled, return its value;
    ///    if rejected, return an error; otherwise sleep briefly and retry
    ///
    /// The thread.sleep is necessary because Perry's promise resolution
    /// is signaled via internal channels that we drain via the pump
    /// functions — there's no waker-style notification we could cooperate
    /// with via tokio. The sleep is small (1ms) so it adds negligible
    /// latency for fast operations and only matters for long-running
    /// requests where it dominates over the actual I/O time anyway.
    fn await_promise(&mut self, promise_value: f64) -> Result<f64> {
        // Extract the raw Promise pointer from the NaN-boxed value.
        let promise_ptr = {
            let v = JSValue::from_bits(promise_value.to_bits());
            if !v.is_pointer() {
                return Err(anyhow!(
                    "handler returned non-pointer non-string value: 0x{:016x}",
                    promise_value.to_bits()
                ));
            }
            v.as_pointer::<u8>() as *mut u8
        };

        let start = Instant::now();
        loop {
            unsafe {
                js_promise_run_microtasks();
                js_run_stdlib_pump();
            }

            let state = unsafe { js_promise_state(promise_ptr) };
            match state {
                1 => {
                    // Fulfilled.
                    return Ok(unsafe { js_promise_value(promise_ptr) });
                }
                2 => {
                    // Rejected. Read the rejection reason as a string for
                    // the error message; if it's not a string, we just
                    // include the raw bits.
                    let reason = unsafe { js_promise_reason(promise_ptr) };
                    let reason_str = read_perry_string(reason)
                        .unwrap_or_else(|| format!("0x{:016x}", reason.to_bits()));
                    return Err(anyhow!("handler promise rejected: {}", reason_str));
                }
                _ => {
                    // Still pending.
                    if start.elapsed() > PROMISE_AWAIT_TIMEOUT {
                        return Err(anyhow!(
                            "handler promise timed out after {:?}",
                            PROMISE_AWAIT_TIMEOUT
                        ));
                    }
                    std::thread::sleep(PROMISE_POLL_INTERVAL);
                }
            }
        }
    }
}

impl Drop for LoadedPlugin {
    fn drop(&mut self) {
        if !self.lib_handle.is_null() {
            unsafe { libc::dlclose(self.lib_handle) };
        }
    }
}

/// Safe dlsym wrapper — returns None if the symbol isn't found.
unsafe fn dlsym_checked(handle: *mut libc::c_void, name: &str) -> Option<*mut libc::c_void> {
    let cname = std::ffi::CString::new(name).ok()?;
    let ptr = libc::dlsym(handle, cname.as_ptr());
    if ptr.is_null() {
        None
    } else {
        Some(ptr)
    }
}

// ---------------------------------------------------------------------------
// NaN-box string helpers
// ---------------------------------------------------------------------------

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
        let len = (*header).byte_len as usize;
        let data = (header as *const u8).add(std::mem::size_of::<StringHeader>());
        let slice = std::slice::from_raw_parts(data, len);
        Some(
            std::str::from_utf8(slice)
                .map(str::to_owned)
                .unwrap_or_else(|_| String::from_utf8_lossy(slice).into_owned()),
        )
    }
}

/// Derive the Perry module name from a file path the same way Perry v0.5
/// does: replace non-alphanumeric/underscore chars with `_`, strip leading
/// underscores.
///
/// `handlers/contact.ts` → `handlers_contact_ts`
/// `echo-v5.ts` → `echo_v5_ts`
pub fn module_name_from_path(path: &Path) -> String {
    let s = path
        .to_str()
        .unwrap_or("module")
        .replace(|c: char| !c.is_alphanumeric() && c != '_', "_");
    s.trim_start_matches('_').to_string()
}

/// Convenience: load a deployment dylib with automatic module-name
/// derivation.
pub fn load_deployment(dylib_path: &Path) -> Result<LoadedPlugin> {
    let module_name = module_name_from_path(
        dylib_path.file_stem().map(Path::new).unwrap_or(Path::new("module"))
    );
    LoadedPlugin::load(dylib_path, &module_name)
        .with_context(|| format!("loading deployment dylib {:?} (module={})", dylib_path, module_name))
}
