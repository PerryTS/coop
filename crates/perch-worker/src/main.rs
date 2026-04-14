//! perch-worker — per-deployment host process.
//!
//! Spawned by perch-daemon with `--deployment <name>`. Responsibilities:
//!
//! 1. `dlopen` the deployment's compiled `.dylib` via perry-runtime's
//!    existing plugin loader (`perry_plugin_load`). Phase A.2 proved this
//!    flow works end-to-end: the deployment's `activate()` runs inside
//!    our address space and registers its tools/routes/hooks in perry's
//!    shared registry.
//!
//! 2. Listen on `/var/lib/perch/sockets/<deployment>.sock` for framed
//!    requests from perch-daemon. Each message is length-prefixed JSON
//!    (see perch_host_abi).
//!
//! 3. For every `Dispatch` request, build a `DeploymentRequest`, serialize
//!    to JSON, and invoke the deployment's registered `"route"` tool via
//!    `perry_plugin_invoke_tool`. The tool returns a `DeploymentResponse`
//!    JSON, which we frame back over the socket.
//!
//! The Phase A.2 derisk binary (scripts/derisk/host-rust) proved the
//! plugin-roundtrip half of this. perch-worker graduates that into a real
//! daemon-facing process, but the core trick — link perry-runtime, force
//! the symbols the plugin needs into our binary via black_box, dlopen,
//! invoke — is the same.

mod cron;
mod host;
mod listener;
mod plugin_host;
mod queue;
mod symbol_pin;

use clap::Parser;
use std::path::PathBuf;
use tracing::{error, info};

#[derive(Debug, Parser)]
#[command(name = "perch-worker", about = "Per-deployment host process")]
struct Cli {
    /// Deployment name. The worker will listen on
    /// <sockets_dir>/<deployment>.sock and load
    /// <compiled_dir>/<deployment>.dylib.
    #[arg(long)]
    deployment: String,

    /// Path to the compiled deployment dylib. If not set, defaults to
    /// <compiled_dir>/<deployment>.{dylib,so}.
    #[arg(long)]
    dylib: Option<PathBuf>,

    /// Directory where the worker's Unix socket is created. Defaults to
    /// /var/lib/perch/sockets.
    #[arg(long, default_value = "/var/lib/perch/sockets")]
    sockets_dir: PathBuf,

    /// Directory where compiled deployment dylibs live. Defaults to
    /// /var/lib/perch/compiled.
    #[arg(long, default_value = "/var/lib/perch/compiled")]
    compiled_dir: PathBuf,

    /// Override the Perry module name for symbol lookup. By default the
    /// module name is derived from the dylib filename stem. When the
    /// daemon compiles `handlers/contact.ts` and names the output
    /// `landing.dylib`, the symbol inside is `perry_fn_contact_ts__handle`
    /// (based on the TS filename), not `perry_fn_landing__handle`. Pass
    /// `--module-name contact_ts` to tell the worker the correct name.
    #[arg(long)]
    module_name: Option<String>,
}

fn main() -> anyhow::Result<()> {
    // Force the linker to keep the perry-runtime symbols that the dlopen'd
    // plugin will reference as undefined. Without this, Cargo's release
    // linker dead-strips them from the worker binary and dlopen fails with
    // "symbol not found in flat namespace". See symbol_pin.rs for details.
    symbol_pin::force_link_perry_runtime_symbols();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .json()
        .init();

    let cli = Cli::parse();
    info!(deployment = %cli.deployment, "perch-worker starting");

    let dylib_path = cli.dylib.clone().unwrap_or_else(|| {
        let ext = if cfg!(target_os = "macos") { "dylib" } else { "so" };
        cli.compiled_dir.join(format!("{}.{}", cli.deployment, ext))
    });

    if !dylib_path.exists() {
        error!(dylib = %dylib_path.display(), "deployment dylib not found");
        std::process::exit(1);
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name(format!("perch-worker-{}", cli.deployment))
        .build()?;

    runtime.block_on(async move {
        let host = host::DeploymentHost::load(
            &cli.deployment,
            &dylib_path,
            cli.module_name.as_deref(),
        )?;

        let socket_path = cli.sockets_dir.join(format!("{}.sock", cli.deployment));
        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let listener = listener::Listener::bind(&cli.deployment, &socket_path, host)?;
        info!(
            socket = %socket_path.display(),
            "worker ready"
        );

        listener.serve().await
    })?;

    Ok(())
}
