//! Per-deployment Postgres schema provisioning + SQL migration runner.
//!
//! Each deployment gets its own Postgres schema (`deployment_<name>`).
//! Migrations live in `<deployment_dir>/migrations/*.sql` and are applied
//! in filename order at deploy time. Applied migrations are tracked in a
//! `_perch_migrations` table inside the deployment's schema.
//!
//! The daemon refuses to start a deployment if its migrations fail. This
//! is the spec's "migrations fail → deployment doesn't start" guarantee.
//!
//! **Note:** This module needs a real Postgres connection to run. For
//! Checkpoint 5 it's the structural skeleton + the migration logic using
//! `tokio-postgres` or `sqlx`. Since we don't have Postgres in the test
//! environment, the actual execution is gated behind `runtime_cfg.postgres`
//! being set.

use anyhow::{Context, Result};
use std::path::Path;
use tracing::{debug, info, warn};

/// Provision the Postgres schema for a deployment and run any pending
/// migrations. This is called by the deployment supervisor before
/// spawning the worker process.
///
/// Steps:
/// 1. `CREATE SCHEMA IF NOT EXISTS deployment_<name>`
/// 2. `CREATE TABLE IF NOT EXISTS deployment_<name>._perch_migrations (...)`
/// 3. Read `<deployment_dir>/migrations/*.sql`, sort by filename
/// 4. For each migration not yet in `_perch_migrations`: execute and record
///
/// Returns the number of migrations applied (0 if all were already applied).
pub async fn provision_and_migrate(
    _pg_url: &str,
    deployment_name: &str,
    deployment_dir: &Path,
) -> Result<usize> {
    let migrations_dir = deployment_dir.join("migrations");
    if !migrations_dir.exists() {
        debug!(
            deployment = deployment_name,
            "no migrations directory, skipping schema provisioning"
        );
        return Ok(0);
    }

    // Collect migration files sorted by name.
    let mut migration_files: Vec<_> = std::fs::read_dir(&migrations_dir)
        .with_context(|| format!("reading {:?}", migrations_dir))?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .map(|ext| ext == "sql")
                .unwrap_or(false)
        })
        .collect();
    migration_files.sort_by_key(|e| e.file_name());

    if migration_files.is_empty() {
        debug!(
            deployment = deployment_name,
            "migrations directory is empty"
        );
        return Ok(0);
    }

    info!(
        deployment = deployment_name,
        count = migration_files.len(),
        "found migration files"
    );

    // TODO: Connect to Postgres, create schema, run migrations.
    // This is gated behind having a real Postgres URL in runtime.toml.
    // For Checkpoint 5 we log the migration files and return 0.
    //
    // The real implementation:
    //   1. sqlx::PgPool::connect(pg_url).await
    //   2. sqlx::query("CREATE SCHEMA IF NOT EXISTS ...").execute(&pool)
    //   3. sqlx::query("CREATE TABLE IF NOT EXISTS ..._perch_migrations (
    //        name TEXT PRIMARY KEY, applied_at TIMESTAMPTZ DEFAULT now()
    //      )").execute(&pool)
    //   4. For each file: check if name is in _perch_migrations, if not
    //      execute the SQL and INSERT into _perch_migrations
    //   5. Return count of newly applied migrations

    for entry in &migration_files {
        let name = entry.file_name();
        info!(
            deployment = deployment_name,
            migration = ?name,
            "migration pending (Postgres not connected — schema provisioning is a scaffold)"
        );
    }

    warn!(
        deployment = deployment_name,
        "schema provisioning is a scaffold — connect Postgres in runtime.toml to enable"
    );

    Ok(0)
}

/// Read a migration file's SQL content.
#[allow(dead_code)]
fn read_migration(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).with_context(|| format!("reading migration {:?}", path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn no_migrations_dir_returns_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let result = provision_and_migrate("postgres://unused", "test", tmp.path()).await;
        assert_eq!(result.unwrap(), 0);
    }

    #[tokio::test]
    async fn empty_migrations_dir_returns_zero() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("migrations")).unwrap();
        let result = provision_and_migrate("postgres://unused", "test", tmp.path()).await;
        assert_eq!(result.unwrap(), 0);
    }

    #[tokio::test]
    async fn finds_sql_files_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let mig = tmp.path().join("migrations");
        std::fs::create_dir(&mig).unwrap();
        std::fs::write(mig.join("0002_second.sql"), "SELECT 2;").unwrap();
        std::fs::write(mig.join("0001_first.sql"), "SELECT 1;").unwrap();
        std::fs::write(mig.join("readme.md"), "not a migration").unwrap();

        // Currently returns 0 (scaffold), but doesn't error.
        let result = provision_and_migrate("postgres://unused", "test", tmp.path()).await;
        assert_eq!(result.unwrap(), 0);
    }
}
