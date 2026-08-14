//! Multi-application worker-shard listener.
//!
//! The daemon loads immutable application generations through control
//! messages, then dispatches ordinary HTTP/cron/queue requests with an exact
//! runtime ID. Each loaded application still owns a separate thread-affine
//! `DeploymentHost`; only the process, provider mappings, Tokio runtime, queue
//! pool, and failure domain are shared.

use crate::listener::{process_request, read_frame, write_frame};
use anyhow::{anyhow, Context, Result};
use perch_app_host::host::{DeploymentHost, DeploymentHostOptions};
use perch_host_abi::{
    AbiError, ClientHello, WorkerDeploymentSpec, WorkerRequest, WorkerResponse, ABI_VERSION,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{watch, Mutex};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

struct LoadedDeployment {
    deployment: String,
    deployment_context_id: u64,
    host: Arc<DeploymentHost>,
}

struct LoadingDeployment {
    spec: WorkerDeploymentSpec,
    completion: watch::Sender<bool>,
}

enum BeginLoad {
    Start,
    Wait(watch::Receiver<bool>),
    AlreadyLoaded,
}

#[derive(Default)]
struct RegistryState {
    runtimes: HashMap<String, LoadedDeployment>,
    loaded_specs: HashMap<String, WorkerDeploymentSpec>,
    loading: HashMap<String, LoadingDeployment>,
}

impl RegistryState {
    fn begin_load(&mut self, spec: &WorkerDeploymentSpec, max_apps: usize) -> Result<BeginLoad> {
        let runtime_id = &spec.runtime_id;
        if let Some(loaded) = self.loaded_specs.get(runtime_id) {
            return if loaded == spec {
                Ok(BeginLoad::AlreadyLoaded)
            } else {
                Err(anyhow!(
                    "runtime ID {runtime_id:?} is already loaded with a different specification"
                ))
            };
        }
        if let Some(loading) = self.loading.get(runtime_id) {
            return if loading.spec == *spec {
                Ok(BeginLoad::Wait(loading.completion.subscribe()))
            } else {
                Err(anyhow!(
                    "runtime ID {runtime_id:?} is loading with a different specification"
                ))
            };
        }
        let deployments = self
            .loaded_specs
            .values()
            .map(|loaded| loaded.deployment.as_str())
            .chain(
                self.loading
                    .values()
                    .map(|loading| loading.spec.deployment.as_str()),
            )
            .collect::<HashSet<_>>();
        if !deployments.contains(spec.deployment.as_str()) && deployments.len() >= max_apps {
            return Err(anyhow!(
                "worker shard application capacity {max_apps} is exhausted"
            ));
        }
        let (completion, _) = watch::channel(false);
        self.loading.insert(
            runtime_id.clone(),
            LoadingDeployment {
                spec: spec.clone(),
                completion,
            },
        );
        Ok(BeginLoad::Start)
    }

    fn finish_load(&mut self, runtime_id: &str, loaded: bool) -> Result<()> {
        let loading = self
            .loading
            .remove(runtime_id)
            .ok_or_else(|| anyhow!("runtime ID {runtime_id:?} has no active load reservation"))?;
        if loaded {
            self.loaded_specs
                .insert(runtime_id.to_string(), loading.spec);
        }
        let _ = loading.completion.send(true);
        Ok(())
    }

    fn remove_loaded(&mut self, runtime_id: &str) -> Option<LoadedDeployment> {
        let runtime = self.runtimes.remove(runtime_id);
        if runtime.is_some() {
            self.loaded_specs.remove(runtime_id);
        }
        runtime
    }
}

type Registry = Arc<Mutex<RegistryState>>;

pub struct ShardListener {
    shard_id: String,
    socket_path: PathBuf,
    listener: UnixListener,
    registry: Registry,
    max_apps: usize,
    shutdown: CancellationToken,
}

impl ShardListener {
    pub fn bind(shard_id: &str, socket_path: &Path, max_apps: usize) -> Result<Self> {
        if max_apps == 0 {
            return Err(anyhow!("worker shard max_apps must be positive"));
        }
        if socket_path.exists() {
            std::fs::remove_file(socket_path)
                .with_context(|| format!("removing stale shard socket {socket_path:?}"))?;
        }
        let listener = UnixListener::bind(socket_path)
            .with_context(|| format!("binding worker shard socket at {socket_path:?}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("restricting shard socket {socket_path:?}"))?;
        }
        Ok(Self {
            shard_id: shard_id.to_string(),
            socket_path: socket_path.to_path_buf(),
            listener,
            registry: Arc::new(Mutex::new(RegistryState::default())),
            max_apps,
            shutdown: CancellationToken::new(),
        })
    }

    pub async fn serve(self) -> Result<()> {
        let Self {
            shard_id,
            socket_path,
            listener,
            registry,
            max_apps,
            shutdown,
        } = self;

        loop {
            let accepted = tokio::select! {
                biased;
                _ = shutdown.cancelled() => break,
                accepted = listener.accept() => accepted,
            };
            match accepted {
                Ok((stream, _)) => {
                    let shard_id = shard_id.clone();
                    let registry = registry.clone();
                    let shutdown = shutdown.clone();
                    tokio::spawn(async move {
                        if let Err(error) =
                            handle_connection(&shard_id, stream, registry, max_apps, shutdown).await
                        {
                            warn!(shard = %shard_id, ?error, "shard connection failed");
                        }
                    });
                }
                Err(error) => {
                    error!(shard = %shard_id, ?error, "shard accept failed");
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            }
            if !socket_path.exists() {
                warn!(shard = %shard_id, "shard socket removed; shutting down");
                break;
            }
        }
        drop(listener);

        let loaded = {
            let mut state = registry.lock().await;
            for (_, loading) in state.loading.drain() {
                let _ = loading.completion.send(true);
            }
            state.loaded_specs.clear();
            state
                .runtimes
                .drain()
                .map(|(_, runtime)| runtime)
                .collect::<Vec<_>>()
        };
        for runtime in loaded {
            if let Err(error) = runtime.host.shutdown().await {
                warn!(
                    shard = %shard_id,
                    deployment = %runtime.deployment,
                    ?error,
                    "shard application shutdown failed"
                );
            }
            perch_app_host::queue_store::unregister_enqueue_context(runtime.deployment_context_id);
        }
        match std::fs::remove_file(&socket_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("removing worker shard socket"),
        }
        info!(shard = %shard_id, "worker shard stopped");
        Ok(())
    }
}

async fn handle_connection(
    shard_id: &str,
    stream: UnixStream,
    registry: Registry,
    max_apps: usize,
    shutdown: CancellationToken,
) -> Result<()> {
    let (mut reader, mut writer) = stream.into_split();
    let first = read_frame(&mut reader).await?;
    let request: WorkerRequest = serde_json::from_slice(&first).context("parsing shard Hello")?;
    match request {
        WorkerRequest::Hello(ClientHello {
            abi_version,
            client_name,
        }) if abi_version == ABI_VERSION => {
            debug!(shard = shard_id, client = %client_name, "shard client connected");
            write_frame(
                &mut writer,
                &WorkerResponse::Hello {
                    abi_version: ABI_VERSION,
                    worker_name: format!("perch-worker-shard/{}", env!("CARGO_PKG_VERSION")),
                    deployment: format!("shard:{shard_id}"),
                },
            )
            .await?;
        }
        WorkerRequest::Hello(ClientHello { abi_version, .. }) => {
            write_frame(
                &mut writer,
                &WorkerResponse::ProtocolError {
                    message: format!(
                        "ABI version mismatch: client={abi_version}, worker={ABI_VERSION}"
                    ),
                },
            )
            .await?;
            return Err(AbiError::VersionMismatch {
                client: abi_version,
                worker: ABI_VERSION,
            }
            .into());
        }
        other => {
            write_frame(
                &mut writer,
                &WorkerResponse::ProtocolError {
                    message: format!("expected Hello as first shard frame, got {other:?}"),
                },
            )
            .await?;
            return Err(anyhow!("first shard frame was not Hello"));
        }
    }

    loop {
        let frame = match read_frame(&mut reader).await {
            Ok(frame) => frame,
            Err(error) => {
                debug!(shard = shard_id, ?error, "shard connection closed");
                return Ok(());
            }
        };
        let request: WorkerRequest =
            serde_json::from_slice(&frame).context("parsing shard request")?;
        let response = match request {
            WorkerRequest::LoadDeployment {
                request_id,
                deployment,
            } => load_deployment(registry.clone(), max_apps, request_id, deployment).await,
            WorkerRequest::UnloadDeployment {
                request_id,
                runtime_id,
            } => unload_deployment(registry.clone(), request_id, runtime_id).await,
            WorkerRequest::Shutdown { .. } => {
                shutdown.cancel();
                WorkerResponse::Goodbye
            }
            WorkerRequest::Dispatch {
                runtime_id: Some(ref runtime_id),
                ..
            }
            | WorkerRequest::Cron {
                runtime_id: Some(ref runtime_id),
                ..
            }
            | WorkerRequest::Queue {
                runtime_id: Some(ref runtime_id),
                ..
            } => {
                let selected = {
                    let state = registry.lock().await;
                    state
                        .runtimes
                        .get(runtime_id)
                        .map(|runtime| (runtime.host.clone(), runtime.deployment.clone()))
                };
                match selected {
                    Some((host, deployment)) => process_request(request, host, &deployment).await,
                    None => WorkerResponse::ProtocolError {
                        message: format!("unknown shard runtime {runtime_id:?}"),
                    },
                }
            }
            WorkerRequest::Dispatch {
                runtime_id: None, ..
            }
            | WorkerRequest::Cron {
                runtime_id: None, ..
            }
            | WorkerRequest::Queue {
                runtime_id: None, ..
            } => WorkerResponse::ProtocolError {
                message: "shard dispatch requires runtime_id".to_string(),
            },
            WorkerRequest::Hello(_) => WorkerResponse::ProtocolError {
                message: "Hello after shard handshake".to_string(),
            },
        };
        write_frame(&mut writer, &response).await?;
        if matches!(response, WorkerResponse::Goodbye) {
            return Ok(());
        }
    }
}

async fn load_deployment(
    registry: Registry,
    max_apps: usize,
    request_id: u64,
    spec: WorkerDeploymentSpec,
) -> WorkerResponse {
    let runtime_id = spec.runtime_id.clone();
    let reservation = loop {
        let action = {
            let mut state = registry.lock().await;
            state.begin_load(&spec, max_apps)
        };
        match action {
            Ok(BeginLoad::Start) => break Ok(()),
            Ok(BeginLoad::AlreadyLoaded) => {
                return WorkerResponse::LoadResult {
                    request_id,
                    runtime_id,
                    error: None,
                };
            }
            Ok(BeginLoad::Wait(mut completion)) => match completion.changed().await {
                Ok(()) => continue,
                Err(_) => {
                    break Err(anyhow!(
                        "runtime ID {runtime_id:?} load coordination ended unexpectedly"
                    ))
                }
            },
            Err(error) => break Err(error),
        }
    };
    if let Err(error) = reservation {
        return WorkerResponse::LoadResult {
            request_id,
            runtime_id,
            error: Some(format!("{error:#}")),
        };
    }

    let result = async {
        validate_spec(&spec)?;

        let context_id = spec.deployment_context_id;
        let registered = if context_id != 0 {
            let policies = spec
                .queue_policies
                .iter()
                .map(|policy| {
                    (
                        policy.name.clone(),
                        perch_app_host::queue_store::EnqueuePolicy {
                            max_payload_bytes: policy.max_payload_bytes,
                            max_attempts: policy.max_attempts,
                            max_delay: std::time::Duration::from_millis(policy.max_delay_ms),
                        },
                    )
                })
                .collect();
            perch_app_host::queue_store::register_enqueue_context(
                context_id,
                spec.deployment.clone(),
                policies,
            )?;
            true
        } else {
            false
        };

        let deployment = spec.deployment.clone();
        let dylib_path = spec.dylib_path.clone();
        let module_name = spec.module_name.clone();
        let options = DeploymentHostOptions {
            executor_stack_size_bytes: spec.executor_stack_size_bytes,
            command_queue_capacity: spec.command_queue_capacity,
            gc_reclaim_check_interval: spec.gc_reclaim_check_interval,
            gc_reclaim_growth_bytes: spec.gc_reclaim_growth_bytes,
            deployment_context_id: context_id,
        };
        let loaded = match tokio::task::spawn_blocking(move || {
            DeploymentHost::load_with_options(
                &deployment,
                &dylib_path,
                module_name.as_deref(),
                options,
            )
        })
        .await
        {
            Ok(loaded) => loaded,
            Err(error) => {
                if registered {
                    perch_app_host::queue_store::unregister_enqueue_context(context_id);
                }
                return Err(anyhow!("joining shard application preload: {error}"));
            }
        };
        let host = match loaded {
            Ok(host) => Arc::new(host),
            Err(error) => {
                if registered {
                    perch_app_host::queue_store::unregister_enqueue_context(context_id);
                }
                return Err(error);
            }
        };

        let mut state = registry.lock().await;
        state.finish_load(&runtime_id, true)?;
        state.runtimes.insert(
            runtime_id.clone(),
            LoadedDeployment {
                deployment: spec.deployment,
                deployment_context_id: context_id,
                host,
            },
        );
        Ok::<(), anyhow::Error>(())
    }
    .await;

    if result.is_err() {
        let mut state = registry.lock().await;
        if state.loading.contains_key(&runtime_id) {
            let _ = state.finish_load(&runtime_id, false);
        }
    }
    WorkerResponse::LoadResult {
        request_id,
        runtime_id,
        error: result.err().map(|error| format!("{error:#}")),
    }
}

async fn unload_deployment(
    registry: Registry,
    request_id: u64,
    runtime_id: String,
) -> WorkerResponse {
    let runtime = registry.lock().await.remove_loaded(&runtime_id);
    let error = match runtime {
        Some(runtime) => {
            let result = runtime.host.shutdown().await;
            perch_app_host::queue_store::unregister_enqueue_context(runtime.deployment_context_id);
            result.err().map(|error| format!("{error:#}"))
        }
        None => Some(format!("unknown shard runtime {runtime_id:?}")),
    };
    WorkerResponse::UnloadResult {
        request_id,
        runtime_id,
        error,
    }
}

fn validate_spec(spec: &WorkerDeploymentSpec) -> Result<()> {
    if spec.deployment.is_empty()
        || spec.deployment.len() > 255
        || spec.deployment.contains(['/', '\\', '\0'])
    {
        return Err(anyhow!(
            "shard deployment must be a safe 1-255 byte path component"
        ));
    }
    if spec.runtime_id.is_empty()
        || spec.runtime_id.len() > 255
        || !spec
            .runtime_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(anyhow!(
            "shard runtime_id must contain 1-255 ASCII letters, digits, '-', '_' or '.'"
        ));
    }
    if !spec.dylib_path.is_absolute() || !spec.dylib_path.is_file() {
        return Err(anyhow!(
            "shard application library must be an existing absolute file: {}",
            spec.dylib_path.display()
        ));
    }
    if spec.executor_stack_size_bytes == 0
        || spec.command_queue_capacity == 0
        || (spec.gc_reclaim_check_interval > 0 && spec.gc_reclaim_growth_bytes == 0)
    {
        return Err(anyhow!("shard executor limits must be positive"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{validate_spec, BeginLoad, RegistryState};
    use perch_host_abi::WorkerDeploymentSpec;

    fn spec(path: std::path::PathBuf) -> WorkerDeploymentSpec {
        WorkerDeploymentSpec {
            deployment: "alpha".into(),
            runtime_id: "runtime-1".into(),
            dylib_path: path,
            module_name: None,
            executor_stack_size_bytes: 256 * 1024,
            command_queue_capacity: 8,
            gc_reclaim_check_interval: 256,
            gc_reclaim_growth_bytes: 256 * 1024,
            deployment_context_id: 0,
            queue_policies: Vec::new(),
        }
    }

    #[test]
    fn registry_capacity_allows_replacement_overlap_and_releases_failed_loads() {
        let mut state = RegistryState::default();
        let temp = tempfile::tempdir().unwrap();
        let mut alpha_1 = spec(temp.path().join("alpha-1.so"));
        alpha_1.runtime_id = "alpha-1".into();
        let mut alpha_2 = alpha_1.clone();
        alpha_2.runtime_id = "alpha-2".into();
        let mut beta_1 = alpha_1.clone();
        beta_1.deployment = "beta".into();
        beta_1.runtime_id = "beta-1".into();

        assert!(matches!(
            state.begin_load(&alpha_1, 1).unwrap(),
            BeginLoad::Start
        ));
        assert!(matches!(
            state.begin_load(&alpha_2, 1).unwrap(),
            BeginLoad::Start
        ));
        assert!(state.begin_load(&beta_1, 1).is_err());
        assert!(matches!(
            state.begin_load(&alpha_1, 1).unwrap(),
            BeginLoad::Wait(_)
        ));

        state.finish_load("alpha-1", false).unwrap();
        assert!(state.begin_load(&beta_1, 1).is_err());
        state.finish_load("alpha-2", false).unwrap();
        assert!(matches!(
            state.begin_load(&beta_1, 1).unwrap(),
            BeginLoad::Start
        ));
    }

    #[test]
    fn registry_load_retry_is_idempotent_but_runtime_id_collision_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let application = spec(temp.path().join("alpha.so"));

        let mut state = RegistryState::default();
        assert!(matches!(
            state.begin_load(&application, 1).unwrap(),
            BeginLoad::Start
        ));
        assert!(matches!(
            state.begin_load(&application, 1).unwrap(),
            BeginLoad::Wait(_)
        ));
        let mut collision = application.clone();
        collision.module_name = Some("different".into());
        assert!(state.begin_load(&collision, 1).is_err());

        state.finish_load(&application.runtime_id, true).unwrap();
        assert!(matches!(
            state.begin_load(&application, 1).unwrap(),
            BeginLoad::AlreadyLoaded
        ));
        assert!(state.begin_load(&collision, 1).is_err());
    }

    #[test]
    fn deployment_specs_fail_closed_before_loading() {
        let temp = tempfile::tempdir().unwrap();
        let library = temp.path().join("app.so");
        std::fs::write(&library, b"test fixture").unwrap();
        validate_spec(&spec(library.clone())).unwrap();

        let mut invalid = spec(library.clone());
        invalid.deployment = "../alpha".into();
        assert!(validate_spec(&invalid).is_err());
        invalid = spec(library.clone());
        invalid.runtime_id = "bad runtime".into();
        assert!(validate_spec(&invalid).is_err());
        invalid = spec(library.clone());
        invalid.executor_stack_size_bytes = 0;
        assert!(validate_spec(&invalid).is_err());
        invalid = spec(library.clone());
        invalid.command_queue_capacity = 0;
        assert!(validate_spec(&invalid).is_err());
        invalid = spec(library);
        invalid.gc_reclaim_growth_bytes = 0;
        assert!(validate_spec(&invalid).is_err());
        invalid = spec(std::path::PathBuf::from("relative.so"));
        assert!(validate_spec(&invalid).is_err());
        invalid = spec(temp.path().join("missing.so"));
        assert!(validate_spec(&invalid).is_err());
    }
}
