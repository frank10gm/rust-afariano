use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use sqlx::PgPool;
use tokio::sync::{Notify, Semaphore, broadcast};

use crate::accounts::Limiter;
use crate::config::Config;
use crate::logs::LogHub;
use crate::models::{REPO_COLS, Repo};

pub type Shared = Arc<AppState>;

/// A pipeline or one of its jobs changed. Pages showing it fetch themselves again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PipelineChanged {
    pub repo_id: i64,
    pub pipeline_id: i64,
}

pub struct AppState {
    pub cfg: Config,
    pub db: PgPool,
    pub http: reqwest::Client,
    /// Woken when a job becomes pending: long-polling runners retry.
    pub jobs_ready: Notify,
    pub logs: LogHub,
    /// Bounds concurrent pack building (clones/fetches), the memory-heavy part.
    pub pack_slots: Arc<Semaphore>,
    /// One push at a time per repo, so before/after ref snapshots are exact.
    push_locks: Mutex<HashMap<i64, Arc<tokio::sync::Mutex<()>>>>,
    /// Encrypts CI/CD variable values at rest.
    pub cipher: crate::secrets::Cipher,
    /// One-time first-admin link while there are no accounts.
    pub setup_token: Mutex<Option<String>>,
    /// Failed password sign-ins per email.
    pub limiter: Limiter,
    /// Argon2 hashing is memory hungry: two at a time.
    pub hash_slots: Semaphore,
    /// OpenID provider metadata, fetched on first use.
    pub oidc: tokio::sync::Mutex<Option<crate::auth::OidcProvider>>,
    /// Mirrors with a background sync waiting to start (webhooks coalesce into it).
    pub mirror_queued: Mutex<HashSet<i64>>,
    /// Pipelines whose status changed, for the commit status worker.
    pub status_tx: tokio::sync::mpsc::UnboundedSender<i64>,
    pub status_rx: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<i64>>>,
    /// Every pipeline change, for the live pages (server-sent events).
    pub pipeline_events: broadcast::Sender<PipelineChanged>,
    /// Bumped on every pipeline change. Pages carry the value they were rendered at,
    /// so a stream opened later knows whether they are behind. Starts at the clock
    /// (microseconds), so a page from before a restart never matches.
    change_seq: AtomicU64,
}

impl AppState {
    pub fn new(cfg: Config, db: PgPool) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .user_agent(concat!("rust-afariano/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let cipher = crate::secrets::Cipher::load(cfg.secret_key.as_deref(), &cfg.data_dir)?;
        let (status_tx, status_rx) = tokio::sync::mpsc::unbounded_channel();
        Ok(Self {
            mirror_queued: Mutex::new(HashSet::new()),
            status_tx,
            status_rx: Mutex::new(Some(status_rx)),
            pipeline_events: broadcast::channel(256).0,
            change_seq: AtomicU64::new(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_micros() as u64)
                    .unwrap_or(1),
            ),
            cipher,
            setup_token: Mutex::new(None),
            limiter: Limiter::default(),
            hash_slots: Semaphore::new(2),
            oidc: tokio::sync::Mutex::new(None),
            logs: LogHub::new(cfg.data_dir.join("logs")),
            pack_slots: Arc::new(Semaphore::new(cfg.max_packs.max(1))),
            cfg,
            db,
            http,
            jobs_ready: Notify::new(),
            push_locks: Mutex::new(HashMap::new()),
        })
    }

    /// Call after the change is committed, so pages fetched on the ping see it.
    pub fn pipeline_changed(&self, repo_id: i64, pipeline_id: i64) {
        self.change_seq.fetch_add(1, Ordering::SeqCst);
        // No receivers (no live page open) is not an error.
        let _ = self.pipeline_events.send(PipelineChanged { repo_id, pipeline_id });
    }

    /// Read before loading what a live page shows (see `change_seq`).
    pub fn change_seq(&self) -> u64 {
        self.change_seq.load(Ordering::SeqCst)
    }

    pub fn close_setup(&self) {
        *self.setup_token.lock().expect("setup token") = None;
    }

    pub fn push_lock(&self, repo_id: i64) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self.push_locks.lock().expect("push lock map");
        map.entry(repo_id).or_default().clone()
    }

    pub async fn repo_by_path(&self, namespace: &str, name: &str) -> sqlx::Result<Option<Repo>> {
        let name = name.strip_suffix(".git").unwrap_or(name);
        sqlx::query_as::<_, Repo>(sql!(
            "SELECT {REPO_COLS} FROM repos WHERE lower(namespace) = lower($1) AND lower(name) = lower($2)"
        ))
        .bind(namespace)
        .bind(name)
        .fetch_optional(&self.db)
        .await
    }

    pub async fn repo_by_id(&self, id: i64) -> sqlx::Result<Repo> {
        sqlx::query_as::<_, Repo>(sql!("SELECT {REPO_COLS} FROM repos WHERE id = $1"))
            .bind(id)
            .fetch_one(&self.db)
            .await
    }

    pub fn git(&self, repo_id: i64) -> crate::git::Git {
        crate::git::Git::new(&self.cfg.data_dir, repo_id)
    }
}
