//! Repository permissions enforcement.
//!
//! Every read path asks one [`Checker`] for the viewer's [`RepoAccess`] and filters with it:
//! search filters its in-memory catalog with [`RepoAccess::can_read`], and lookups by id or
//! name go through the repo store's `*_authorized` methods, which apply the same rule in SQL.
//! An unreadable repository is indistinguishable from a missing one.
//!
//! The rule: site admins read everything (unless `authz.enforceForSiteAdmins`); everyone else
//! reads the unrestricted repositories (SQL `repo_restricted()` is false) plus the ones granted
//! to them in `user_repo_permissions`.
//!
//! The checker caches the unrestricted set for [`UNRESTRICTED_TTL`] and each user's grants
//! (the materialized `user_perm_bitmaps` row) for [`USER_TTL`]. [`Checker::run_listener`]
//! drops cached entries on `NOTIFY authz`, so grants and revocations apply at once; the TTLs
//! bound staleness when notifications are lost.
//!
//! [`sync`] fills the `sync` grants from the code hosts (GitLab `oauth` and `username`
//! identity providers, user- and repo-centric); [`worker`] runs it from the
//! `permission_sync_jobs` queue with a leader-elected scheduler.

mod feature;
pub mod sync;
pub mod worker;

pub use feature::PermsSyncFeature;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use roaring::RoaringBitmap;
use sourcegull_authn::Actor;
use sourcegull_config::SiteConfigHandle;
use sourcegull_db::notify::channels;
use sourcegull_db::{Db, DbError, authz_payload};
use sourcegull_types::{RepoAccess, UserId};
use tokio_util::sync::CancellationToken;

/// How long the unrestricted set is reused. New repositories become visible to non-admins
/// within this time (the set fails closed).
pub const UNRESTRICTED_TTL: Duration = Duration::from_secs(5);
/// How long a user's grants are reused without a notification.
pub const USER_TTL: Duration = Duration::from_secs(60);
/// Users whose grants are cached.
const USER_CACHE_CAPACITY: u64 = 100_000;

/// Resolves who may read which repositories. Cheap to clone; clones share caches.
#[derive(Debug, Clone)]
pub struct Checker {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    db: Db,
    config: SiteConfigHandle,
    unrestricted: tokio::sync::Mutex<Option<Unrestricted>>,
    users: moka::future::Cache<UserId, Arc<RoaringBitmap>>,
    /// Bumped when user grants are invalidated, so a load that raced it is not cached.
    users_generation: AtomicU64,
    /// Bumped when the unrestricted set is invalidated.
    unrestricted_generation: AtomicU64,
}

#[derive(Debug)]
struct Unrestricted {
    loaded_at: Instant,
    user_mapping: bool,
    generation: u64,
    bitmap: Arc<RoaringBitmap>,
}

impl Checker {
    /// A checker reading permissions from `db` and the rules from `config`.
    pub fn new(db: Db, config: SiteConfigHandle) -> Self {
        Self {
            inner: Arc::new(Inner {
                db,
                config,
                unrestricted: tokio::sync::Mutex::new(None),
                users: moka::future::Cache::builder()
                    .max_capacity(USER_CACHE_CAPACITY)
                    .time_to_live(USER_TTL)
                    .build(),
                users_generation: AtomicU64::new(0),
                unrestricted_generation: AtomicU64::new(0),
            }),
        }
    }

    /// The site config the checker reads its rules from.
    pub fn config(&self) -> &SiteConfigHandle {
        &self.inner.config
    }

    /// The repositories `actor` (`None`: anonymous) may read.
    pub async fn access(&self, actor: Option<&Actor>) -> Result<RepoAccess, DbError> {
        let config = self.inner.config.load();
        let rules = &config.value;
        if let Some(a) = actor
            && a.site_admin
            && !rules.authz_enforce_for_site_admins
        {
            return Ok(RepoAccess::All);
        }
        let user_mapping = rules.permissions_user_mapping.enabled;
        let unrestricted = self.unrestricted(user_mapping).await?;
        let Some(actor) = actor else {
            return Ok(RepoAccess::anonymous(user_mapping, unrestricted));
        };
        Ok(RepoAccess::Restricted {
            user_id: Some(actor.user_id),
            user_mapping,
            unrestricted,
            granted: self.granted(actor.user_id).await?,
        })
    }

    async fn unrestricted(&self, user_mapping: bool) -> Result<Arc<RoaringBitmap>, DbError> {
        let mut slot = self.inner.unrestricted.lock().await;
        let generation = self.inner.unrestricted_generation.load(Ordering::Acquire);
        if let Some(u) = slot.as_ref()
            && u.user_mapping == user_mapping
            && u.generation == generation
            && u.loaded_at.elapsed() < UNRESTRICTED_TTL
        {
            return Ok(u.bitmap.clone());
        }
        let bitmap = Arc::new(
            self.inner
                .db
                .perms()
                .unrestricted_repos(user_mapping)
                .await?,
        );
        *slot = Some(Unrestricted {
            loaded_at: Instant::now(),
            user_mapping,
            generation,
            bitmap: bitmap.clone(),
        });
        Ok(bitmap)
    }

    async fn granted(&self, user: UserId) -> Result<Arc<RoaringBitmap>, DbError> {
        if let Some(b) = self.inner.users.get(&user).await {
            return Ok(b);
        }
        let generation = self.inner.users_generation.load(Ordering::Acquire);
        let bitmap = Arc::new(self.inner.db.perms().user_bitmap(user).await?);
        if self.inner.users_generation.load(Ordering::Acquire) == generation {
            self.inner.users.insert(user, bitmap.clone()).await;
        }
        Ok(bitmap)
    }

    /// Forgets `user`'s cached grants.
    pub async fn invalidate_user(&self, user: UserId) {
        self.inner.users_generation.fetch_add(1, Ordering::AcqRel);
        self.inner.users.invalidate(&user).await;
    }

    /// Forgets every cached grant and the unrestricted set.
    pub fn invalidate_all(&self) {
        self.inner.users_generation.fetch_add(1, Ordering::AcqRel);
        self.inner.users.invalidate_all();
        self.invalidate_unrestricted();
    }

    /// Forgets the cached unrestricted set: the next [`access`](Self::access) reloads it.
    pub fn invalidate_unrestricted(&self) {
        self.inner
            .unrestricted_generation
            .fetch_add(1, Ordering::AcqRel);
    }

    /// Applies one `NOTIFY authz` payload.
    pub async fn on_notify(&self, payload: &str) {
        if payload == authz_payload::UNRESTRICTED {
            self.invalidate_unrestricted();
        } else if let Some(id) = payload
            .strip_prefix(authz_payload::USER_PREFIX)
            .and_then(|s| s.parse::<i64>().ok())
        {
            self.invalidate_user(UserId(id)).await;
        } else {
            self.invalidate_all();
        }
    }

    /// Listens on `authz` and invalidates cached permissions until `cancel` fires. Everything
    /// is invalidated whenever the listener (re)connects, since notifications sent while
    /// disconnected are lost.
    pub async fn run_listener(self, cancel: CancellationToken) {
        let mut sub = tokio::select! {
            () = cancel.cancelled() => return,
            s = sourcegull_pubsub::subscribe(self.inner.db.pool(), &[channels::AUTHZ]) => s,
        };
        loop {
            tokio::select! {
                () = cancel.cancelled() => return,
                e = sub.recv() => match e {
                    sourcegull_pubsub::Event::Notification { payload, .. } => self.on_notify(&payload).await,
                    sourcegull_pubsub::Event::Resync => self.invalidate_all(),
                },
            }
        }
    }
}
