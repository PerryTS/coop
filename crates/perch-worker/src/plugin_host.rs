//! Safe wrapper around perry-runtime + libloading for loading and calling
//! Perry-compiled deployment dylibs.
//!
//! ## v0.5 model (current)
//!
//! Perry v0.5 compiles each TS module to a shared library. Perch loads the
//! exact init and HTTP symbols recorded in the required ABI manifest.
//!
//! Before calling any user function, the host must:
//! 1. Call `js_gc_init()` to set up Perry's GC + arena
//! 2. Call `perry_module_init()` to initialize the compiled module
//! 3. Call the user function with a NaN-boxed Perry `Buffer`

use anyhow::{anyhow, Context, Result};
use perch_host_abi::{
    AppLibraryManifest, HandlerAbi, APP_LIBRARY_ABI_VERSION, APP_LIBRARY_BOUNDARY_VERSION,
    MAX_FRAME_SIZE,
};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::Path;
use std::time::{Duration, Instant};

const POINTER_TAG: u64 = 0x7FFD_0000_0000_0000;
const POINTER_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

#[repr(C)]
struct StringHeader {
    utf16_len: u32,
    byte_len: u32,
    capacity: u32,
    refcount: u32,
    flags: u32,
}

/// Maximum time to wait for an async handler's Promise to resolve.
/// Matches the spec's per-invocation wall clock limit (30s).
const PROMISE_AWAIT_TIMEOUT: Duration = Duration::from_secs(30);

/// A loaded v0.5 Perry deployment dylib. Uses raw dlopen/dlsym because
/// the app's undefined Perry symbols resolve from the process-wide runtime
/// and stdlib provider libraries loaded before this point.
/// Either ABI shape Perry can emit for the user's `handle` function.
#[derive(Clone, Copy)]
pub enum HandleFn {
    /// Bare user function: `extern "C" fn(f64) -> f64`. Older Perry, or
    /// the post-link version after wrapper inlining.
    Bare(extern "C" fn(f64) -> f64),
    /// Closure-ABI wrapper: `extern "C" fn(i64 closure, f64 arg) -> f64`.
    /// Pass closure=0 (no captured environment).
    Wrapped(extern "C" fn(i64, f64) -> f64),
}

impl HandleFn {
    fn call(&self, arg: f64) -> f64 {
        match self {
            HandleFn::Bare(f) => f(arg),
            HandleFn::Wrapped(f) => f(0, arg),
        }
    }
}

pub struct LoadedPlugin {
    #[allow(dead_code)]
    lib_handle: *mut libc::c_void,
    name: String,
    /// `perry_module_init` (Perry v0.5.57+): registers module-level GC
    /// roots and runs top-level statements. Must be called once after
    /// string init and before any user function.
    module_init_fn: Option<extern "C" fn()>,
    /// Required raw Buffer HTTP entry point.
    handle_fn: HandleFn,
    cron_handlers: HashMap<String, HandleFn>,
    queue_handlers: HashMap<String, HandleFn>,
    /// Whether GC has been initialized (once per process).
    gc_initialized: bool,
}

impl LoadedPlugin {
    /// Load a Perry v0.5 deployment dylib.
    ///
    /// `module_name` is the sanitized module name used in Perry's symbol
    /// naming convention. For a file `handlers/contact.ts`, Perry produces
    /// symbols like `perry_fn_handlers_contact_ts__handle`. The module
    /// name here would be `handlers_contact_ts`.
    pub fn load(dylib_path: &Path, _module_name: &str) -> Result<Self> {
        let total_started = Instant::now();
        let manifest_started = Instant::now();
        let manifest = AppLibraryManifest::load(dylib_path)
            .with_context(|| format!("reading app-library manifest for {:?}", dylib_path))?
            .ok_or_else(|| {
                anyhow!(
                    "app library {:?} has no ABI manifest; recompile it with Perch",
                    dylib_path
                )
            })?;
        let manifest_ms = manifest_started.elapsed().as_secs_f64() * 1_000.0;
        let boundary_started = Instant::now();
        validate_manifest(&manifest, dylib_path)?;
        let used_cached_boundary = validate_cached_boundary(&manifest, dylib_path)?;
        if !used_cached_boundary {
            validate_library_boundary(dylib_path)?;
        }
        let boundary_ms = boundary_started.elapsed().as_secs_f64() * 1_000.0;

        let path_cstr = std::ffi::CString::new(
            dylib_path
                .to_str()
                .ok_or_else(|| anyhow!("path not UTF-8: {:?}", dylib_path))?,
        )?;

        let dlopen_started = Instant::now();
        let handle = unsafe { libc::dlopen(path_cstr.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if handle.is_null() {
            let err = unsafe {
                let e = libc::dlerror();
                if e.is_null() {
                    "unknown".to_string()
                } else {
                    std::ffi::CStr::from_ptr(e).to_string_lossy().into_owned()
                }
            };
            return Err(anyhow!("dlopen {:?} failed: {}", dylib_path, err));
        }
        let dlopen_ms = dlopen_started.elapsed().as_secs_f64() * 1_000.0;
        let symbols_started = Instant::now();

        let handle_fn = match unsafe {
            load_handle_fn(handle, &manifest.handle_symbol, manifest.handler_abi)
        } {
            Ok(function) => function,
            Err(error) => {
                unsafe { libc::dlclose(handle) };
                return Err(error).with_context(|| {
                    format!("binding HTTP entry from application {:?}", dylib_path)
                });
            }
        };
        let cron_handlers = match manifest
            .cron_entries
            .iter()
            .map(|entry| unsafe {
                load_handle_fn(handle, &entry.symbol, entry.handler_abi)
                    .map(|function| (entry.expression.clone(), function))
            })
            .collect::<Result<HashMap<_, _>>>()
        {
            Ok(handlers) => handlers,
            Err(error) => {
                unsafe { libc::dlclose(handle) };
                return Err(error).with_context(|| {
                    format!("binding cron entries from application {:?}", dylib_path)
                });
            }
        };
        let queue_handlers = match manifest
            .queue_entries
            .iter()
            .map(|entry| unsafe {
                load_handle_fn(handle, &entry.symbol, entry.handler_abi)
                    .map(|function| (entry.queue_name.clone(), function))
            })
            .collect::<Result<HashMap<_, _>>>()
        {
            Ok(handlers) => handlers,
            Err(error) => {
                unsafe { libc::dlclose(handle) };
                return Err(error).with_context(|| {
                    format!("binding queue entries from application {:?}", dylib_path)
                });
            }
        };

        let name = dylib_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("plugin")
            .to_string();

        // Look up perry_module_init (v0.5.57+ emits this for dylibs).
        let init_symbol = manifest.init_symbol.as_str();
        let module_init_fn = unsafe {
            dlsym_checked(handle, init_symbol)
                .map(|ptr| std::mem::transmute::<*mut libc::c_void, extern "C" fn()>(ptr))
        };
        if module_init_fn.is_none() {
            unsafe { libc::dlclose(handle) };
            return Err(anyhow!(
                "manifest init symbol {:?} is missing from {:?}",
                init_symbol,
                dylib_path
            ));
        }
        if module_init_fn.is_some() {
            tracing::debug!("found perry_module_init");
        }

        tracing::info!(
            dylib = %dylib_path.display(),
            manifest_ms,
            boundary_ms,
            cached_boundary = used_cached_boundary,
            dlopen_ms,
            symbol_bind_ms = symbols_started.elapsed().as_secs_f64() * 1_000.0,
            total_ms = total_started.elapsed().as_secs_f64() * 1_000.0,
            "application library load phases complete"
        );

        Ok(Self {
            lib_handle: handle,
            name,
            module_init_fn,
            handle_fn,
            cron_handlers,
            queue_handlers,
            gc_initialized: false,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Initialize the Perry GC and compiled module. Must be called once before
    /// the first HTTP invocation. Safe to call multiple times (idempotent).
    pub fn ensure_initialized(&mut self) {
        if !self.gc_initialized {
            unsafe { (crate::runtime_libraries::runtime_api().js_gc_init)() };
            self.gc_initialized = true;
        }
        // Call perry_module_init if present — Perry v0.5.57+ emits this
        // for dylibs to register module-level GC roots and run top-level
        // statements (e.g. const CONN_STATES = new Map(), let counter = 0).
        // Without this, module-level state is uninitialized and user
        // functions return 0/null.
        if let Some(init_mod) = self.module_init_fn.take() {
            tracing::debug!("calling perry_module_init");
            init_mod();
        }
    }

    /// Invoke the required compact HTTP ABI. Input and output stay as raw
    /// Perry `Buffer` values.
    pub fn invoke_http(&mut self, frame: &[u8]) -> Result<Vec<u8>> {
        self.invoke_buffer_handler(self.handle_fn, frame, "HTTP")
    }

    pub fn invoke_cron(&mut self, expression: &str, frame: &[u8]) -> Result<Vec<u8>> {
        let handler = self
            .cron_handlers
            .get(expression)
            .copied()
            .ok_or_else(|| anyhow!("application has no cron entry for {expression:?}"))?;
        self.invoke_buffer_handler(handler, frame, "cron")
    }

    pub fn invoke_queue(&mut self, queue_name: &str, frame: &[u8]) -> Result<Vec<u8>> {
        let handler = self
            .queue_handlers
            .get(queue_name)
            .copied()
            .ok_or_else(|| anyhow!("application has no queue entry for {queue_name:?}"))?;
        self.invoke_buffer_handler(handler, frame, "queue")
    }

    fn invoke_buffer_handler(
        &mut self,
        handler: HandleFn,
        frame: &[u8],
        entry_kind: &str,
    ) -> Result<Vec<u8>> {
        self.ensure_initialized();
        if frame.len() > MAX_FRAME_SIZE {
            return Err(anyhow!(
                "{entry_kind} request frame exceeds {MAX_FRAME_SIZE} bytes"
            ));
        }

        let argument = make_perry_buffer(frame)?;
        let result = self.resolve_handler_result(handler.call(argument))?;
        let api = crate::runtime_libraries::runtime_api();
        let data = unsafe { (api.js_native_buffer_data_ptr)(result) };
        let len = unsafe { (api.js_native_buffer_byte_len)(result) };
        if data.is_null() {
            return Err(anyhow!(
                "application {entry_kind} handler returned a non-Buffer"
            ));
        }
        if len > MAX_FRAME_SIZE {
            return Err(anyhow!(
                "{entry_kind} response frame is {len} bytes (max {MAX_FRAME_SIZE})"
            ));
        }
        Ok(unsafe { std::slice::from_raw_parts(data, len) }.to_vec())
    }

    fn resolve_handler_result(&mut self, result: f64) -> Result<f64> {
        // Detect whether the handler returned a Buffer synchronously or a
        // Promise. The runtime helper inspects the NaN-box tag and GC type.
        let api = crate::runtime_libraries::runtime_api();
        let is_promise = unsafe { (api.js_value_is_promise)(result) } != 0;

        if !is_promise {
            return Ok(result);
        }

        // Async handler — await the Promise.
        self.await_promise(result)
    }

    /// Drive Perry's event loop until a Promise resolves or times out.
    ///
    /// This runs on the app's dedicated Perry thread. `perry_poll` drains all
    /// ready JS/stdlib work and `js_wait_for_event` blocks on Perry's native
    /// wake primitive until I/O or the next timer is ready. There is no fixed
    /// polling quantum on the request path.
    fn await_promise(&mut self, promise_value: f64) -> Result<f64> {
        // Extract the raw Promise pointer from the NaN-boxed value.
        let promise_ptr = {
            let bits = promise_value.to_bits();
            if (bits & !POINTER_MASK) != POINTER_TAG {
                return Err(anyhow!(
                    "handler returned non-pointer non-string value: 0x{:016x}",
                    bits
                ));
            }
            (bits & POINTER_MASK) as usize as *mut u8
        };

        let api = crate::runtime_libraries::runtime_api();
        let start = Instant::now();
        loop {
            unsafe {
                let _ = (api.perry_poll)();
            }

            let state = unsafe { (api.js_promise_state)(promise_ptr) };
            match state {
                1 => {
                    // Fulfilled.
                    return Ok(unsafe { (api.js_promise_value)(promise_ptr) });
                }
                2 => {
                    // Rejected. Read the rejection reason as a string for
                    // the error message; if it's not a string, we just
                    // include the raw bits.
                    let reason = unsafe { (api.js_promise_reason)(promise_ptr) };
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
                    unsafe { (api.js_wait_for_event)() };
                }
            }
        }
    }
}

fn validate_manifest(manifest: &AppLibraryManifest, dylib_path: &Path) -> Result<()> {
    if manifest.abi_version != APP_LIBRARY_ABI_VERSION {
        return Err(anyhow!(
            "app-library ABI mismatch for {:?}: library={}, host={}",
            dylib_path,
            manifest.abi_version,
            APP_LIBRARY_ABI_VERSION
        ));
    }
    if manifest.perry_version != crate::PERRY_RUNTIME_VERSION {
        return Err(anyhow!(
            "Perry ABI mismatch for {:?}: compiled with {}, host runtime is {}",
            dylib_path,
            manifest.perry_version,
            crate::PERRY_RUNTIME_VERSION
        ));
    }
    if manifest.perry_commit != crate::PERRY_RUNTIME_COMMIT {
        return Err(anyhow!(
            "app library Perry source mismatch: app={}, host={}",
            manifest.perry_commit,
            crate::PERRY_RUNTIME_COMMIT
        ));
    }
    let provider = crate::perry_provider_identity();
    if manifest.compiler_sha256 != provider.compiler_sha256 {
        return Err(anyhow!(
            "app library compiler mismatch: app={}, provider={}",
            manifest.compiler_sha256,
            provider.compiler_sha256
        ));
    }
    let host_target = format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS);
    if manifest.target != host_target {
        return Err(anyhow!(
            "target mismatch for {:?}: library={}, host={}",
            dylib_path,
            manifest.target,
            host_target
        ));
    }
    let mut cron_expressions = HashSet::new();
    for entry in &manifest.cron_entries {
        if entry.expression.is_empty()
            || entry.symbol.is_empty()
            || !cron_expressions.insert(entry.expression.as_str())
        {
            return Err(anyhow!(
                "app library {:?} has an empty or duplicate cron entry",
                dylib_path
            ));
        }
    }
    let mut queue_names = HashSet::new();
    for entry in &manifest.queue_entries {
        if entry.queue_name.is_empty()
            || entry.symbol.is_empty()
            || !queue_names.insert(entry.queue_name.as_str())
        {
            return Err(anyhow!(
                "app library {:?} has an empty or duplicate queue entry",
                dylib_path
            ));
        }
    }
    Ok(())
}

fn validate_cached_boundary(manifest: &AppLibraryManifest, dylib_path: &Path) -> Result<bool> {
    if !manifest.boundary_verified
        || manifest.boundary_verification_version != APP_LIBRARY_BOUNDARY_VERSION
    {
        return Ok(false);
    }
    let (Some(expected_sha), Some(expected_size)) =
        (manifest.library_sha256.as_deref(), manifest.library_size)
    else {
        return Ok(false);
    };
    let actual_size = std::fs::metadata(dylib_path)
        .with_context(|| format!("reading metadata for {:?}", dylib_path))?
        .len();
    if actual_size != expected_size {
        return Err(anyhow!(
            "verified app library {:?} changed size: manifest={}, actual={}",
            dylib_path,
            expected_size,
            actual_size
        ));
    }
    let actual_sha = library_sha256(dylib_path)?;
    if actual_sha != expected_sha {
        return Err(anyhow!(
            "verified app library {:?} failed its SHA-256 integrity check",
            dylib_path
        ));
    }
    Ok(true)
}

/// Hash an application artifact so its expensive boundary audit can be
/// performed once at deployment time and proven at every subsequent load.
pub fn library_sha256(path: &Path) -> Result<String> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {:?}", path))?;
    let mut reader = std::io::BufReader::with_capacity(1024 * 1024, file);
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let count = reader
            .read(&mut buffer)
            .with_context(|| format!("hashing {:?}", path))?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

/// Verify that an app library does not define code owned by the separately
/// packaged Perry providers. A dynamic dependency on those files is allowed:
/// it gives dyld a two-level namespace without embedding provider code.
/// The daemon calls this immediately after compile; unverified artifacts are
/// checked once when loaded.
pub fn validate_library_boundary(dylib_path: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    let dependencies = std::process::Command::new("otool")
        .arg("-L")
        .arg(dylib_path)
        .output();
    #[cfg(not(target_os = "macos"))]
    let dependencies = std::process::Command::new("readelf")
        .arg("-d")
        .arg(dylib_path)
        .output();

    let dependencies =
        dependencies.with_context(|| format!("inspecting dependencies of {:?}", dylib_path))?;
    if !dependencies.status.success() {
        return Err(anyhow!(
            "could not inspect dependencies of {:?}: {}",
            dylib_path,
            String::from_utf8_lossy(&dependencies.stderr).trim()
        ));
    }
    validate_provider_dependencies(dylib_path, &dependencies.stdout)?;
    let mut nm = std::process::Command::new("nm");
    #[cfg(target_os = "macos")]
    nm.arg("-gU");
    #[cfg(not(target_os = "macos"))]
    nm.args(["-D", "--defined-only"]);
    let symbols = nm
        .arg(dylib_path)
        .output()
        .with_context(|| format!("inspecting exports of {:?}", dylib_path))?;
    if !symbols.status.success() {
        return Err(anyhow!(
            "could not inspect exports of {:?}: {}",
            dylib_path,
            String::from_utf8_lossy(&symbols.stderr).trim()
        ));
    }

    for line in String::from_utf8_lossy(&symbols.stdout).lines() {
        let Some(raw_symbol) = line.split_whitespace().last() else {
            continue;
        };
        #[cfg(target_os = "macos")]
        let symbol = raw_symbol.strip_prefix('_').unwrap_or(raw_symbol);
        #[cfg(not(target_os = "macos"))]
        let symbol = raw_symbol;
        if (symbol.starts_with("js_") || symbol.starts_with("perry_"))
            && crate::runtime_libraries::provider_exports_symbol(symbol)
        {
            return Err(anyhow!(
                "app library {:?} defines provider symbol {:?}; runtime and stdlib must remain separate",
                dylib_path,
                symbol
            ));
        }
    }
    Ok(())
}

fn validate_provider_dependencies(dylib_path: &Path, output: &[u8]) -> Result<()> {
    let output = String::from_utf8_lossy(output);
    #[cfg(target_os = "macos")]
    let required = [
        "@rpath/libperry_runtime.dylib",
        "@rpath/libperry_stdlib.dylib",
    ];
    #[cfg(not(target_os = "macos"))]
    let required = [
        "Shared library: [libperry_runtime.so]",
        "Shared library: [libperry_stdlib.so]",
    ];
    for dependency in required {
        if !output.contains(dependency) {
            return Err(anyhow!(
                "app library {:?} is missing required provider dependency {:?}",
                dylib_path,
                dependency
            ));
        }
    }
    Ok(())
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

unsafe fn load_handle_fn(
    handle: *mut libc::c_void,
    symbol: &str,
    abi: HandlerAbi,
) -> Result<HandleFn> {
    let pointer = dlsym_checked(handle, symbol)
        .ok_or_else(|| anyhow!("manifest entry symbol {symbol:?} is missing"))?;
    Ok(match abi {
        HandlerAbi::Bare => HandleFn::Bare(std::mem::transmute::<
            *mut libc::c_void,
            extern "C" fn(f64) -> f64,
        >(pointer)),
        HandlerAbi::Wrapped => HandleFn::Wrapped(std::mem::transmute::<
            *mut libc::c_void,
            extern "C" fn(i64, f64) -> f64,
        >(pointer)),
    })
}

// ---------------------------------------------------------------------------
// NaN-box value helpers
// ---------------------------------------------------------------------------

fn make_perry_buffer(bytes: &[u8]) -> Result<f64> {
    let len = i32::try_from(bytes.len()).map_err(|_| anyhow!("Perry Buffer input is too large"))?;
    let api = crate::runtime_libraries::runtime_api();
    let header = unsafe { (api.js_buffer_alloc)(len, 0) };
    if header.is_null() {
        return Err(anyhow!("Perry Buffer allocation failed"));
    }
    let value = f64::from_bits(POINTER_TAG | (header as u64 & POINTER_MASK));
    let data = unsafe { (api.js_native_buffer_data_ptr)(value) };
    if data.is_null() && !bytes.is_empty() {
        return Err(anyhow!("Perry Buffer allocation returned no data pointer"));
    }
    if !bytes.is_empty() {
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), data, bytes.len()) };
    }
    Ok(value)
}

fn read_perry_string(value: f64) -> Option<String> {
    // js_get_string_pointer_unified handles both heap-allocated strings
    // (STRING_TAG) and inline SSO strings (SHORT_STRING_TAG) by
    // materializing the latter to the heap. Returns 0 if not a string.
    let header_ptr =
        unsafe { (crate::runtime_libraries::runtime_api().js_get_string_pointer_unified)(value) }
            as *const StringHeader;
    if header_ptr.is_null() {
        return None;
    }
    unsafe {
        let len = (*header_ptr).byte_len as usize;
        let data = (header_ptr as *const u8).add(std::mem::size_of::<StringHeader>());
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
        dylib_path
            .file_stem()
            .map(Path::new)
            .unwrap_or(Path::new("module")),
    );
    LoadedPlugin::load(dylib_path, &module_name).with_context(|| {
        format!(
            "loading deployment dylib {:?} (module={})",
            dylib_path, module_name
        )
    })
}

// Tests live at the end of the file: clippy's `items_after_test_module` fires
// when definitions follow the `#[cfg(test)]` module, and a reader scanning for
// production code should not have to page past them.
#[cfg(test)]
mod boundary_tests {
    use super::validate_provider_dependencies;
    use std::path::Path;

    #[test]
    fn provider_dependency_audit_requires_runtime_and_stdlib() {
        #[cfg(target_os = "macos")]
        let (both, runtime_only) = (
            b"@rpath/libperry_runtime.dylib\n@rpath/libperry_stdlib.dylib".as_slice(),
            b"@rpath/libperry_runtime.dylib".as_slice(),
        );
        #[cfg(not(target_os = "macos"))]
        let (both, runtime_only) = (
            b"Shared library: [libperry_runtime.so]\nShared library: [libperry_stdlib.so]"
                .as_slice(),
            b"Shared library: [libperry_runtime.so]".as_slice(),
        );

        validate_provider_dependencies(Path::new("app"), both).unwrap();
        let error = validate_provider_dependencies(Path::new("app"), runtime_only).unwrap_err();
        assert!(error.to_string().contains("libperry_stdlib"));
    }
}
