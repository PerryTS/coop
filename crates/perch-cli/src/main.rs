//! perch-cli — developer CLI for Perch.
//!
//! Subcommands:
//! - `perch-cli status` — show deployment list from the daemon admin API
//! - `perch-cli deploy <dir> <target>` — rsync + trigger reload
//! - `perch-cli logs <deployment>` — tail deployment logs (placeholder)
//! - `perch-cli rollback <deployment>` — rollback (placeholder)
//! - `perch-cli dev <dir>` — local dev mode (placeholder)

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "perch-cli", about = "Perch developer CLI", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Base URL of the perch daemon. Defaults to http://127.0.0.1:80.
    #[arg(long, env = "PERCH_URL", default_value = "http://127.0.0.1:80")]
    url: String,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Show all deployed deployments and their status.
    Status,

    /// Deploy a directory to the perch box.
    Deploy {
        /// Local directory containing the deployment (with perch.toml).
        dir: String,
        /// Remote target (e.g., root@box:/var/lib/perch/deployments/name).
        target: String,
    },

    /// Tail logs for a deployment.
    Logs {
        /// Deployment name.
        deployment: String,
    },

    /// Rollback a deployment to the previous version.
    Rollback {
        /// Deployment name.
        deployment: String,
    },

    /// Run a deployment in local development mode.
    Dev {
        /// Directory containing the deployment.
        dir: String,
    },
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    rt.block_on(async move {
        match cli.command {
            Command::Status => cmd_status(&cli.url).await,
            Command::Deploy { dir, target } => cmd_deploy(&dir, &target).await,
            Command::Logs { deployment } => cmd_logs(&cli.url, &deployment).await,
            Command::Rollback { deployment } => cmd_rollback(&cli.url, &deployment).await,
            Command::Dev { dir } => cmd_dev(&dir).await,
        }
    })
}

async fn cmd_status(base_url: &str) -> anyhow::Result<()> {
    let client = reqwest::Client::new();
    let url = format!("{}/_perch/admin", base_url.trim_end_matches('/'));
    println!("Fetching status from {}...", url);

    match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => {
            // For the MVP, we just fetch the admin HTML and show a summary.
            // A proper JSON API lands later; for now the admin dashboard IS the API.
            let body = resp.text().await?;
            // Quick and dirty: count deployments by counting table rows.
            let deployment_count = body.matches("<tr>").count().saturating_sub(1); // minus header
            println!("Connected. {} deployment(s) loaded.", deployment_count);
            println!("Open {} in a browser for the full dashboard.", url);
            Ok(())
        }
        Ok(resp) => {
            anyhow::bail!("daemon returned HTTP {}", resp.status());
        }
        Err(e) => {
            anyhow::bail!(
                "Could not connect to daemon at {}: {}. \
                 Is `perch` running? Set --url or PERCH_URL if the \
                 daemon is on a different address.",
                base_url,
                e
            );
        }
    }
}

async fn cmd_deploy(dir: &str, target: &str) -> anyhow::Result<()> {
    // Use rsync to push the deployment directory to the target box.
    let status = tokio::process::Command::new("rsync")
        .args(["-avz", "--delete", &format!("{}/", dir), target])
        .status()
        .await?;

    if !status.success() {
        anyhow::bail!("rsync failed with exit code {:?}", status.code());
    }

    println!("Deployed {} to {}.", dir, target);
    println!("The daemon will auto-detect the change and reload.");
    Ok(())
}

async fn cmd_logs(_base_url: &str, deployment: &str) -> anyhow::Result<()> {
    println!(
        "Log tailing for '{}' is not yet implemented. \
         Check /_perch/admin for recent logs.",
        deployment
    );
    Ok(())
}

async fn cmd_rollback(_base_url: &str, deployment: &str) -> anyhow::Result<()> {
    println!(
        "Rollback for '{}' is not yet implemented. \
         Previous compiled dylibs are retained in the compiled_dir \
         for manual rollback.",
        deployment
    );
    Ok(())
}

async fn cmd_dev(dir: &str) -> anyhow::Result<()> {
    println!(
        "Dev mode for '{}' is not yet implemented. \
         Run `perch --config ./dev-runtime.toml` with a local \
         deployments_dir pointing at your project for now.",
        dir
    );
    Ok(())
}
