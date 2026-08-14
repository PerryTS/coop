//! Process-wide loader for Perry's separately packaged runtime and stdlib.
//!
//! Application dylibs deliberately leave Perry symbols undefined. The host
//! loads these two provider libraries with `RTLD_GLOBAL` before any app is
//! opened, then calls the small runtime surface below through resolved function
//! pointers. Consequently neither `perch` nor `perch-worker` statically embeds
//! Perry.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Instant;

pub(crate) type JsGcInit = unsafe extern "C" fn();
pub(crate) type JsPromiseState = unsafe extern "C" fn(*mut u8) -> i32;
pub(crate) type JsPromiseValue = unsafe extern "C" fn(*mut u8) -> f64;
pub(crate) type PerryPoll = unsafe extern "C" fn() -> i32;
pub(crate) type JsWaitForEvent = unsafe extern "C" fn();
pub(crate) type JsValueIsPromise = unsafe extern "C" fn(f64) -> i32;
pub(crate) type JsGetStringPointerUnified = unsafe extern "C" fn(f64) -> i64;
pub(crate) type JsBufferAlloc = unsafe extern "C" fn(i32, i32) -> *mut libc::c_void;
pub(crate) type JsNativeBufferDataPtr = unsafe extern "C" fn(f64) -> *mut u8;
pub(crate) type JsNativeBufferByteLen = unsafe extern "C" fn(f64) -> usize;
pub(crate) type JsArenaStats = unsafe extern "C" fn(*mut u64, *mut u64);
pub(crate) type JsGcMemoryPressure = unsafe extern "C" fn(u32) -> u32;
pub(crate) type JsGcReleaseCurrentThreadCollectionSideAllocations = unsafe extern "C" fn();
pub type PerchQueueEnqueueCallback =
    unsafe extern "C" fn(u64, *const u8, usize, *const u8, usize, u64) -> i32;
pub(crate) type PerchRegisterQueueEnqueueCallback =
    unsafe extern "C" fn(PerchQueueEnqueueCallback) -> i32;
pub(crate) type PerchSetDeploymentContext = unsafe extern "C" fn(u64);
pub(crate) type PerchClearDeploymentContext = unsafe extern "C" fn();

pub(crate) struct RuntimeApi {
    pub js_gc_init: JsGcInit,
    pub js_promise_state: JsPromiseState,
    pub js_promise_value: JsPromiseValue,
    pub js_promise_reason: JsPromiseValue,
    pub perry_poll: PerryPoll,
    pub js_wait_for_event: JsWaitForEvent,
    pub js_value_is_promise: JsValueIsPromise,
    pub js_get_string_pointer_unified: JsGetStringPointerUnified,
    pub js_buffer_alloc: JsBufferAlloc,
    pub js_native_buffer_data_ptr: JsNativeBufferDataPtr,
    pub js_native_buffer_byte_len: JsNativeBufferByteLen,
    pub js_arena_stats: JsArenaStats,
    pub js_gc_memory_pressure: JsGcMemoryPressure,
    pub js_gc_release_current_thread_collection_side_allocations:
        JsGcReleaseCurrentThreadCollectionSideAllocations,
    pub perch_host_register_queue_enqueue_callback: PerchRegisterQueueEnqueueCallback,
    pub perch_host_set_deployment_context: PerchSetDeploymentContext,
    pub perch_host_clear_deployment_context: PerchClearDeploymentContext,
}

/// Kept for the process lifetime. Closing either provider while applications
/// are loaded would invalidate their relocations.
struct RuntimeLibraries {
    runtime_handle: usize,
    stdlib_handle: usize,
    runtime_path: PathBuf,
    stdlib_path: PathBuf,
    verification: ProviderVerification,
    identity: PerryProviderIdentity,
    api: RuntimeApi,
}

static LIBRARIES: OnceLock<RuntimeLibraries> = OnceLock::new();

#[derive(Deserialize)]
struct LibrariesManifest {
    manifest_version: u32,
    perry_version: String,
    perry_commit: String,
    compiler_sha256: String,
    rustc_version: String,
    target: String,
    arena_block_size_bytes: u64,
    allocator: String,
    runtime_file: String,
    runtime_sha256: String,
    runtime_size: u64,
    stdlib_file: String,
    stdlib_sha256: String,
    stdlib_size: u64,
}

#[derive(Debug)]
pub struct PerryProviderIdentity {
    pub perry_version: String,
    pub perry_commit: String,
    pub compiler_sha256: String,
    pub runtime_sha256: String,
    pub stdlib_sha256: String,
    pub rustc_version: String,
    pub target: String,
    pub arena_block_size_bytes: u64,
    pub allocator: String,
}

/// Integrity boundary used before opening the process-wide provider pair.
/// Full hashing is the portable default. A production package may skip the
/// repeated 95-MiB read only when the complete canonical path is protected by
/// a root-owned, non-writable filesystem boundary and Perch runs unprivileged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderVerification {
    FullHash,
    RootOwnedImmutable,
}

impl ProviderVerification {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FullHash => "full_hash",
            Self::RootOwnedImmutable => "root_owned_immutable",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "full_hash" => Ok(Self::FullHash),
            "root_owned_immutable" => Ok(Self::RootOwnedImmutable),
            _ => Err(anyhow!(
                "unknown Perry provider verification mode {value:?}"
            )),
        }
    }
}

/// Load the exact Perry runtime/stdlib pair before any application library.
/// Repeated calls are accepted only for the same canonical paths.
pub fn initialize_runtime_libraries(runtime_path: &Path, stdlib_path: &Path) -> Result<()> {
    initialize_runtime_libraries_with_verification(
        runtime_path,
        stdlib_path,
        ProviderVerification::FullHash,
    )
}

pub fn initialize_runtime_libraries_with_verification(
    runtime_path: &Path,
    stdlib_path: &Path,
    verification: ProviderVerification,
) -> Result<()> {
    let total_started = Instant::now();
    let runtime_path = runtime_path
        .canonicalize()
        .with_context(|| format!("locating Perry runtime {}", runtime_path.display()))?;
    let stdlib_path = stdlib_path
        .canonicalize()
        .with_context(|| format!("locating Perry stdlib {}", stdlib_path.display()))?;

    if let Some(existing) = LIBRARIES.get() {
        return ensure_same_paths(existing, &runtime_path, &stdlib_path, verification);
    }

    let manifest_started = Instant::now();
    let manifest = load_and_validate_manifest(&runtime_path, &stdlib_path, verification)?;
    let manifest_ms = manifest_started.elapsed().as_secs_f64() * 1_000.0;

    let runtime_started = Instant::now();
    let runtime_handle = open_global(&runtime_path)
        .with_context(|| format!("loading Perry runtime {}", runtime_path.display()))?;
    let runtime_ms = runtime_started.elapsed().as_secs_f64() * 1_000.0;
    let stdlib_started = Instant::now();
    let stdlib_handle = match open_global(&stdlib_path) {
        Ok(handle) => handle,
        Err(error) => {
            unsafe { libc::dlclose(runtime_handle) };
            return Err(error)
                .with_context(|| format!("loading Perry stdlib {}", stdlib_path.display()));
        }
    };
    let stdlib_ms = stdlib_started.elapsed().as_secs_f64() * 1_000.0;

    let symbols_started = Instant::now();
    let loaded = unsafe {
        (|| -> Result<RuntimeLibraries> {
            let api = RuntimeApi {
                js_gc_init: load_symbol(runtime_handle, "js_gc_init")?,
                js_promise_state: load_symbol(runtime_handle, "js_promise_state")?,
                js_promise_value: load_symbol(runtime_handle, "js_promise_value")?,
                js_promise_reason: load_symbol(runtime_handle, "js_promise_reason")?,
                perry_poll: load_symbol(runtime_handle, "perry_poll")?,
                js_wait_for_event: load_symbol(runtime_handle, "js_wait_for_event")?,
                js_value_is_promise: load_symbol(runtime_handle, "js_value_is_promise")?,
                js_get_string_pointer_unified: load_symbol(
                    runtime_handle,
                    "js_get_string_pointer_unified",
                )?,
                js_buffer_alloc: load_symbol(runtime_handle, "js_buffer_alloc")?,
                js_native_buffer_data_ptr: load_symbol(
                    runtime_handle,
                    "js_native_buffer_data_ptr",
                )?,
                js_native_buffer_byte_len: load_symbol(
                    runtime_handle,
                    "js_native_buffer_byte_len",
                )?,
                js_arena_stats: load_symbol(runtime_handle, "js_arena_stats")?,
                js_gc_memory_pressure: load_symbol(runtime_handle, "js_gc_memory_pressure")?,
                js_gc_release_current_thread_collection_side_allocations: load_symbol(
                    runtime_handle,
                    "js_gc_release_current_thread_collection_side_allocations",
                )?,
                perch_host_register_queue_enqueue_callback: load_symbol(
                    stdlib_handle,
                    "perch_host_register_queue_enqueue_callback",
                )?,
                perch_host_set_deployment_context: load_symbol(
                    stdlib_handle,
                    "perch_host_set_deployment_context",
                )?,
                perch_host_clear_deployment_context: load_symbol(
                    stdlib_handle,
                    "perch_host_clear_deployment_context",
                )?,
            };

            // This symbol pins the stdlib entry surface and proves that the
            // stdlib's runtime calls bind to the already-loaded runtime image.
            let _: unsafe extern "C" fn() -> i32 =
                load_symbol(stdlib_handle, "js_stdlib_process_pending")?;
            let probe: unsafe extern "C" fn() -> usize =
                load_symbol(stdlib_handle, "perch_stdlib_runtime_probe")?;
            let expected = api.js_gc_init as *const () as usize;
            let actual = probe();
            if actual != expected {
                return Err(anyhow!(
                    "Perry stdlib is bound to a different runtime image (expected {expected:#x}, got {actual:#x})"
                ));
            }

            Ok(RuntimeLibraries {
                runtime_handle: runtime_handle as usize,
                stdlib_handle: stdlib_handle as usize,
                runtime_path: runtime_path.clone(),
                stdlib_path: stdlib_path.clone(),
                verification,
                identity: PerryProviderIdentity {
                    perry_version: manifest.perry_version.clone(),
                    perry_commit: manifest.perry_commit.clone(),
                    compiler_sha256: manifest.compiler_sha256.clone(),
                    runtime_sha256: manifest.runtime_sha256.clone(),
                    stdlib_sha256: manifest.stdlib_sha256.clone(),
                    rustc_version: manifest.rustc_version.clone(),
                    target: manifest.target.clone(),
                    arena_block_size_bytes: manifest.arena_block_size_bytes,
                    allocator: manifest.allocator.clone(),
                },
                api,
            })
        })()
    };
    let symbols_ms = symbols_started.elapsed().as_secs_f64() * 1_000.0;

    let loaded = match loaded {
        Ok(loaded) => loaded,
        Err(error) => {
            unsafe {
                libc::dlclose(stdlib_handle);
                libc::dlclose(runtime_handle);
            }
            return Err(error);
        }
    };

    if LIBRARIES.set(loaded).is_err() {
        // A racing initializer won. Close our duplicate handles and validate
        // that both callers requested the same provider pair.
        unsafe {
            libc::dlclose(stdlib_handle);
            libc::dlclose(runtime_handle);
        }
        let existing = LIBRARIES.get().expect("runtime initializer won race");
        return ensure_same_paths(existing, &runtime_path, &stdlib_path, verification);
    }

    tracing::info!(
        runtime = %runtime_path.display(),
        stdlib = %stdlib_path.display(),
        provider_verification = verification.as_str(),
        perry_version = crate::PERRY_RUNTIME_VERSION,
        perry_commit = %manifest.perry_commit,
        rustc = %manifest.rustc_version,
        manifest_ms,
        runtime_dlopen_ms = runtime_ms,
        stdlib_dlopen_ms = stdlib_ms,
        symbol_bind_ms = symbols_ms,
        total_ms = total_started.elapsed().as_secs_f64() * 1_000.0,
        rss_kib = crate::process_rss_kib(),
        "separate Perry runtime and stdlib loaded globally"
    );
    Ok(())
}

fn load_and_validate_manifest(
    runtime_path: &Path,
    stdlib_path: &Path,
    verification: ProviderVerification,
) -> Result<LibrariesManifest> {
    let runtime_dir = runtime_path
        .parent()
        .ok_or_else(|| anyhow!("Perry runtime has no parent directory"))?;
    if stdlib_path.parent() != Some(runtime_dir) {
        return Err(anyhow!(
            "Perry runtime and stdlib must be packaged together in one directory: {} vs {}",
            runtime_path.display(),
            stdlib_path.display()
        ));
    }

    let manifest_path = runtime_dir.join("perry-libraries.json");
    if verification == ProviderVerification::RootOwnedImmutable {
        validate_root_owned_immutable_package(
            runtime_dir,
            &manifest_path,
            runtime_path,
            stdlib_path,
        )?;
    }
    let bytes = std::fs::read(&manifest_path)
        .with_context(|| format!("reading Perry library manifest {}", manifest_path.display()))?;
    let manifest: LibrariesManifest = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing Perry library manifest {}", manifest_path.display()))?;

    if manifest.manifest_version != 2 {
        return Err(anyhow!(
            "unsupported Perry library manifest version {} in {}",
            manifest.manifest_version,
            manifest_path.display()
        ));
    }
    if manifest.perry_version != crate::PERRY_RUNTIME_VERSION {
        return Err(anyhow!(
            "Perry provider ABI mismatch: package={}, host={}",
            manifest.perry_version,
            crate::PERRY_RUNTIME_VERSION
        ));
    }
    if manifest.perry_commit != crate::PERRY_RUNTIME_COMMIT {
        return Err(anyhow!(
            "Perry provider source mismatch: package={}, host={}",
            manifest.perry_commit,
            crate::PERRY_RUNTIME_COMMIT
        ));
    }
    let target = format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS);
    if manifest.target != target {
        return Err(anyhow!(
            "Perry provider target mismatch: package={}, host={target}",
            manifest.target
        ));
    }
    let runtime_file = runtime_path.file_name().and_then(|name| name.to_str());
    let stdlib_file = stdlib_path.file_name().and_then(|name| name.to_str());
    if runtime_file != Some(manifest.runtime_file.as_str())
        || stdlib_file != Some(manifest.stdlib_file.as_str())
    {
        return Err(anyhow!(
            "Perry provider filenames do not match {}",
            manifest_path.display()
        ));
    }
    match verification {
        ProviderVerification::FullHash => validate_provider_artifacts(
            runtime_path,
            manifest.runtime_size,
            &manifest.runtime_sha256,
            stdlib_path,
            manifest.stdlib_size,
            &manifest.stdlib_sha256,
        )?,
        ProviderVerification::RootOwnedImmutable => {
            validate_provider_size(runtime_path, manifest.runtime_size, "runtime")?;
            validate_provider_size(stdlib_path, manifest.stdlib_size, "stdlib")?;
        }
    }
    if manifest.perry_commit.len() != 40
        || manifest.compiler_sha256.len() != 64
        || manifest.runtime_sha256.len() != 64
        || manifest.stdlib_sha256.len() != 64
        || manifest.rustc_version.is_empty()
        || manifest.arena_block_size_bytes < 128 * 1024
        || !manifest.arena_block_size_bytes.is_power_of_two()
        || !matches!(manifest.allocator.as_str(), "system" | "mimalloc")
    {
        return Err(anyhow!(
            "Perry provider manifest lacks an exact source/toolchain identity"
        ));
    }
    Ok(manifest)
}

fn validate_provider_artifacts(
    runtime_path: &Path,
    expected_runtime_size: u64,
    expected_runtime_sha256: &str,
    stdlib_path: &Path,
    expected_stdlib_size: u64,
    expected_stdlib_sha256: &str,
) -> Result<()> {
    validate_provider_size(runtime_path, expected_runtime_size, "runtime")?;
    validate_provider_size(stdlib_path, expected_stdlib_size, "stdlib")?;

    // These independent files total roughly 95 MiB in the current provider
    // package. Hashing them serially dominated eager daemon startup despite
    // the host having multiple cores and enough I/O parallelism. Preserve the
    // full fail-closed integrity contract while verifying both images at once.
    let (runtime_sha256, stdlib_sha256) = std::thread::scope(|scope| {
        let runtime = scope.spawn(|| crate::plugin_host::library_sha256(runtime_path));
        let stdlib = scope.spawn(|| crate::plugin_host::library_sha256(stdlib_path));
        let runtime = runtime
            .join()
            .map_err(|_| anyhow!("Perry runtime integrity worker panicked"))??;
        let stdlib = stdlib
            .join()
            .map_err(|_| anyhow!("Perry stdlib integrity worker panicked"))??;
        Ok::<_, anyhow::Error>((runtime, stdlib))
    })?;
    if runtime_sha256 != expected_runtime_sha256 {
        return Err(anyhow!(
            "Perry runtime failed its SHA-256 integrity check: {}",
            runtime_path.display()
        ));
    }
    if stdlib_sha256 != expected_stdlib_sha256 {
        return Err(anyhow!(
            "Perry stdlib failed its SHA-256 integrity check: {}",
            stdlib_path.display()
        ));
    }
    Ok(())
}

fn validate_provider_size(path: &Path, expected_size: u64, kind: &str) -> Result<()> {
    let actual_size = std::fs::metadata(path)
        .with_context(|| format!("reading Perry {kind} metadata {}", path.display()))?
        .len();
    if actual_size == expected_size {
        Ok(())
    } else {
        Err(anyhow!(
            "Perry {kind} size mismatch for {}: package={expected_size}, actual={actual_size}",
            path.display()
        ))
    }
}

#[cfg(unix)]
fn validate_root_owned_immutable_package(
    package_dir: &Path,
    manifest_path: &Path,
    runtime_path: &Path,
    stdlib_path: &Path,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    if unsafe { libc::geteuid() } == 0 {
        return Err(anyhow!(
            "root_owned_immutable provider verification requires an unprivileged Perch process"
        ));
    }

    let mut directory = Some(package_dir);
    while let Some(path) = directory {
        let metadata = std::fs::symlink_metadata(path)
            .with_context(|| format!("checking immutable provider path {}", path.display()))?;
        if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Err(anyhow!(
                "immutable provider directory must be root-owned and not group/other writable: {}",
                path.display()
            ));
        }
        reject_effectively_writable(path, "provider directory")?;
        directory = path.parent();
    }

    for (path, kind) in [
        (manifest_path, "manifest"),
        (runtime_path, "runtime"),
        (stdlib_path, "stdlib"),
    ] {
        let metadata = std::fs::symlink_metadata(path)
            .with_context(|| format!("checking immutable Perry {kind} {}", path.display()))?;
        if !metadata.is_file() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Err(anyhow!(
                "immutable Perry {kind} must be a root-owned regular file that is not group/other writable: {}",
                path.display()
            ));
        }
        reject_effectively_writable(path, kind)?;
    }
    Ok(())
}

#[cfg(unix)]
fn reject_effectively_writable(path: &Path, kind: &str) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let path_c = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    if unsafe { libc::access(path_c.as_ptr(), libc::W_OK) } == 0 {
        Err(anyhow!(
            "root_owned_immutable Perry {kind} is writable by the Perch service identity: {}",
            path.display()
        ))
    } else {
        Ok(())
    }
}

#[cfg(not(unix))]
fn validate_root_owned_immutable_package(
    _package_dir: &Path,
    _manifest_path: &Path,
    _runtime_path: &Path,
    _stdlib_path: &Path,
) -> Result<()> {
    Err(anyhow!(
        "root_owned_immutable provider verification requires a Unix host"
    ))
}

pub fn runtime_libraries_loaded() -> bool {
    LIBRARIES.get().is_some()
}

pub fn perry_provider_identity() -> &'static PerryProviderIdentity {
    &LIBRARIES
        .get()
        .expect("Perry runtime libraries must be initialized first")
        .identity
}

pub(crate) fn runtime_api() -> &'static RuntimeApi {
    &LIBRARIES
        .get()
        .expect("Perry runtime libraries must be initialized before loading applications")
        .api
}

/// Return the live and reserved Perry arena bytes owned by the calling
/// executor thread. Perry's arena state is thread-local, so this must run on
/// the application's executor.
pub(crate) fn current_thread_arena_stats() -> (u64, u64) {
    let mut live_bytes = 0;
    let mut reserved_bytes = 0;
    unsafe {
        (runtime_api().js_arena_stats)(&mut live_bytes, &mut reserved_bytes);
    }
    (live_bytes, reserved_bytes)
}

/// Release collection side-storage owned by the calling executor thread.
/// Application libraries do not run Perry's executable exit epilogue, so the
/// host owns this lifecycle boundary.
pub(crate) fn release_current_thread_runtime_state() {
    unsafe {
        (runtime_api().js_gc_release_current_thread_collection_side_allocations)();
    }
}

pub fn register_queue_enqueue_callback(callback: PerchQueueEnqueueCallback) -> Result<()> {
    let registered =
        unsafe { (runtime_api().perch_host_register_queue_enqueue_callback)(callback) };
    if registered == 1 {
        Ok(())
    } else {
        Err(anyhow!(
            "Perry stdlib rejected the Perch queue enqueue callback"
        ))
    }
}

pub(crate) fn set_deployment_context(deployment_id: u64) {
    unsafe { (runtime_api().perch_host_set_deployment_context)(deployment_id) }
}

pub(crate) fn clear_deployment_context() {
    unsafe { (runtime_api().perch_host_clear_deployment_context)() }
}

pub(crate) fn provider_exports_symbol(name: &str) -> bool {
    let Ok(name) = std::ffi::CString::new(name) else {
        return false;
    };
    let libraries = LIBRARIES
        .get()
        .expect("Perry runtime libraries must be initialized first");
    unsafe {
        !libc::dlsym(libraries.runtime_handle as *mut libc::c_void, name.as_ptr()).is_null()
            || !libc::dlsym(libraries.stdlib_handle as *mut libc::c_void, name.as_ptr()).is_null()
    }
}

fn ensure_same_paths(
    existing: &RuntimeLibraries,
    runtime_path: &Path,
    stdlib_path: &Path,
    verification: ProviderVerification,
) -> Result<()> {
    if existing.runtime_path == runtime_path
        && existing.stdlib_path == stdlib_path
        && existing.verification == verification
    {
        Ok(())
    } else {
        Err(anyhow!(
            "Perry libraries are already initialized from {} and {} with {}; cannot replace them with {} and {} using {} in a live process",
            existing.runtime_path.display(),
            existing.stdlib_path.display(),
            existing.verification.as_str(),
            runtime_path.display(),
            stdlib_path.display(),
            verification.as_str()
        ))
    }
}

fn open_global(path: &Path) -> Result<*mut libc::c_void> {
    let path = std::ffi::CString::new(
        path.to_str()
            .ok_or_else(|| anyhow!("library path is not UTF-8: {}", path.display()))?,
    )?;
    let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
    if handle.is_null() {
        Err(anyhow!("dlopen failed: {}", dlerror_message()))
    } else {
        Ok(handle)
    }
}

unsafe fn load_symbol<T: Copy>(handle: *mut libc::c_void, name: &str) -> Result<T> {
    let name_c = std::ffi::CString::new(name)?;
    libc::dlerror();
    let symbol = libc::dlsym(handle, name_c.as_ptr());
    let error = libc::dlerror();
    if !error.is_null() || symbol.is_null() {
        return Err(anyhow!(
            "required Perry symbol {name:?} is missing: {}",
            if error.is_null() {
                "unknown loader error".to_string()
            } else {
                std::ffi::CStr::from_ptr(error)
                    .to_string_lossy()
                    .into_owned()
            }
        ));
    }
    debug_assert_eq!(std::mem::size_of::<T>(), std::mem::size_of_val(&symbol));
    Ok(std::mem::transmute_copy(&symbol))
}

fn dlerror_message() -> String {
    unsafe {
        let error = libc::dlerror();
        if error.is_null() {
            "unknown loader error".to_string()
        } else {
            std::ffi::CStr::from_ptr(error)
                .to_string_lossy()
                .into_owned()
        }
    }
}

// Make the process-lifetime ownership explicit and keep the handles observable
// to the optimizer even though they are used only by dyld after initialization.
impl Drop for RuntimeLibraries {
    fn drop(&mut self) {
        std::hint::black_box((self.runtime_handle, self.stdlib_handle));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_verification_names_are_strict() {
        assert_eq!(
            ProviderVerification::parse("full_hash").unwrap(),
            ProviderVerification::FullHash
        );
        assert_eq!(
            ProviderVerification::parse("root_owned_immutable").unwrap(),
            ProviderVerification::RootOwnedImmutable
        );
        assert!(ProviderVerification::parse("trust-me").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn immutable_verification_rejects_writable_development_package() {
        let package = tempfile::tempdir().unwrap();
        let manifest = package.path().join("perry-libraries.json");
        let runtime = package.path().join("libperry_runtime.so");
        let stdlib = package.path().join("libperry_stdlib.so");
        for path in [&manifest, &runtime, &stdlib] {
            std::fs::write(path, b"fixture").unwrap();
        }
        let error =
            validate_root_owned_immutable_package(package.path(), &manifest, &runtime, &stdlib)
                .unwrap_err()
                .to_string();
        assert!(
            error.contains("root-owned") || error.contains("unprivileged"),
            "unexpected rejection: {error}"
        );
    }
}
