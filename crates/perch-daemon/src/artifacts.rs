//! Immutable application-package state, rollback, retention, and crash cleanup.
//!
//! Package directories are content addressed. The package digest commits the
//! ABI/integrity manifest and the exact deployment configuration needed to
//! restore routes, cron/queue entries, and limits during rollback. Activation
//! state is a separately atomically replaced file inside each deployment
//! namespace.

use crate::config::DeploymentConfig;
use anyhow::{anyhow, Context, Result};
use perch_host_abi::AppLibraryManifest;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

pub const PACKAGE_CONFIG_FILE: &str = "deployment.perch.json";
pub const STATIC_MANIFEST_FILE: &str = "static.perch-manifest.json";
const STATIC_ROOT: &str = ".perch-static";
const STATE_FILE: &str = ".perch-deployment-state.json";
const STATE_TEMP_PREFIX: &str = ".perch-deployment-state.tmp-";
const STATE_VERSION: u32 = 1;
const PACKAGE_DIGEST_DOMAIN: &[u8] = b"perch-application-package-v1\0";
const PACKAGE_DIGEST_V2_DOMAIN: &[u8] = b"perch-application-package-v2\0";
const STATIC_MANIFEST_VERSION: u32 = 1;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PackageActivation {
    pub package_sha256: String,
    pub activated_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeploymentArtifactState {
    pub version: u32,
    pub deployment: String,
    pub active: Option<PackageActivation>,
    #[serde(default)]
    pub previous: Vec<PackageActivation>,
    pub updated_at_ms: u64,
}

impl DeploymentArtifactState {
    fn empty(deployment: &str) -> Self {
        Self {
            version: STATE_VERSION,
            deployment: deployment.to_string(),
            active: None,
            previous: Vec::new(),
            updated_at_ms: now_ms(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct VerifiedPackage {
    pub package_sha256: String,
    pub library_path: PathBuf,
    pub config: DeploymentConfig,
    pub static_snapshotted: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ArtifactStatus {
    pub active: Option<String>,
    pub previous: Vec<String>,
    pub retained_bytes: u64,
    pub packages: Vec<ArtifactPackageStatus>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ArtifactPackageStatus {
    pub package_sha256: String,
    pub bytes: u64,
    pub active: bool,
    pub rollback_pinned: bool,
    pub rollbackable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct StaticAssetManifest {
    version: u32,
    files: Vec<StaticAssetFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct StaticAssetFile {
    path: String,
    size: u64,
    sha256: String,
}

#[derive(Debug, Clone)]
pub struct ArtifactStore {
    root: PathBuf,
    retention_count: usize,
    retention_age: Option<Duration>,
    reconcile_age: Duration,
    max_static_files: usize,
    max_static_bytes: u64,
}

impl ArtifactStore {
    pub fn new(
        root: PathBuf,
        retention_count: usize,
        retention_days: u32,
        reconcile_age_seconds: u64,
        max_static_files: usize,
        max_static_bytes: u64,
    ) -> Result<Self> {
        if retention_count == 0 {
            return Err(anyhow!("artifact retention count must be positive"));
        }
        if max_static_files == 0 || max_static_bytes == 0 {
            return Err(anyhow!("artifact static snapshot limits must be positive"));
        }
        Ok(Self {
            root,
            retention_count,
            retention_age: (retention_days > 0)
                .then(|| Duration::from_secs(u64::from(retention_days) * 24 * 60 * 60)),
            reconcile_age: Duration::from_secs(reconcile_age_seconds),
            max_static_files,
            max_static_bytes,
        })
    }

    pub fn package_digest(manifest_bytes: &[u8], config_bytes: &[u8]) -> String {
        let mut digest = Sha256::new();
        digest.update(PACKAGE_DIGEST_DOMAIN);
        digest.update((manifest_bytes.len() as u64).to_be_bytes());
        digest.update(manifest_bytes);
        digest.update((config_bytes.len() as u64).to_be_bytes());
        digest.update(config_bytes);
        format!("{:x}", digest.finalize())
    }

    pub fn package_digest_with_static(
        manifest_bytes: &[u8],
        config_bytes: &[u8],
        static_manifest_bytes: &[u8],
    ) -> String {
        let mut digest = Sha256::new();
        digest.update(PACKAGE_DIGEST_V2_DOMAIN);
        for bytes in [manifest_bytes, config_bytes, static_manifest_bytes] {
            digest.update((bytes.len() as u64).to_be_bytes());
            digest.update(bytes);
        }
        format!("{:x}", digest.finalize())
    }

    pub fn ensure_namespace(&self, deployment: &str) -> Result<PathBuf> {
        validate_component("deployment", deployment)?;
        self.ensure_root()?;
        let namespace = self.root.join(deployment);
        std::fs::create_dir_all(&namespace)
            .with_context(|| format!("creating artifact namespace {}", namespace.display()))?;
        require_plain_directory(&namespace)?;
        Ok(namespace)
    }

    pub fn ensure_staging_root(&self) -> Result<PathBuf> {
        self.ensure_root()?;
        let staging = self.root.join(".staging");
        std::fs::create_dir_all(&staging)
            .with_context(|| format!("creating artifact staging root {}", staging.display()))?;
        require_plain_directory(&staging)?;
        Ok(staging)
    }

    /// Publish one fully synced staging directory into its immutable,
    /// content-addressed deployment namespace. Callers must handle an
    /// existing package before entering this path.
    pub fn publish_staging_package(
        &self,
        deployment: &str,
        package: &str,
        staging: &Path,
    ) -> Result<PathBuf> {
        validate_component("deployment", deployment)?;
        if !valid_package_id(package) {
            return Err(anyhow!("invalid application package identity {package:?}"));
        }
        validate_staging_tree(staging)?;
        let namespace = self.ensure_namespace(deployment)?;
        let target = namespace.join(package);
        if target.exists() {
            return Err(anyhow!(
                "immutable application package already exists at {}",
                target.display()
            ));
        }
        std::fs::rename(staging, &target).with_context(|| {
            format!(
                "atomically publishing staged package {} as {}",
                staging.display(),
                target.display()
            )
        })?;
        test_pause_at_crash_point("package_published");
        sync_directory(&namespace)?;
        Ok(target)
    }

    pub fn write_packaged_config(
        staging_dir: &Path,
        config: &DeploymentConfig,
    ) -> Result<(PathBuf, Vec<u8>)> {
        let bytes = serde_json::to_vec_pretty(config)
            .context("serializing deployment configuration for immutable package")?;
        let path = staging_dir.join(PACKAGE_CONFIG_FILE);
        std::fs::write(&path, &bytes)
            .with_context(|| format!("writing packaged config {}", path.display()))?;
        sync_file(&path)?;
        Ok((path, bytes))
    }

    /// Copy configured static trees into the immutable package and rewrite
    /// their roots to package-local paths. The returned manifest bytes are
    /// committed into the package identity and reverified on every activation.
    pub fn snapshot_static_assets(
        &self,
        staging_dir: &Path,
        deployment_dir: &Path,
        config: &DeploymentConfig,
    ) -> Result<(DeploymentConfig, Vec<u8>)> {
        require_plain_directory(staging_dir)?;
        let mut packaged_config = config.clone();
        let mut files = Vec::new();
        let mut total_bytes = 0u64;
        for (index, block) in config.static_blocks.iter().enumerate() {
            let source = deployment_dir.join(&block.directory);
            require_plain_directory(&source).with_context(|| {
                format!(
                    "static snapshot source is not a plain directory: {}",
                    source.display()
                )
            })?;
            let relative_root = PathBuf::from(STATIC_ROOT).join(index.to_string());
            let destination = staging_dir.join(&relative_root);
            copy_static_tree(
                &source,
                &destination,
                staging_dir,
                &mut files,
                &mut total_bytes,
                self.max_static_files,
                self.max_static_bytes,
            )?;
            packaged_config.static_blocks[index].directory = relative_root;
        }
        files.sort_by(|left, right| left.path.cmp(&right.path));
        let manifest = StaticAssetManifest {
            version: STATIC_MANIFEST_VERSION,
            files,
        };
        let bytes = serde_json::to_vec_pretty(&manifest)
            .context("serializing immutable static-asset manifest")?;
        let path = staging_dir.join(STATIC_MANIFEST_FILE);
        std::fs::write(&path, &bytes)
            .with_context(|| format!("writing static-asset manifest {}", path.display()))?;
        sync_file(&path)?;
        Ok((packaged_config, bytes))
    }

    pub fn package_id_for_library(&self, deployment: &str, library: &Path) -> Option<String> {
        let package_dir = library.parent()?;
        if package_dir.parent()? != self.root.join(deployment) {
            return None;
        }
        let package = package_dir.file_name()?.to_str()?;
        valid_package_id(package).then(|| package.to_string())
    }

    pub fn verify_package(&self, deployment: &str, package: &str) -> Result<VerifiedPackage> {
        validate_component("deployment", deployment)?;
        if !valid_package_id(package) {
            return Err(anyhow!("invalid application package identity {package:?}"));
        }
        let namespace = self.ensure_namespace(deployment)?;
        let package_dir = namespace.join(package);
        require_plain_directory(&package_dir)?;
        if package_dir.parent() != Some(namespace.as_path()) {
            return Err(anyhow!("application package escaped its namespace"));
        }

        let extension = if cfg!(target_os = "macos") {
            "dylib"
        } else {
            "so"
        };
        let library_path = package_dir.join(format!("app.{extension}"));
        require_plain_file(&library_path)?;
        let manifest_path = AppLibraryManifest::adjacent_path(&library_path);
        require_plain_file(&manifest_path)?;
        let config_path = package_dir.join(PACKAGE_CONFIG_FILE);
        require_plain_file(&config_path).with_context(|| {
            format!(
                "package {package} predates rollback configuration snapshots and cannot be rolled back"
            )
        })?;

        let manifest_bytes = std::fs::read(&manifest_path)
            .with_context(|| format!("reading {}", manifest_path.display()))?;
        let manifest: AppLibraryManifest = serde_json::from_slice(&manifest_bytes)
            .with_context(|| format!("parsing {}", manifest_path.display()))?;
        if manifest.deployment != deployment {
            return Err(anyhow!(
                "package {package} belongs to deployment {:?}, not {deployment:?}",
                manifest.deployment
            ));
        }
        let config_bytes = std::fs::read(&config_path)
            .with_context(|| format!("reading {}", config_path.display()))?;
        let config: DeploymentConfig = serde_json::from_slice(&config_bytes)
            .with_context(|| format!("parsing {}", config_path.display()))?;
        if config.name != deployment {
            return Err(anyhow!(
                "packaged configuration belongs to {:?}, not {deployment:?}",
                config.name
            ));
        }
        config
            .validate()
            .context("validating packaged deployment config")?;
        let static_manifest_path = package_dir.join(STATIC_MANIFEST_FILE);
        let (calculated, static_snapshotted) =
            match std::fs::symlink_metadata(&static_manifest_path) {
                Ok(_) => {
                    require_plain_file(&static_manifest_path)?;
                    let max_manifest_bytes = self
                        .max_static_files
                        .saturating_mul(512)
                        .saturating_add(1024) as u64;
                    let manifest_size = std::fs::metadata(&static_manifest_path)?.len();
                    if manifest_size > max_manifest_bytes {
                        return Err(anyhow!(
                            "static-asset manifest exceeds the verification size limit"
                        ));
                    }
                    let static_manifest_bytes = std::fs::read(&static_manifest_path)
                        .with_context(|| format!("reading {}", static_manifest_path.display()))?;
                    let static_manifest: StaticAssetManifest =
                        serde_json::from_slice(&static_manifest_bytes).with_context(|| {
                            format!("parsing {}", static_manifest_path.display())
                        })?;
                    self.verify_static_assets(&package_dir, &config, &static_manifest)?;
                    (
                        Self::package_digest_with_static(
                            &manifest_bytes,
                            &config_bytes,
                            &static_manifest_bytes,
                        ),
                        true,
                    )
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    (Self::package_digest(&manifest_bytes, &config_bytes), false)
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("reading metadata for {}", static_manifest_path.display())
                    })
                }
            };
        if calculated != package {
            return Err(anyhow!(
                "application package digest mismatch: directory={package}, calculated={calculated}"
            ));
        }

        Ok(VerifiedPackage {
            package_sha256: package.to_string(),
            library_path,
            config,
            static_snapshotted,
        })
    }

    fn verify_static_assets(
        &self,
        package_dir: &Path,
        config: &DeploymentConfig,
        manifest: &StaticAssetManifest,
    ) -> Result<()> {
        if manifest.version != STATIC_MANIFEST_VERSION {
            return Err(anyhow!(
                "unsupported static-asset manifest version {}",
                manifest.version
            ));
        }
        if manifest.files.len() > self.max_static_files {
            return Err(anyhow!(
                "static-asset manifest exceeds the configured file limit"
            ));
        }
        for (index, block) in config.static_blocks.iter().enumerate() {
            let expected = PathBuf::from(STATIC_ROOT).join(index.to_string());
            if block.directory != expected {
                return Err(anyhow!(
                    "packaged static block {index} points outside its immutable snapshot"
                ));
            }
        }

        let mut previous = None::<&str>;
        let mut expected_paths = HashSet::new();
        let mut total_bytes = 0u64;
        for file in &manifest.files {
            if previous.is_some_and(|previous| previous >= file.path.as_str()) {
                return Err(anyhow!(
                    "static-asset manifest paths are not unique and sorted"
                ));
            }
            previous = Some(&file.path);
            let relative = Path::new(&file.path);
            if !safe_static_manifest_path(relative, config.static_blocks.len()) {
                return Err(anyhow!("unsafe static-asset manifest path {:?}", file.path));
            }
            let path = package_dir.join(relative);
            require_plain_file(&path)?;
            let actual_size = std::fs::metadata(&path)?.len();
            if actual_size != file.size || file_sha256(&path)? != file.sha256 {
                return Err(anyhow!(
                    "static asset integrity mismatch for {}",
                    path.display()
                ));
            }
            total_bytes = total_bytes.saturating_add(actual_size);
            if total_bytes > self.max_static_bytes {
                return Err(anyhow!("static assets exceed the configured byte limit"));
            }
            expected_paths.insert(relative.to_path_buf());
        }

        let static_root = package_dir.join(STATIC_ROOT);
        let mut actual_paths = HashSet::new();
        match std::fs::symlink_metadata(&static_root) {
            Ok(_) => collect_plain_files(&static_root, package_dir, &mut actual_paths)?,
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound && expected_paths.is_empty() => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading static snapshot {}", static_root.display()))
            }
        }
        if actual_paths != expected_paths {
            return Err(anyhow!(
                "static snapshot contains files not committed by its manifest"
            ));
        }
        Ok(())
    }

    pub fn record_activation(
        &self,
        deployment: &str,
        library: &Path,
    ) -> Result<Option<DeploymentArtifactState>> {
        let Some(package) = self.package_id_for_library(deployment, library) else {
            return Ok(None);
        };
        // A package without the committed configuration snapshot is valid for
        // classic loading but must not become part of a rollback history.
        let config_path = library
            .parent()
            .expect("content-addressed package library has a parent")
            .join(PACKAGE_CONFIG_FILE);
        match std::fs::symlink_metadata(&config_path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                warn!(
                    deployment,
                    package_sha256 = package,
                    "loaded pre-snapshot application package; excluding it from rollback history"
                );
                return Ok(None);
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading metadata for {}", config_path.display()))
            }
            Ok(_) => {}
        }
        self.verify_package(deployment, &package)?;
        let mut state = self.read_state(deployment)?;
        if state
            .active
            .as_ref()
            .map(|active| active.package_sha256.as_str())
            != Some(package.as_str())
        {
            if let Some(active) = state.active.take() {
                state.previous.insert(0, active);
            }
            state
                .previous
                .retain(|entry| entry.package_sha256 != package);
            state.active = Some(PackageActivation {
                package_sha256: package,
                activated_at_ms: now_ms(),
            });
        }
        state.updated_at_ms = now_ms();
        self.trim_history(&mut state);
        self.write_state(&state)?;
        Ok(Some(state))
    }

    /// Return and verify the exact package persisted as active. Restarts use
    /// this immutable image instead of silently recompiling mutable sources.
    pub fn active_package(&self, deployment: &str) -> Result<Option<VerifiedPackage>> {
        let state = self.read_state(deployment)?;
        state
            .active
            .map(|active| self.verify_package(deployment, &active.package_sha256))
            .transpose()
    }

    pub fn status(&self, deployment: &str) -> Result<ArtifactStatus> {
        let state = self.read_state(deployment)?;
        let active = state.active.map(|entry| entry.package_sha256);
        let previous: Vec<String> = state
            .previous
            .iter()
            .map(|entry| entry.package_sha256.clone())
            .collect();
        let previous_set: HashSet<&str> = previous.iter().map(String::as_str).collect();
        let mut packages = self.package_directories(deployment)?;
        packages.sort();
        let namespace = self.ensure_namespace(deployment)?;
        let mut retained_bytes = 0u64;
        let packages = packages
            .into_iter()
            .map(|package_sha256| {
                let bytes = package_bytes(&namespace.join(&package_sha256))?;
                retained_bytes = retained_bytes.saturating_add(bytes);
                Ok(ArtifactPackageStatus {
                    rollbackable: self.verify_package(deployment, &package_sha256).is_ok(),
                    bytes,
                    active: active.as_deref() == Some(package_sha256.as_str()),
                    rollback_pinned: previous_set.contains(package_sha256.as_str()),
                    package_sha256,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(ArtifactStatus {
            active,
            previous,
            retained_bytes,
            packages,
        })
    }

    pub fn collect(&self, deployment: &str, extra_pins: &HashSet<String>) -> Result<Vec<String>> {
        let state = self.read_state(deployment)?;
        let mut pins = extra_pins.clone();
        if let Some(active) = state.active {
            pins.insert(active.package_sha256);
        }
        pins.extend(state.previous.into_iter().map(|entry| entry.package_sha256));

        let namespace = self.ensure_namespace(deployment)?;
        let trash = self.ensure_internal_directory(".trash")?;
        let mut removed = Vec::new();
        for package in self.package_directories(deployment)? {
            if pins.contains(&package) {
                continue;
            }
            let source = namespace.join(&package);
            validate_package_tree(&source)?;
            let target = trash.join(format!(
                "{deployment}-{package}-{}-{}",
                std::process::id(),
                TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::rename(&source, &target).with_context(|| {
                format!(
                    "moving unreferenced package {} to artifact trash",
                    source.display()
                )
            })?;
            test_pause_at_crash_point("trash_renamed");
            sync_directory(&namespace)?;
            std::fs::remove_dir_all(&target)
                .with_context(|| format!("removing artifact trash {}", target.display()))?;
            removed.push(package);
        }
        if !removed.is_empty() {
            sync_directory(&trash)?;
            info!(deployment, packages = ?removed, "collected unreferenced application packages");
        }
        Ok(removed)
    }

    pub fn reconcile_startup(&self) -> Result<usize> {
        self.ensure_root()?;
        let trash = self.ensure_internal_directory(".trash")?;
        let mut removed = 0;

        let staging = self.root.join(".staging");
        if staging.exists() {
            require_plain_directory(&staging)?;
            for entry in std::fs::read_dir(&staging)
                .with_context(|| format!("reading staging root {}", staging.display()))?
                .flatten()
            {
                let path = entry.path();
                // Compiler staging names carry their owner PID. A dead owner
                // makes the tree immediately abandoned even when it is newer
                // than the configured age; a live owner is protected until
                // the age threshold, which also keeps shared-root
                // misconfiguration from deleting another active compile.
                if (!older_than(&path, self.reconcile_age) && !staging_owner_is_dead(&path))
                    || validate_staging_tree(&path).is_err()
                {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                validate_component("staging entry", &name)?;
                let target = trash.join(format!(
                    "staging-{name}-{}-{}",
                    std::process::id(),
                    TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
                ));
                std::fs::rename(&path, &target).with_context(|| {
                    format!("moving abandoned staging directory {}", path.display())
                })?;
                std::fs::remove_dir_all(&target)
                    .with_context(|| format!("removing abandoned staging {}", target.display()))?;
                removed += 1;
            }
        }

        // State replacement writes and syncs a temporary file before an
        // atomic rename. A crash before that rename can only leave an
        // unreferenced temporary file inside a deployment namespace.
        for entry in std::fs::read_dir(&self.root)
            .with_context(|| format!("reading compiled root {}", self.root.display()))?
            .flatten()
        {
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) => {
                    warn!(path = %entry.path().display(), ?error, "cannot inspect artifact root entry");
                    continue;
                }
            };
            if !file_type.is_dir() || file_type.is_symlink() {
                continue;
            }
            let namespace_name = entry.file_name().to_string_lossy().to_string();
            if namespace_name.starts_with('.')
                || validate_component("deployment", &namespace_name).is_err()
            {
                continue;
            }
            let namespace = entry.path();
            let mut namespace_changed = false;
            for candidate in std::fs::read_dir(&namespace)
                .with_context(|| format!("reading artifact namespace {}", namespace.display()))?
                .flatten()
            {
                let name = candidate.file_name().to_string_lossy().to_string();
                if !name.starts_with(STATE_TEMP_PREFIX)
                    || !older_than(&candidate.path(), self.reconcile_age)
                {
                    continue;
                }
                if let Err(error) = require_plain_file(&candidate.path()) {
                    warn!(path = %candidate.path().display(), ?error, "refusing to remove unexpected artifact-state temporary entry");
                    continue;
                }
                std::fs::remove_file(candidate.path()).with_context(|| {
                    format!(
                        "removing abandoned artifact-state temporary file {}",
                        candidate.path().display()
                    )
                })?;
                namespace_changed = true;
                removed += 1;
            }
            if namespace_changed {
                sync_directory(&namespace)?;
            }
        }

        // A crash after an atomic rename to trash may leave only the trash
        // directory. It is already unreachable, so completing deletion is
        // safe after revalidating that it is a plain, shallow package tree.
        for entry in std::fs::read_dir(&trash)
            .with_context(|| format!("reading artifact trash {}", trash.display()))?
            .flatten()
        {
            let path = entry.path();
            if validate_staging_tree(&path).is_ok() {
                std::fs::remove_dir_all(&path).with_context(|| {
                    format!("finishing artifact trash removal {}", path.display())
                })?;
                removed += 1;
            } else {
                warn!(path = %path.display(), "refusing to remove unexpected artifact trash entry");
            }
        }
        sync_directory(&self.root)?;
        Ok(removed)
    }

    fn trim_history(&self, state: &mut DeploymentArtifactState) {
        let count_floor = self.retention_count.saturating_sub(1);
        let cutoff_ms = self
            .retention_age
            .map(|age| now_ms().saturating_sub(u64::try_from(age.as_millis()).unwrap_or(u64::MAX)));
        state.previous = state
            .previous
            .drain(..)
            .enumerate()
            .filter_map(|(index, activation)| {
                (index < count_floor
                    || cutoff_ms.is_some_and(|cutoff| activation.activated_at_ms >= cutoff))
                .then_some(activation)
            })
            .collect();
        let mut seen = HashSet::new();
        state
            .previous
            .retain(|entry| seen.insert(entry.package_sha256.clone()));
    }

    fn read_state(&self, deployment: &str) -> Result<DeploymentArtifactState> {
        validate_component("deployment", deployment)?;
        let path = self.ensure_namespace(deployment)?.join(STATE_FILE);
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(anyhow!(
                    "artifact state {} is not a plain file",
                    path.display()
                ))
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(DeploymentArtifactState::empty(deployment))
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading metadata for {}", path.display()))
            }
        }
        match std::fs::read(&path) {
            Ok(bytes) => {
                let state: DeploymentArtifactState = serde_json::from_slice(&bytes)
                    .with_context(|| format!("parsing artifact state {}", path.display()))?;
                if state.version != STATE_VERSION || state.deployment != deployment {
                    return Err(anyhow!(
                        "artifact state identity/version mismatch at {}",
                        path.display()
                    ));
                }
                let mut identities = state
                    .active
                    .iter()
                    .chain(state.previous.iter())
                    .map(|entry| entry.package_sha256.as_str());
                if identities.any(|identity| !valid_package_id(identity)) {
                    return Err(anyhow!(
                        "artifact state contains an invalid package identity at {}",
                        path.display()
                    ));
                }
                Ok(state)
            }
            Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
        }
    }

    fn write_state(&self, state: &DeploymentArtifactState) -> Result<()> {
        let namespace = self.ensure_namespace(&state.deployment)?;
        let path = namespace.join(STATE_FILE);
        let temp = namespace.join(format!(
            "{STATE_TEMP_PREFIX}{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut cleanup = TempFile::new(temp.clone());
        let bytes = serde_json::to_vec_pretty(state).context("serializing artifact state")?;
        std::fs::write(&temp, bytes)
            .with_context(|| format!("writing temporary artifact state {}", temp.display()))?;
        sync_file(&temp)?;
        test_pause_at_crash_point("state_temp_synced");
        std::fs::rename(&temp, &path).with_context(|| {
            format!(
                "atomically replacing artifact state {} from {}",
                path.display(),
                temp.display()
            )
        })?;
        cleanup.disarm();
        test_pause_at_crash_point("state_renamed");
        sync_directory(&namespace)?;
        Ok(())
    }

    fn package_directories(&self, deployment: &str) -> Result<Vec<String>> {
        validate_component("deployment", deployment)?;
        let namespace = self.root.join(deployment);
        if namespace.exists() {
            require_plain_directory(&namespace)?;
        }
        let entries = match std::fs::read_dir(&namespace) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading artifact namespace {}", namespace.display()))
            }
        };
        let mut packages = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if valid_package_id(&name)
                && entry
                    .file_type()
                    .map(|kind| kind.is_dir() && !kind.is_symlink())
                    .unwrap_or(false)
            {
                packages.push(name);
            }
        }
        Ok(packages)
    }

    fn ensure_root(&self) -> Result<()> {
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("creating compiled root {}", self.root.display()))?;
        require_plain_directory(&self.root)
    }

    fn ensure_internal_directory(&self, name: &str) -> Result<PathBuf> {
        self.ensure_root()?;
        let path = self.root.join(name);
        std::fs::create_dir_all(&path)
            .with_context(|| format!("creating internal artifact directory {}", path.display()))?;
        require_plain_directory(&path)?;
        Ok(path)
    }
}

struct TempFile {
    path: PathBuf,
    armed: bool,
}

impl TempFile {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn valid_package_id(value: &str) -> bool {
    value.len() == 64
        && value
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
}

fn validate_component(kind: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || value == "."
        || value == ".."
        || value.contains('/')
        || value.contains('\\')
    {
        return Err(anyhow!("invalid {kind} path component {value:?}"));
    }
    Ok(())
}

fn require_plain_directory(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("reading directory metadata {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(anyhow!("{} is not a plain directory", path.display()));
    }
    Ok(())
}

fn require_plain_file(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("reading file metadata {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(anyhow!("{} is not a plain file", path.display()));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn copy_static_tree(
    source: &Path,
    destination: &Path,
    package_root: &Path,
    files: &mut Vec<StaticAssetFile>,
    total_bytes: &mut u64,
    max_files: usize,
    max_bytes: u64,
) -> Result<()> {
    require_plain_directory(source)?;
    std::fs::create_dir_all(destination)
        .with_context(|| format!("creating static snapshot {}", destination.display()))?;
    require_plain_directory(destination)?;
    let mut entries = std::fs::read_dir(source)
        .with_context(|| format!("reading static source {}", source.display()))?
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let file_type = entry
            .file_type()
            .with_context(|| format!("reading static entry type {}", source_path.display()))?;
        if file_type.is_symlink() {
            return Err(anyhow!(
                "static snapshot may not contain symlinks: {}",
                source_path.display()
            ));
        }
        if file_type.is_dir() {
            copy_static_tree(
                &source_path,
                &destination_path,
                package_root,
                files,
                total_bytes,
                max_files,
                max_bytes,
            )?;
            continue;
        }
        if !file_type.is_file() {
            return Err(anyhow!(
                "static snapshot contains a non-file entry: {}",
                source_path.display()
            ));
        }
        if files.len() >= max_files {
            return Err(anyhow!(
                "static snapshot exceeds the configured {max_files} file limit"
            ));
        }
        std::fs::copy(&source_path, &destination_path).with_context(|| {
            format!(
                "copying static asset {} to {}",
                source_path.display(),
                destination_path.display()
            )
        })?;
        let size = std::fs::metadata(&destination_path)
            .with_context(|| format!("reading static snapshot {}", destination_path.display()))?
            .len();
        *total_bytes = total_bytes.saturating_add(size);
        if *total_bytes > max_bytes {
            return Err(anyhow!(
                "static snapshot exceeds the configured {max_bytes} byte limit"
            ));
        }
        sync_file(&destination_path)?;
        let relative = destination_path
            .strip_prefix(package_root)
            .with_context(|| {
                format!(
                    "static snapshot {} escaped package root {}",
                    destination_path.display(),
                    package_root.display()
                )
            })?;
        let path = relative
            .to_str()
            .ok_or_else(|| anyhow!("static asset path is not UTF-8: {}", relative.display()))?
            .replace(std::path::MAIN_SEPARATOR, "/");
        files.push(StaticAssetFile {
            path,
            size,
            sha256: file_sha256(&destination_path)?,
        });
    }
    sync_directory(destination)?;
    Ok(())
}

fn file_sha256(path: &Path) -> Result<String> {
    use std::io::Read;

    let file = std::fs::File::open(path)
        .with_context(|| format!("opening {} for hashing", path.display()))?;
    let mut reader = std::io::BufReader::new(file);
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .with_context(|| format!("hashing {}", path.display()))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn safe_static_manifest_path(path: &Path, block_count: usize) -> bool {
    let mut components = path.components();
    if components.next()
        != Some(std::path::Component::Normal(std::ffi::OsStr::new(
            STATIC_ROOT,
        )))
    {
        return false;
    }
    let Some(std::path::Component::Normal(index)) = components.next() else {
        return false;
    };
    let Some(index) = index.to_str().and_then(|index| index.parse::<usize>().ok()) else {
        return false;
    };
    index < block_count
        && components
            .map(|component| matches!(component, std::path::Component::Normal(_)))
            .reduce(|left, right| left && right)
            .unwrap_or(false)
}

fn collect_plain_files(
    directory: &Path,
    package_root: &Path,
    paths: &mut HashSet<PathBuf>,
) -> Result<()> {
    require_plain_directory(directory)?;
    for entry in std::fs::read_dir(directory)
        .with_context(|| format!("reading static snapshot {}", directory.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            return Err(anyhow!(
                "static snapshot contains a symlink: {}",
                path.display()
            ));
        }
        if file_type.is_dir() {
            collect_plain_files(&path, package_root, paths)?;
        } else if file_type.is_file() {
            paths.insert(path.strip_prefix(package_root)?.to_path_buf());
        } else {
            return Err(anyhow!(
                "static snapshot contains a non-file entry: {}",
                path.display()
            ));
        }
    }
    Ok(())
}

fn validate_package_tree(path: &Path) -> Result<()> {
    require_plain_directory(path)?;
    for entry in std::fs::read_dir(path)
        .with_context(|| format!("reading package tree {}", path.display()))?
        .flatten()
    {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let file_type = entry.file_type()?;
        if file_type.is_dir() && name == STATIC_ROOT {
            validate_plain_tree(&entry.path())?;
        } else if file_type.is_file()
            && matches!(
                name.as_ref(),
                "app.dylib"
                    | "app.so"
                    | "app.perch-lib.json"
                    | PACKAGE_CONFIG_FILE
                    | STATIC_MANIFEST_FILE
            )
        {
            require_plain_file(&entry.path())?;
        } else {
            return Err(anyhow!(
                "refusing to collect package with unexpected entry {}",
                entry.path().display()
            ));
        }
    }
    Ok(())
}

fn package_bytes(path: &Path) -> Result<u64> {
    require_plain_directory(path)?;
    let mut bytes = 0u64;
    for entry in std::fs::read_dir(path)
        .with_context(|| format!("reading package tree {}", path.display()))?
        .flatten()
    {
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            return Err(anyhow!(
                "package contains a symlink: {}",
                entry.path().display()
            ));
        }
        if file_type.is_dir() {
            bytes = bytes.saturating_add(package_bytes(&entry.path())?);
        } else if file_type.is_file() {
            bytes = bytes.saturating_add(
                entry
                    .metadata()
                    .with_context(|| format!("reading metadata for {}", entry.path().display()))?
                    .len(),
            );
        } else {
            return Err(anyhow!(
                "package contains a non-file entry: {}",
                entry.path().display()
            ));
        }
    }
    Ok(bytes)
}

fn validate_staging_tree(path: &Path) -> Result<()> {
    require_plain_directory(path)?;
    for entry in std::fs::read_dir(path)
        .with_context(|| format!("reading staging tree {}", path.display()))?
        .flatten()
    {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let file_type = entry.file_type()?;
        if file_type.is_dir() && name == STATIC_ROOT {
            validate_plain_tree(&entry.path())?;
        } else if file_type.is_dir() && name == ".perch-source" {
            validate_compiler_source_tree(&entry.path(), true)?;
        } else if file_type.is_file()
            && matches!(
                name.as_ref(),
                "app.dylib"
                    | "app.so"
                    | "app.perch-lib.json"
                    | "app.perch-exports"
                    | "app.perch-aliases"
                    | PACKAGE_CONFIG_FILE
                    | STATIC_MANIFEST_FILE
            )
        {
            require_plain_file(&entry.path())?;
        } else {
            return Err(anyhow!(
                "unexpected staging/trash entry {}",
                entry.path().display()
            ));
        }
    }
    Ok(())
}

fn validate_plain_tree(path: &Path) -> Result<()> {
    require_plain_directory(path)?;
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            return Err(anyhow!("tree contains symlink {}", entry.path().display()));
        }
        if file_type.is_dir() {
            validate_plain_tree(&entry.path())?;
        } else if !file_type.is_file() {
            return Err(anyhow!(
                "tree contains non-file entry {}",
                entry.path().display()
            ));
        }
    }
    Ok(())
}

fn validate_compiler_source_tree(path: &Path, root: bool) -> Result<()> {
    require_plain_directory(path)?;
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let name = entry.file_name();
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            if root && name == "node_modules" {
                continue;
            }
            return Err(anyhow!(
                "compiler snapshot contains unexpected symlink {}",
                entry.path().display()
            ));
        }
        if file_type.is_dir() {
            validate_compiler_source_tree(&entry.path(), false)?;
        } else if !file_type.is_file() {
            return Err(anyhow!(
                "compiler snapshot contains non-file entry {}",
                entry.path().display()
            ));
        }
    }
    Ok(())
}

fn older_than(path: &Path, age: Duration) -> bool {
    std::fs::symlink_metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|elapsed| elapsed >= age)
}

fn staging_owner_is_dead(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let mut suffix = name.rsplitn(3, '-');
    let Some(sequence) = suffix.next() else {
        return false;
    };
    let Some(pid) = suffix.next() else {
        return false;
    };
    if suffix.next().is_none() || sequence.parse::<u64>().is_err() {
        return false;
    }
    let Ok(pid) = pid.parse::<u32>() else {
        return false;
    };
    if pid == 0 || pid > libc::pid_t::MAX as u32 {
        return false;
    }

    #[cfg(unix)]
    {
        if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
            return false;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }
    #[cfg(not(unix))]
    {
        false
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn sync_file(path: &Path) -> Result<()> {
    std::fs::File::open(path)
        .with_context(|| format!("opening {} for sync", path.display()))?
        .sync_all()
        .with_context(|| format!("syncing {}", path.display()))
}

fn sync_directory(path: &Path) -> Result<()> {
    std::fs::File::open(path)
        .with_context(|| format!("opening directory {} for sync", path.display()))?
        .sync_all()
        .with_context(|| format!("syncing directory {}", path.display()))
}

/// Unit-test-only rendezvous used by the crash matrix below. The parent test
/// observes the marker and sends SIGKILL, so recovery is tested against an
/// actual process death at the same durable boundaries used in production.
#[cfg(test)]
fn test_pause_at_crash_point(point: &str) {
    if !std::env::var("PERCH_TEST_ARTIFACT_CRASH_POINT").is_ok_and(|value| value == point) {
        return;
    }
    let marker = std::env::var_os("PERCH_TEST_ARTIFACT_CRASH_MARKER")
        .expect("crash child requires a marker path");
    std::fs::write(&marker, point).expect("writing artifact crash marker");
    loop {
        std::thread::park_timeout(Duration::from_secs(60));
    }
}

#[cfg(not(test))]
#[inline]
fn test_pause_at_crash_point(_point: &str) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StaticConfig;
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;
    #[cfg(unix)]
    use std::process::{Command, Stdio};

    fn fixture_config(name: &str) -> DeploymentConfig {
        DeploymentConfig {
            name: name.into(),
            ..DeploymentConfig::default()
        }
    }

    fn publish_fixture(store: &ArtifactStore, deployment: &str, marker: &str) -> VerifiedPackage {
        let namespace = store.root.join(deployment);
        std::fs::create_dir_all(&namespace).unwrap();
        let staging = tempfile::tempdir_in(&namespace).unwrap();
        let library = staging.path().join(if cfg!(target_os = "macos") {
            "app.dylib"
        } else {
            "app.so"
        });
        std::fs::write(&library, marker).unwrap();
        let manifest = AppLibraryManifest {
            abi_version: perch_host_abi::APP_LIBRARY_ABI_VERSION,
            deployment: deployment.into(),
            perry_version: "test".into(),
            perry_commit: "test".into(),
            compiler_sha256: "test".into(),
            target: "test".into(),
            init_symbol: "perry_module_init".into(),
            handle_symbol: "handle".into(),
            handler_abi: perch_host_abi::HandlerAbi::Wrapped,
            cron_entries: vec![],
            queue_entries: vec![],
            boundary_verified: true,
            boundary_verification_version: perch_host_abi::APP_LIBRARY_BOUNDARY_VERSION,
            library_sha256: Some(marker.into()),
            library_size: Some(marker.len() as u64),
            source_sha256: Some(marker.into()),
            compile_source_sha256: Some(marker.into()),
            dependency_sha256: Some(marker.into()),
            compiler_invocation_sha256: Some(marker.into()),
        };
        let manifest_path = manifest.write(&library).unwrap();
        let manifest_bytes = std::fs::read(manifest_path).unwrap();
        let (_, config_bytes) =
            ArtifactStore::write_packaged_config(staging.path(), &fixture_config(deployment))
                .unwrap();
        let package = ArtifactStore::package_digest(&manifest_bytes, &config_bytes);
        let staging = staging.keep();
        store
            .publish_staging_package(deployment, &package, &staging)
            .unwrap();
        store.verify_package(deployment, &package).unwrap()
    }

    #[test]
    fn activation_rollback_history_and_collection_are_exact() {
        let temp = tempfile::tempdir().unwrap();
        let store =
            ArtifactStore::new(temp.path().into(), 2, 0, 0, 1_000, 10 * 1024 * 1024).unwrap();
        let first = publish_fixture(&store, "app", "first");
        let second = publish_fixture(&store, "app", "second");
        let third = publish_fixture(&store, "app", "third");

        store.record_activation("app", &first.library_path).unwrap();
        store
            .record_activation("app", &second.library_path)
            .unwrap();
        store.record_activation("app", &third.library_path).unwrap();
        let status = store.status("app").unwrap();
        assert_eq!(
            status.active.as_deref(),
            Some(third.package_sha256.as_str())
        );
        assert_eq!(status.previous, vec![second.package_sha256.clone()]);
        assert!(status.retained_bytes > 0);
        assert!(status.packages.iter().all(|package| package.bytes > 0));
        assert_eq!(
            store.active_package("app").unwrap().unwrap().package_sha256,
            third.package_sha256
        );

        let removed = store.collect("app", &HashSet::new()).unwrap();
        assert_eq!(removed, vec![first.package_sha256]);
        assert!(second.library_path.exists());
        assert!(third.library_path.exists());

        let verified = store.verify_package("app", &second.package_sha256).unwrap();
        assert_eq!(verified.config.name, "app");
        store
            .record_activation("app", &verified.library_path)
            .unwrap();
        let status = store.status("app").unwrap();
        assert_eq!(
            status.active.as_deref(),
            Some(second.package_sha256.as_str())
        );
        assert_eq!(status.previous, vec![third.package_sha256]);
    }

    #[test]
    fn package_digest_rejects_modified_config_and_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let store =
            ArtifactStore::new(temp.path().into(), 2, 0, 0, 1_000, 10 * 1024 * 1024).unwrap();
        let package = publish_fixture(&store, "app", "bytes");
        let config_path = package
            .library_path
            .parent()
            .unwrap()
            .join(PACKAGE_CONFIG_FILE);
        std::fs::write(&config_path, b"{}").unwrap();
        let error = store
            .verify_package("app", &package.package_sha256)
            .unwrap_err()
            .to_string();
        assert!(error.contains("parsing") || error.contains("belongs"));

        #[cfg(unix)]
        {
            let link = temp.path().join("app").join("a".repeat(64));
            std::os::unix::fs::symlink(package.library_path.parent().unwrap(), &link).unwrap();
            assert!(store.verify_package("app", &"a".repeat(64)).is_err());
        }
    }

    #[test]
    fn static_assets_are_snapshotted_rewritten_and_integrity_checked() {
        let temp = tempfile::tempdir().unwrap();
        let compiled = temp.path().join("compiled");
        let deployment_dir = temp.path().join("deployment");
        std::fs::create_dir_all(deployment_dir.join("public/nested")).unwrap();
        std::fs::write(deployment_dir.join("public/index.html"), b"version one").unwrap();
        std::fs::write(deployment_dir.join("public/nested/data.bin"), [0, 1, 0xff]).unwrap();
        let store = ArtifactStore::new(compiled, 2, 0, 0, 1_000, 10 * 1024 * 1024).unwrap();
        let namespace = store.ensure_namespace("app").unwrap();
        let staging = tempfile::tempdir_in(&namespace).unwrap();
        let library = staging.path().join(if cfg!(target_os = "macos") {
            "app.dylib"
        } else {
            "app.so"
        });
        std::fs::write(&library, b"library").unwrap();
        let manifest = AppLibraryManifest {
            abi_version: perch_host_abi::APP_LIBRARY_ABI_VERSION,
            deployment: "app".into(),
            perry_version: "test".into(),
            perry_commit: "test".into(),
            compiler_sha256: "test".into(),
            target: "test".into(),
            init_symbol: "perry_module_init".into(),
            handle_symbol: "handle".into(),
            handler_abi: perch_host_abi::HandlerAbi::Wrapped,
            cron_entries: vec![],
            queue_entries: vec![],
            boundary_verified: true,
            boundary_verification_version: perch_host_abi::APP_LIBRARY_BOUNDARY_VERSION,
            library_sha256: Some("library".into()),
            library_size: Some(7),
            source_sha256: Some("source".into()),
            compile_source_sha256: Some("compile-source".into()),
            dependency_sha256: Some("dependencies".into()),
            compiler_invocation_sha256: Some("compiler-invocation".into()),
        };
        let manifest_path = manifest.write(&library).unwrap();
        let mut config = fixture_config("app");
        config.static_blocks.push(StaticConfig {
            directory: "public".into(),
            path: "/".into(),
        });
        let (packaged_config, static_manifest_bytes) = store
            .snapshot_static_assets(staging.path(), &deployment_dir, &config)
            .unwrap();
        assert_eq!(
            packaged_config.static_blocks[0].directory,
            PathBuf::from(".perch-static/0")
        );
        let (_, config_bytes) =
            ArtifactStore::write_packaged_config(staging.path(), &packaged_config).unwrap();
        let manifest_bytes = std::fs::read(manifest_path).unwrap();
        let package = ArtifactStore::package_digest_with_static(
            &manifest_bytes,
            &config_bytes,
            &static_manifest_bytes,
        );
        let package_dir = namespace.join(&package);
        std::fs::rename(staging.keep(), &package_dir).unwrap();

        let verified = store.verify_package("app", &package).unwrap();
        assert_eq!(
            verified.config.static_blocks[0].directory,
            PathBuf::from(".perch-static/0")
        );
        assert_eq!(
            std::fs::read(package_dir.join(".perch-static/0/index.html")).unwrap(),
            b"version one"
        );
        std::fs::write(package_dir.join(".perch-static/0/index.html"), b"tampered").unwrap();
        assert!(store.verify_package("app", &package).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn static_snapshot_rejects_symlinks_and_size_limits() {
        let temp = tempfile::tempdir().unwrap();
        let deployment_dir = temp.path().join("deployment");
        std::fs::create_dir_all(deployment_dir.join("public")).unwrap();
        let outside = temp.path().join("outside");
        std::fs::write(&outside, b"secret").unwrap();
        std::os::unix::fs::symlink(&outside, deployment_dir.join("public/link")).unwrap();
        let store = ArtifactStore::new(temp.path().join("compiled"), 2, 0, 0, 1, 4).unwrap();
        let staging = store.ensure_staging_root().unwrap().join("snapshot");
        std::fs::create_dir(&staging).unwrap();
        let mut config = fixture_config("app");
        config.static_blocks.push(StaticConfig {
            directory: "public".into(),
            path: "/".into(),
        });
        assert!(store
            .snapshot_static_assets(&staging, &deployment_dir, &config)
            .unwrap_err()
            .to_string()
            .contains("symlink"));

        std::fs::remove_file(deployment_dir.join("public/link")).unwrap();
        std::fs::write(deployment_dir.join("public/large"), b"12345").unwrap();
        assert!(store
            .snapshot_static_assets(&staging, &deployment_dir, &config)
            .unwrap_err()
            .to_string()
            .contains("byte limit"));
    }

    #[test]
    fn startup_reconciliation_removes_only_plain_abandoned_trees() {
        let temp = tempfile::tempdir().unwrap();
        let store =
            ArtifactStore::new(temp.path().into(), 2, 0, 0, 1_000, 10 * 1024 * 1024).unwrap();
        let staging = store.ensure_staging_root().unwrap();
        let abandoned = staging.join("app-123-1");
        std::fs::create_dir(&abandoned).unwrap();
        std::fs::write(
            abandoned.join(if cfg!(target_os = "macos") {
                "app.dylib"
            } else {
                "app.so"
            }),
            b"partial",
        )
        .unwrap();
        assert_eq!(store.reconcile_startup().unwrap(), 1);
        assert!(!abandoned.exists());

        let namespace = store.ensure_namespace("app").unwrap();
        let state_temp = namespace.join(format!("{STATE_TEMP_PREFIX}old"));
        std::fs::write(&state_temp, b"complete but never published").unwrap();
        assert_eq!(store.reconcile_startup().unwrap(), 1);
        assert!(!state_temp.exists());

        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            let link = staging.join("do-not-follow");
            std::os::unix::fs::symlink(outside.path(), &link).unwrap();
            let state_link = namespace.join(format!("{STATE_TEMP_PREFIX}do-not-follow"));
            std::os::unix::fs::symlink(outside.path(), &state_link).unwrap();
            assert_eq!(store.reconcile_startup().unwrap(), 0);
            assert!(std::fs::symlink_metadata(link)
                .unwrap()
                .file_type()
                .is_symlink());
            assert!(std::fs::symlink_metadata(state_link)
                .unwrap()
                .file_type()
                .is_symlink());
        }
    }

    #[test]
    fn startup_reconciliation_uses_staging_owner_liveness() {
        let temp = tempfile::tempdir().unwrap();
        let store = ArtifactStore::new(
            temp.path().into(),
            2,
            0,
            24 * 60 * 60,
            1_000,
            10 * 1024 * 1024,
        )
        .unwrap();
        let staging = store.ensure_staging_root().unwrap();
        let live = staging.join(format!("app-{}-1", std::process::id()));
        std::fs::create_dir(&live).unwrap();
        assert_eq!(store.reconcile_startup().unwrap(), 0);
        assert!(live.exists());

        let dead = staging.join(format!("app-{}-2", libc::pid_t::MAX));
        std::fs::create_dir(&dead).unwrap();
        assert_eq!(store.reconcile_startup().unwrap(), 1);
        assert!(!dead.exists());
        assert!(live.exists());
    }

    #[test]
    fn durable_crash_points_leave_old_or_new_generation_recoverable() {
        let temp = tempfile::tempdir().unwrap();
        let store =
            ArtifactStore::new(temp.path().into(), 3, 0, 0, 1_000, 10 * 1024 * 1024).unwrap();
        let old = publish_fixture(&store, "app", "old");
        store.record_activation("app", &old.library_path).unwrap();

        // Crash after immutable package publication but before active-state
        // replacement: restart selects the old package while the complete new
        // package remains independently verifiable.
        let new = publish_fixture(&store, "app", "new");
        assert_eq!(
            store.active_package("app").unwrap().unwrap().package_sha256,
            old.package_sha256
        );
        assert_eq!(
            store
                .verify_package("app", &new.package_sha256)
                .unwrap()
                .package_sha256,
            new.package_sha256
        );

        // Crash after the replacement state file is written and fsynced but
        // before its atomic rename: the committed state is still old and
        // startup reconciliation removes only the abandoned temporary file.
        let mut desired = store.read_state("app").unwrap();
        let previous = desired.active.take().unwrap();
        desired.previous.insert(0, previous);
        desired.active = Some(PackageActivation {
            package_sha256: new.package_sha256.clone(),
            activated_at_ms: now_ms(),
        });
        desired.updated_at_ms = now_ms();
        let namespace = store.ensure_namespace("app").unwrap();
        let state_temp = namespace.join(format!("{STATE_TEMP_PREFIX}crash-before-rename"));
        std::fs::write(&state_temp, serde_json::to_vec_pretty(&desired).unwrap()).unwrap();
        sync_file(&state_temp).unwrap();
        assert_eq!(
            store.active_package("app").unwrap().unwrap().package_sha256,
            old.package_sha256
        );
        assert_eq!(store.reconcile_startup().unwrap(), 1);
        assert!(!state_temp.exists());

        // Crash after the atomic state rename (the normal write_state path):
        // the complete new generation is selected and the old one is retained
        // as rollback history.
        store.write_state(&desired).unwrap();
        let status = store.status("app").unwrap();
        assert_eq!(status.active.as_deref(), Some(new.package_sha256.as_str()));
        assert_eq!(status.previous, vec![old.package_sha256.clone()]);

        // Crash after collection's atomic move to private trash but before
        // deletion cannot affect the active/rollback packages. Reconciliation
        // finishes deleting only the validated orphan tree.
        let orphan = publish_fixture(&store, "app", "orphan");
        let trash = store.ensure_internal_directory(".trash").unwrap();
        let orphan_dir = orphan.library_path.parent().unwrap();
        let trashed = trash.join(format!("collection-crash-{}", orphan.package_sha256));
        std::fs::rename(orphan_dir, &trashed).unwrap();
        assert_eq!(
            store.active_package("app").unwrap().unwrap().package_sha256,
            new.package_sha256
        );
        assert_eq!(store.reconcile_startup().unwrap(), 1);
        assert!(!trashed.exists());
        assert!(old.library_path.exists());
        assert!(new.library_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn os_process_kill_matrix_recovers_durable_boundaries() {
        const CHILD_ENV: &str = "PERCH_TEST_ARTIFACT_CRASH_CHILD";
        const ROOT_ENV: &str = "PERCH_TEST_ARTIFACT_CRASH_ROOT";
        const PACKAGE_ENV: &str = "PERCH_TEST_ARTIFACT_CRASH_PACKAGE";
        const TEST_NAME: &str =
            "artifacts::tests::os_process_kill_matrix_recovers_durable_boundaries";

        if let Ok(point) = std::env::var(CHILD_ENV) {
            let root = PathBuf::from(
                std::env::var_os(ROOT_ENV).expect("artifact crash child requires its store root"),
            );
            let store = ArtifactStore::new(root, 3, 0, 0, 1_000, 10 * 1024 * 1024).unwrap();
            match point.as_str() {
                "package_published" => {
                    let _ = publish_fixture(&store, "app", "new");
                }
                "state_temp_synced" | "state_renamed" => {
                    let package = std::env::var(PACKAGE_ENV)
                        .expect("state crash child requires its package identity");
                    let verified = store.verify_package("app", &package).unwrap();
                    store
                        .record_activation("app", &verified.library_path)
                        .unwrap();
                }
                "trash_renamed" => {
                    store.collect("app", &HashSet::new()).unwrap();
                }
                other => panic!("unknown artifact crash point {other}"),
            }
            panic!("artifact crash child passed {point} without pausing");
        }

        let temp = tempfile::tempdir().unwrap();
        for point in [
            "package_published",
            "state_temp_synced",
            "state_renamed",
            "trash_renamed",
        ] {
            let root = temp.path().join(point);
            let marker = temp.path().join(format!("{point}.ready"));
            let store = ArtifactStore::new(root.clone(), 3, 0, 0, 1_000, 10 * 1024 * 1024).unwrap();
            let old = publish_fixture(&store, "app", "old");
            store.record_activation("app", &old.library_path).unwrap();
            let prepared = match point {
                "state_temp_synced" | "state_renamed" => {
                    Some(publish_fixture(&store, "app", "new"))
                }
                "trash_renamed" => Some(publish_fixture(&store, "app", "orphan")),
                _ => None,
            };

            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .arg("--exact")
                .arg(TEST_NAME)
                .arg("--nocapture")
                .env(CHILD_ENV, point)
                .env(ROOT_ENV, &root)
                .env("PERCH_TEST_ARTIFACT_CRASH_POINT", point)
                .env("PERCH_TEST_ARTIFACT_CRASH_MARKER", &marker)
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            if matches!(point, "state_temp_synced" | "state_renamed") {
                command.env(PACKAGE_ENV, &prepared.as_ref().unwrap().package_sha256);
            }
            let mut child = command.spawn().expect("spawn artifact crash child");
            let mut reached = false;
            for _ in 0..500 {
                if marker.exists() {
                    reached = true;
                    break;
                }
                if let Some(status) = child.try_wait().unwrap() {
                    panic!("artifact crash child exited before {point} with status {status}");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(reached, "artifact crash child never reached {point}");
            child.kill().expect("SIGKILL artifact crash child");
            let status = child.wait().expect("reap artifact crash child");
            assert_eq!(status.signal(), Some(libc::SIGKILL));

            let recovered = ArtifactStore::new(root, 3, 0, 0, 1_000, 10 * 1024 * 1024).unwrap();
            match point {
                "package_published" => {
                    assert_eq!(
                        recovered
                            .active_package("app")
                            .unwrap()
                            .unwrap()
                            .package_sha256,
                        old.package_sha256
                    );
                    let packages = recovered.package_directories("app").unwrap();
                    assert_eq!(packages.len(), 2);
                    for package in packages {
                        recovered.verify_package("app", &package).unwrap();
                    }
                }
                "state_temp_synced" => {
                    assert_eq!(
                        recovered
                            .active_package("app")
                            .unwrap()
                            .unwrap()
                            .package_sha256,
                        old.package_sha256
                    );
                    assert_eq!(recovered.reconcile_startup().unwrap(), 1);
                    recovered
                        .verify_package("app", &prepared.unwrap().package_sha256)
                        .unwrap();
                }
                "state_renamed" => {
                    let status = recovered.status("app").unwrap();
                    assert_eq!(
                        status.active.as_deref(),
                        Some(prepared.as_ref().unwrap().package_sha256.as_str())
                    );
                    assert_eq!(status.previous, vec![old.package_sha256]);
                }
                "trash_renamed" => {
                    assert_eq!(
                        recovered
                            .active_package("app")
                            .unwrap()
                            .unwrap()
                            .package_sha256,
                        old.package_sha256
                    );
                    assert_eq!(recovered.reconcile_startup().unwrap(), 1);
                    assert_eq!(recovered.package_directories("app").unwrap().len(), 1);
                }
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn pre_snapshot_package_loads_but_is_not_pinned_for_rollback() {
        let temp = tempfile::tempdir().unwrap();
        let store =
            ArtifactStore::new(temp.path().into(), 2, 0, 0, 1_000, 10 * 1024 * 1024).unwrap();
        let package = publish_fixture(&store, "app", "classic");
        std::fs::remove_file(
            package
                .library_path
                .parent()
                .unwrap()
                .join(PACKAGE_CONFIG_FILE),
        )
        .unwrap();

        assert!(store
            .record_activation("app", &package.library_path)
            .unwrap()
            .is_none());
        assert!(store.active_package("app").unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn artifact_state_symlink_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let store =
            ArtifactStore::new(temp.path().into(), 2, 0, 0, 1_000, 10 * 1024 * 1024).unwrap();
        let namespace = store.ensure_namespace("app").unwrap();
        let outside = temp.path().join("outside-state.json");
        std::fs::write(&outside, b"{}").unwrap();
        std::os::unix::fs::symlink(&outside, namespace.join(STATE_FILE)).unwrap();
        assert!(store
            .status("app")
            .unwrap_err()
            .to_string()
            .contains("plain file"));
        assert_eq!(std::fs::read(&outside).unwrap(), b"{}");
    }
}
