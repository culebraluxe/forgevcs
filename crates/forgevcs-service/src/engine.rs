//! Engine wiring: environment configuration → a running Scion engine (ADR-0002).
//!
//! One [`Engine`] owns one repository: its metadata store, its object store, its policy handle
//! and the upstream [`ServerApp`] that binds them (D3). [`Engines`] is the process-wide registry
//! and [`EngineHealth`] turns the registry into a readiness snapshot that `/ready` can read
//! without touching Neon on the request path (D5).

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result};
use scion_auth::{GitHubClient, Keyring};
use scion_config::{
    AuthSection, MetadataSection, ObjectsSection, PolicySection, ServerConfig, ServerSection,
    ServicePrincipalConfig,
};
use scion_core::{FaultRegistry, Principal, SystemClock};
use scion_obs::{Metrics, Readiness};
use scion_policy::PolicyHandle;
use scion_proto::{Endpoint, ScionClient};
use scion_server::{AppContext, ServerApp, ServerDeps};
use scion_store::{BlobStore, Storage};
use scion_store_crdb::{CrdbConfig, CrdbMetaStore};
use scion_store_s3::S3BlobStore;
use tempfile::TempDir;
use tokio::task::JoinHandle;

use crate::config::Config;
use crate::workspace::WorkspaceService;

/// GitHub REST base URL (upstream SPEC 6.5 default).
const DEFAULT_GITHUB_API: &str = "https://api.github.com";
/// GitHub OAuth base URL (upstream SPEC 6.5 default).
const DEFAULT_GITHUB_OAUTH: &str = "https://github.com";
/// Token lifetime (upstream SPEC 6.5 default: 12 h).
const DEFAULT_TOKEN_TTL: Duration = Duration::from_secs(12 * 60 * 60);
/// Objects below this size are inlined in metadata (upstream SPEC 6.5 default).
const INLINE_THRESHOLD_BYTES: usize = 65_536;
/// zstd level for stored objects (upstream SPEC 6.5 default).
const ZSTD_LEVEL: i32 = 3;
/// How often the readiness task re-probes its dependencies.
const PROBE_INTERVAL: Duration = Duration::from_secs(5);
/// Upper bound on one dependency probe; a slow Neon must not stall `/ready` (D5).
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// Key the readiness probe `HEAD`s. Content-addressed keys never collide with it.
const PROBE_KEY: &str = "forgevcs/readiness-probe";
/// Lifetime of the internal workspace client's token. Workspace operations are short-lived;
/// the container is restarted far more often than this.
const INTERNAL_TOKEN_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// One repository's engine.
pub struct Engine {
    repo: String,
    meta: Arc<CrdbMetaStore>,
    blobs: Arc<dyn BlobStore>,
    /// The internal `scion.v1` client the working-copy layer uses. It talks to this engine's
    /// own gRPC router over a Unix socket, so REST remains the only external surface.
    client: ScionClient,
    /// The WorkspaceService built on the internal client.
    workspaces: Arc<WorkspaceService>,
    /// The internal gRPC server task; aborted on drop.
    _grpc: JoinHandle<()>,
    /// Holds the Unix socket path; removing the directory removes the socket.
    _socket_dir: TempDir,
    /// Kept for its `Drop`: dropping the app aborts its background tasks.
    _app: ServerApp,
    ctx: Arc<AppContext>,
}

impl Drop for Engine {
    fn drop(&mut self) {
        self._grpc.abort();
    }
}

impl Engine {
    /// The repository this engine serves.
    pub fn repo(&self) -> &str {
        &self.repo
    }

    /// The handler context every endpoint will call into (D4).
    pub fn context(&self) -> Arc<AppContext> {
        Arc::clone(&self.ctx)
    }

    /// A client for the engine's internal `scion.v1` server (the working-copy layer).
    pub fn client(&self) -> ScionClient {
        self.client.clone()
    }

    /// The WorkspaceService for this repository.
    pub fn workspaces(&self) -> Arc<WorkspaceService> {
        Arc::clone(&self.workspaces)
    }

    /// Connect the metadata store, configure the object store, load the policy and the signing
    /// keys, then build the upstream server around them.
    ///
    /// `CrdbMetaStore::connect` applies the migrations and ensures the `repos` row, so a
    /// successful return means the schema exists on this database (W2).
    pub async fn connect(config: &Config, repo: &str) -> Result<Self> {
        let server = server_config(config, repo)?;
        let policy = PolicyHandle::load(&server.policy.path)
            .map_err(|e| anyhow::anyhow!("loading policy {}: {e}", server.policy.path.display()))?;
        let keyring = signing_keyring(config)?;
        let meta = Arc::new(
            CrdbMetaStore::connect(CrdbConfig {
                url: server.metadata.url.clone(),
                max_connections: server.metadata.max_connections,
                repo_name: repo.to_owned(),
                // Neon rejects plaintext connections; the `postgres` feature supplies the
                // webpki-rooted rustls configuration (ADR-0001 §7.1).
                tls: scion_store_crdb::dialect::tls_client_config(),
            })
            .await
            .context("connecting to the metadata store")?,
        );
        let s3 = config
            .s3_config()
            .context("object storage is not configured (FORGEVCS_BUCKET + endpoint)")?;
        let blobs: Arc<dyn BlobStore> =
            Arc::new(S3BlobStore::new(s3).context("configuring the object store")?);
        let storage = {
            let meta_store: Arc<dyn scion_store::MetaStore> = meta.clone();
            let blob_store: Arc<dyn BlobStore> = blobs.clone();
            Storage::new(meta_store, blob_store, FaultRegistry::new())
        };
        let app = ServerApp::build(ServerDeps {
            storage,
            config: server,
            policy,
            keyring,
            github: GitHubClient::new(DEFAULT_GITHUB_API),
            clock: Arc::new(SystemClock),
            metrics: Metrics::new(),
            registry: None,
        })
        .await
        .context("building the engine")?;
        let ctx = app.context();

        // The working-copy library is a client of the gRPC surface. Serve that surface on a
        // private Unix socket and connect a client to it, so the workspace layer uses Scion's
        // own implementation without making gRPC externally reachable (ADR-0002 D4 keeps the
        // REST façade in-process; this is the internal/local gRPC ADR-0001 §5.2 permits).
        let socket_dir =
            tempfile::tempdir().context("creating the internal gRPC socket directory")?;
        let socket_path = socket_dir.path().join("scion.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path)
            .context("binding the internal gRPC Unix socket")?;
        let (token, _claims) = ctx.keyring.issue(
            &Principal::Service("forgevcs-workspace".to_owned()),
            INTERNAL_TOKEN_TTL,
            ctx.clock.now(),
        )?;
        let router = app.router();
        let grpc = tokio::spawn(async move {
            let incoming = tokio_stream::wrappers::UnixListenerStream::new(listener);
            if let Err(error) = router.serve_with_incoming(incoming).await {
                tracing::error!(%error, "internal gRPC server stopped");
            }
        });
        let client = ScionClient::connect(&Endpoint::Unix(socket_path), None, Some(token))
            .await
            .context("connecting the internal gRPC client")?;
        let workspaces = Arc::new(
            WorkspaceService::connect(client.clone(), repo, config)
                .await
                .map_err(|error| anyhow::anyhow!("starting the workspace service: {error}"))?,
        );

        Ok(Self {
            repo: repo.to_owned(),
            meta,
            blobs,
            client,
            workspaces,
            _grpc: grpc,
            _socket_dir: socket_dir,
            _app: app,
            ctx,
        })
    }
}

/// Translate the process environment into the upstream `[server]` configuration.
///
/// Only the sections ForgeVCS sets are mapped; every tunable the work order does not name keeps
/// upstream's SPEC 6.5 default.
fn server_config(config: &Config, repo: &str) -> Result<ServerConfig> {
    let database_url = config
        .database_url
        .clone()
        .context("FORGEVCS_DATABASE_URL is not set")?;
    let s3 = config
        .s3_config()
        .context("object storage is not configured (FORGEVCS_BUCKET + endpoint)")?;
    Ok(ServerConfig {
        server: ServerSection {
            // The gRPC listener is not exposed in M0: handlers are called in-process (D4).
            listen: SocketAddr::from(([127, 0, 0, 1], 0)),
            tls_cert: PathBuf::new(),
            tls_key: PathBuf::new(),
            repo: repo.to_owned(),
        },
        metadata: MetadataSection {
            url: database_url,
            max_connections: config.max_connections(),
        },
        objects: ObjectsSection {
            endpoint: s3.endpoint,
            bucket: s3.bucket,
            prefix: s3.key_prefix,
            inline_threshold_bytes: INLINE_THRESHOLD_BYTES,
            zstd_level: ZSTD_LEVEL,
            region: s3.region,
            path_style: s3.path_style,
        },
        auth: AuthSection {
            github_client_id: config.github_client_id.clone().unwrap_or_default(),
            github_api_url: DEFAULT_GITHUB_API.to_owned(),
            github_oauth_base_url: DEFAULT_GITHUB_OAUTH.to_owned(),
            token_ttl: DEFAULT_TOKEN_TTL,
            signing_keys: config.signing_keys(),
            service_principals: parse_principals(config.service_principals.as_deref())?,
        },
        policy: PolicySection {
            path: config.policy_path(),
            reload: true,
        },
        limits: Default::default(),
        land: Default::default(),
        derive: Default::default(),
        redaction: Default::default(),
        observability: Default::default(),
        rate_limit: Default::default(),
    })
}

/// Parse `name=<sha256 hex>[,name=<sha256 hex>]…` into service principals.
///
/// These are the machine identities policy grants access to as `service:<name>` — the agents
/// (Maestro, an OpenCode harness) and importers.
fn parse_principals(raw: Option<&str>) -> Result<Vec<ServicePrincipalConfig>> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    raw.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            let (name, digest) = entry.split_once('=').with_context(|| {
                format!("service principal `{entry}` must be name=<sha256 hex>")
            })?;
            if name.is_empty() {
                anyhow::bail!("service principal entry `{entry}` has an empty name");
            }
            if digest.len() != 64 || !digest.chars().all(|c| c.is_ascii_hexdigit()) {
                anyhow::bail!("service principal `{name}` needs a 64-character hex SHA-256 digest");
            }
            Ok(ServicePrincipalConfig {
                name: name.to_owned(),
                secret_sha256: digest.to_ascii_lowercase(),
            })
        })
        .collect()
}

/// The token signing keys.
///
/// Production sets `FORGEVCS_SIGNING_KEY_PATH`. Without it the engine generates an ephemeral
/// key and says so: the service still starts, but tokens do not survive a restart.
fn signing_keyring(config: &Config) -> Result<Keyring> {
    let paths = config.signing_keys();
    if !paths.is_empty() {
        return Keyring::from_files(&paths)
            .map_err(|e| anyhow::anyhow!("loading signing keys {paths:?}: {e}"));
    }
    if let Some(pem) = config.signing_key_pem.as_deref() {
        // Vercel injects secrets as environment values, not files. Write the PEM to a private
        // temporary file, load it through the same parser as a path, then let the file drop.
        use std::io::Write as _;
        let mut file = tempfile::NamedTempFile::new()
            .context("creating a temporary file for FORGEVCS_SIGNING_KEY_PEM")?;
        file.write_all(pem.as_bytes())
            .context("writing FORGEVCS_SIGNING_KEY_PEM")?;
        file.flush().context("flushing FORGEVCS_SIGNING_KEY_PEM")?;
        let path = file.path().to_owned();
        let keyring = Keyring::from_files(std::slice::from_ref(&path))
            .map_err(|e| anyhow::anyhow!("loading FORGEVCS_SIGNING_KEY_PEM: {e}"))?;
        return Ok(keyring);
    }
    tracing::warn!(
        "FORGEVCS_SIGNING_KEY_PATH and FORGEVCS_SIGNING_KEY_PEM are unset: using an ephemeral \
         Ed25519 key; every token becomes invalid when this process restarts"
    );
    Ok(Keyring::generate_for_tests(1))
}

/// The process-wide registry of engines, keyed by repository name (D3).
#[derive(Default)]
pub struct Engines {
    repos: RwLock<BTreeMap<String, Arc<Engine>>>,
    boot_error: RwLock<Option<String>>,
    /// Serializes on-demand boots. Two concurrent `init` calls for the same new repo would
    /// otherwise both run the migrations and the policy/bootstrap work before either insert.
    boot_lock: tokio::sync::Mutex<()>,
}

impl Engines {
    /// Boot the repository named by `$FORGEVCS_REPO`.
    ///
    /// A failure is recorded rather than fatal (D5): a container that cannot reach Neon stays up
    /// and reports `/ready` 503 with this reason until the dependency recovers.
    pub async fn boot(config: &Config) -> Self {
        let engines = Self::default();
        if config.database_url.is_none() || !config.has_object_storage() {
            engines.record_error(
                "FORGEVCS_DATABASE_URL and object storage (FORGEVCS_BUCKET + endpoint) are \
                 required"
                    .to_owned(),
            );
            return engines;
        }
        match Engine::connect(config, config.repo()).await {
            Ok(engine) => {
                tracing::info!(repo = engine.repo(), "engine ready");
                engines.record(engine);
            }
            Err(error) => {
                let reason = format!("{error:#}");
                tracing::error!(reason = %reason, "engine did not start");
                engines.record_error(reason);
            }
        }
        engines
    }

    /// The engine for `repo`, once it has been booted.
    pub fn get(&self, repo: &str) -> Option<Arc<Engine>> {
        self.repos_read().get(repo).cloned()
    }

    /// Boot the engine for `repo` if this process has not already done so, and return it.
    ///
    /// `POST /repos/{repo}/init` is the only caller: `Engine::connect` creates the repository's
    /// `repos` row and upstream's `ServerApp::build` bootstraps the root changeset and `main`,
    /// so this is a create-or-get, not a lookup. The second lock check makes two concurrent
    /// initializations converge on the first engine instead of leaking a duplicate (D3).
    ///
    /// Reads and writes do not call this: a request for a repository that is not in the
    /// registry is answered 404 rather than silently creating it, so a typo cannot mint a
    /// repository.
    pub async fn get_or_boot(&self, config: &Config, repo: &str) -> Result<Arc<Engine>> {
        if let Some(engine) = self.get(repo) {
            return Ok(engine);
        }
        let _booting = self.boot_lock.lock().await;
        if let Some(engine) = self.get(repo) {
            return Ok(engine);
        }
        let engine = Arc::new(Engine::connect(config, repo).await?);
        tracing::info!(repo, "engine ready");
        let mut repos = self.repos_write();
        if let Some(existing) = repos.get(repo) {
            return Ok(Arc::clone(existing));
        }
        repos.insert(repo.to_owned(), Arc::clone(&engine));
        Ok(engine)
    }

    /// Every booted engine.
    pub fn all(&self) -> Vec<Arc<Engine>> {
        self.repos_read().values().cloned().collect()
    }

    /// Whether any engine is booted.
    pub fn is_empty(&self) -> bool {
        self.repos_read().is_empty()
    }

    /// Why boot failed, when it did.
    pub fn boot_error(&self) -> Option<String> {
        self.boot_error
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn record(&self, engine: Engine) {
        let repo = engine.repo.clone();
        self.repos_write().insert(repo, Arc::new(engine));
        *self
            .boot_error
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }

    fn record_error(&self, reason: String) {
        *self
            .boot_error
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(reason);
    }

    fn repos_read(&self) -> std::sync::RwLockReadGuard<'_, BTreeMap<String, Arc<Engine>>> {
        self.repos
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn repos_write(&self) -> std::sync::RwLockWriteGuard<'_, BTreeMap<String, Arc<Engine>>> {
        self.repos
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// The readiness snapshot `/ready` reads (D5).
pub struct EngineHealth {
    config: Config,
    engines: Arc<Engines>,
    snapshot: RwLock<Option<String>>,
    probes: AtomicU64,
}

impl EngineHealth {
    /// Probe once, then keep probing in the background; `Arc<Self>` is the router's state.
    pub async fn start(config: Config, engines: Arc<Engines>) -> Arc<Self> {
        let health = Arc::new(Self {
            config,
            engines,
            snapshot: RwLock::new(Some("readiness probe has not run yet".to_owned())),
            probes: AtomicU64::new(0),
        });
        health.probe().await;
        let task = Arc::clone(&health);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(PROBE_INTERVAL).await;
                task.probe().await;
            }
        });
        health
    }

    /// Re-probe every booted engine and replace the snapshot.
    pub async fn probe(&self) {
        let reason = self.diagnose().await;
        if let Some(reason) = &reason {
            tracing::warn!(reason = %reason, "readiness probe failed");
        }
        *self
            .snapshot
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = reason;
        self.probes.fetch_add(1, Ordering::Relaxed);
    }

    /// How many probes have run.
    pub fn probes(&self) -> u64 {
        self.probes.load(Ordering::Relaxed)
    }

    async fn diagnose(&self) -> Option<String> {
        if let Err(reason) = self.config.validate() {
            return Some(reason);
        }
        let engines = self.engines.all();
        if engines.is_empty() {
            return Some(
                self.engines
                    .boot_error()
                    .unwrap_or_else(|| "no engine is configured".to_owned()),
            );
        }
        for engine in engines {
            match tokio::time::timeout(PROBE_TIMEOUT, engine.meta.ping()).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    return Some(format!("metadata store ({}): {error}", engine.repo()));
                }
                Err(_) => {
                    return Some(format!(
                        "metadata store ({}) did not answer within {PROBE_TIMEOUT:?}",
                        engine.repo()
                    ));
                }
            }
            match tokio::time::timeout(PROBE_TIMEOUT, engine.blobs.exists(PROBE_KEY)).await {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => {
                    return Some(format!("object store ({}): {error}", engine.repo()));
                }
                Err(_) => {
                    return Some(format!(
                        "object store ({}) did not answer within {PROBE_TIMEOUT:?}",
                        engine.repo()
                    ));
                }
            }
        }
        None
    }
}

impl Readiness for EngineHealth {
    fn check(&self) -> std::result::Result<(), String> {
        self.config.validate()?;
        match self
            .snapshot
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
        {
            None => Ok(()),
            Some(reason) => Err(reason.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[tokio::test]
    async fn readiness_reports_an_unconfigured_service() {
        let health =
            EngineHealth::start(Config::from_lookup(|_| None), Arc::new(Engines::default())).await;
        let reason = health.check().expect_err("unconfigured");
        assert!(reason.contains("engine"), "{reason}");
        assert_eq!(health.probes(), 1);
    }

    #[test]
    fn principals_parse_normalize_and_reject_bad_entries() {
        let digest = "A".repeat(64);
        let parsed =
            parse_principals(Some(&format!("maestro={digest}, opencode={digest}"))).expect("valid");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].name, "maestro");
        assert_eq!(parsed[0].secret_sha256, "a".repeat(64));

        assert!(parse_principals(Some("maestro")).is_err());
        assert!(parse_principals(Some("=deadbeef")).is_err());
        assert!(parse_principals(Some("maestro=nothex")).is_err());
        assert_eq!(parse_principals(None).expect("none").len(), 0);
    }

    #[test]
    fn the_shipped_policy_parses() {
        // A policy the parser rejects means every engine refuses to boot, so the file in the
        // repository is part of the contract, not an example.
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../acl.toml");
        PolicyHandle::load(&path).expect("acl.toml parses");
    }

    #[test]
    fn signing_key_pem_is_loaded_through_the_same_parser() {
        let dir = tempfile::tempdir().expect("tempdir");
        let keyring = Keyring::generate_for_tests(1);
        let paths = keyring.write_pem_files(dir.path()).expect("write pem");
        let pem = std::fs::read_to_string(&paths[0]).expect("read pem");
        let config =
            Config::from_lookup(|key| (key == "FORGEVCS_SIGNING_KEY_PEM").then(|| pem.clone()));
        signing_keyring(&config).expect("PEM keyring loads");
    }

    #[test]
    fn server_config_requires_both_backends() {
        let config = Config::from_lookup(|key| match key {
            "FORGEVCS_DATABASE_URL" => Some("postgres://neon/db".to_owned()),
            _ => None,
        });
        assert!(server_config(&config, "forgevcs").is_err(), "no bucket");

        let config = Config::from_lookup(|key| match key {
            "FORGEVCS_DATABASE_URL" => Some("postgres://neon/db".to_owned()),
            "FORGEVCS_BUCKET" => Some("objects".to_owned()),
            "AWS_REGION" => Some("aws-us-east-2".to_owned()),
            "AWS_ENDPOINT_URL_S3" => Some("https://s3.example".to_owned()),
            _ => None,
        });
        let server = server_config(&config, "forgevcs").expect("configured");
        assert_eq!(server.server.repo, "forgevcs");
        assert_eq!(server.metadata.url, "postgres://neon/db");
        assert_eq!(server.objects.bucket, "objects");
        assert!(server.objects.path_style);
        assert!(server.policy.reload);
        assert!(server.auth.signing_keys.is_empty());
    }
}
