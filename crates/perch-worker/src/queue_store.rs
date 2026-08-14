//! Host-owned durable queue state.
//!
//! Queue rows are global to one Perch installation and carry deployment
//! identity explicitly. Applications never choose a table/schema or claim
//! rows themselves. Every state transition is one PostgreSQL statement so a
//! cancelled Rust future cannot leave an open application transaction.

use anyhow::{anyhow, Context, Result};
use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod};
use nanoid::nanoid;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;
use tokio_postgres::NoTls;

const SCHEMA_VERSION: i32 = 1;
const MIGRATION_LOCK: i64 = 0x5045_5243_4851_5545;

const MIGRATION_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS perch_queue_schema_migrations (
    version INTEGER PRIMARY KEY,
    installed_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);

CREATE TABLE IF NOT EXISTS perch_queue_messages (
    id TEXT PRIMARY KEY,
    deployment TEXT NOT NULL,
    queue_name TEXT NOT NULL,
    payload BYTEA NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    max_attempts INTEGER NOT NULL CHECK (max_attempts > 0),
    available_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    lease_owner TEXT,
    lease_token TEXT,
    lease_expires_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    last_error TEXT,
    CHECK (
        (lease_owner IS NULL AND lease_token IS NULL AND lease_expires_at IS NULL)
        OR
        (lease_owner IS NOT NULL AND lease_token IS NOT NULL AND lease_expires_at IS NOT NULL)
    )
);

CREATE INDEX IF NOT EXISTS perch_queue_messages_claim_idx
    ON perch_queue_messages
        (deployment, queue_name, available_at, lease_expires_at, created_at, id);

CREATE TABLE IF NOT EXISTS perch_queue_dead_letters (
    id TEXT PRIMARY KEY,
    deployment TEXT NOT NULL,
    queue_name TEXT NOT NULL,
    payload BYTEA NOT NULL,
    attempts INTEGER NOT NULL,
    max_attempts INTEGER NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    failed_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    final_error TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS perch_queue_dead_letters_lookup_idx
    ON perch_queue_dead_letters (deployment, queue_name, failed_at, id);

INSERT INTO perch_queue_schema_migrations(version)
VALUES (1)
ON CONFLICT (version) DO NOTHING;
"#;

const CLAIM_SQL: &str = r#"
WITH candidate AS (
    SELECT
        id,
        lease_expires_at IS NOT NULL AND lease_expires_at <= clock_timestamp()
            AS expired_lease
    FROM perch_queue_messages
    WHERE deployment = $1
      AND queue_name = $2
      AND available_at <= clock_timestamp()
      AND (lease_expires_at IS NULL OR lease_expires_at <= clock_timestamp())
    ORDER BY available_at, created_at, id
    FOR UPDATE SKIP LOCKED
    LIMIT 1
)
UPDATE perch_queue_messages AS message
SET lease_owner = $3,
    lease_token = $4,
    lease_expires_at = clock_timestamp() + $5::bigint * interval '1 millisecond',
    attempts = message.attempts + 1
FROM candidate
WHERE message.id = candidate.id
RETURNING
    message.id,
    message.deployment,
    message.queue_name,
    message.payload,
    message.attempts,
    message.max_attempts,
    message.lease_token,
    candidate.expired_lease
"#;

const DEAD_LETTER_SQL: &str = r#"
WITH removed AS (
    DELETE FROM perch_queue_messages
    WHERE id = $1 AND lease_token = $2 AND attempts = $3
    RETURNING id, deployment, queue_name, payload, attempts, max_attempts, created_at
)
INSERT INTO perch_queue_dead_letters (
    id, deployment, queue_name, payload, attempts, max_attempts,
    created_at, failed_at, final_error
)
SELECT id, deployment, queue_name, payload, attempts, max_attempts,
       created_at, clock_timestamp(), $4
FROM removed
ON CONFLICT (id) DO NOTHING
"#;

#[derive(Clone)]
pub struct QueueStore {
    pool: Pool,
}

#[derive(Debug, Clone)]
pub struct EnqueueMessage {
    pub deployment: String,
    pub queue_name: String,
    pub payload: Vec<u8>,
    pub delay: Duration,
    pub max_attempts: u32,
}

#[derive(Debug, Clone)]
pub struct ClaimedMessage {
    pub id: String,
    pub deployment: String,
    pub queue_name: String,
    pub payload: Vec<u8>,
    /// One-based delivery attempt reserved by this claim.
    pub attempt: u32,
    pub max_attempts: u32,
    pub lease_token: String,
    pub recovered_expired_lease: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryOutcome {
    Scheduled { delay: Duration },
    DeadLettered,
    LeaseLost,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct QueueStats {
    pub total_depth: u64,
    pub visible_depth: u64,
    pub active_leases: u64,
    pub oldest_visible_age_seconds: f64,
    pub dead_letters: u64,
}

/// Eventually consistent connection-pool health. This mirrors deadpool's
/// atomic status snapshot without exposing the pool implementation to the
/// daemon or applications.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueuePoolStatus {
    pub max_size: usize,
    pub size: usize,
    pub available: usize,
    pub waiting: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeadLetterSummary {
    pub id: String,
    pub deployment: String,
    pub queue_name: String,
    pub payload_bytes: u64,
    pub attempts: u32,
    pub max_attempts: u32,
    pub created_at_ms: i64,
    pub failed_at_ms: i64,
    pub final_error: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeadLetterReplayOutcome {
    Replayed,
    NotFound,
    AlreadyLive,
}

#[derive(Debug, Clone)]
pub struct EnqueuePolicy {
    pub max_payload_bytes: usize,
    pub max_attempts: u32,
    pub max_delay: Duration,
}

#[derive(Debug, Clone)]
struct EnqueueContext {
    deployment: String,
    queues: HashMap<String, EnqueuePolicy>,
}

struct QueueGateway {
    store: Arc<QueueStore>,
    runtime: tokio::runtime::Handle,
    contexts: RwLock<HashMap<u64, EnqueueContext>>,
}

static QUEUE_GATEWAY: OnceLock<QueueGateway> = OnceLock::new();

/// Install the process-wide callback used by `@perch/runtime`. The callback is
/// synchronous because `queue.send()` resolves only after PostgreSQL commits.
/// Perry application calls run on dedicated non-Tokio executor threads, so
/// blocking that thread on the daemon/worker runtime is safe and preserves
/// intuitive enqueue durability.
pub fn initialize_queue_gateway(
    store: Arc<QueueStore>,
    runtime: tokio::runtime::Handle,
) -> Result<()> {
    if let Some(existing) = QUEUE_GATEWAY.get() {
        if !Arc::ptr_eq(&existing.store, &store) {
            return Err(anyhow!(
                "queue gateway was already initialized with another store"
            ));
        }
    } else {
        QUEUE_GATEWAY
            .set(QueueGateway {
                store,
                runtime,
                contexts: RwLock::new(HashMap::new()),
            })
            .map_err(|_| anyhow!("queue gateway initialization raced"))?;
    }
    crate::register_queue_enqueue_callback(queue_enqueue_callback)
}

pub fn register_enqueue_context(
    id: u64,
    deployment: String,
    queues: HashMap<String, EnqueuePolicy>,
) -> Result<()> {
    if id == 0 {
        return Err(anyhow!("deployment enqueue context ID must be non-zero"));
    }
    validate_identity("deployment", &deployment)?;
    for (queue, policy) in &queues {
        validate_identity("queue", queue)?;
        if policy.max_payload_bytes == 0 || policy.max_attempts == 0 {
            return Err(anyhow!("invalid enqueue policy for queue {queue:?}"));
        }
    }
    let gateway = QUEUE_GATEWAY
        .get()
        .ok_or_else(|| anyhow!("queue gateway is not initialized"))?;
    let mut contexts = gateway
        .contexts
        .write()
        .map_err(|_| anyhow!("queue gateway context lock poisoned"))?;
    if contexts.contains_key(&id) {
        return Err(anyhow!("deployment enqueue context {id} already exists"));
    }
    contexts.insert(id, EnqueueContext { deployment, queues });
    Ok(())
}

pub fn unregister_enqueue_context(id: u64) {
    if let Some(gateway) = QUEUE_GATEWAY.get() {
        if let Ok(mut contexts) = gateway.contexts.write() {
            contexts.remove(&id);
        }
    }
}

unsafe extern "C" fn queue_enqueue_callback(
    deployment_id: u64,
    queue: *const u8,
    queue_len: usize,
    payload: *const u8,
    payload_len: usize,
    delay_ms: u64,
) -> i32 {
    match queue_enqueue_callback_inner(
        deployment_id,
        queue,
        queue_len,
        payload,
        payload_len,
        delay_ms,
    ) {
        Ok(()) => 0,
        Err(code) => code,
    }
}

unsafe fn queue_enqueue_callback_inner(
    deployment_id: u64,
    queue: *const u8,
    queue_len: usize,
    payload: *const u8,
    payload_len: usize,
    delay_ms: u64,
) -> std::result::Result<(), i32> {
    if (queue.is_null() && queue_len != 0) || (payload.is_null() && payload_len != 0) {
        return Err(-10);
    }
    // Provider string headers guarantee these exact lengths for the duration
    // of the callback. Copy before touching async state.
    let queue_bytes = if queue_len == 0 {
        &[]
    } else {
        std::slice::from_raw_parts(queue, queue_len)
    };
    let payload_bytes = if payload_len == 0 {
        &[]
    } else {
        std::slice::from_raw_parts(payload, payload_len)
    };
    let queue = std::str::from_utf8(queue_bytes)
        .map_err(|_| -11)?
        .to_string();
    let payload = payload_bytes.to_vec();
    let gateway = QUEUE_GATEWAY.get().ok_or(-12)?;
    let context = gateway
        .contexts
        .read()
        .map_err(|_| -13)?
        .get(&deployment_id)
        .cloned()
        .ok_or(-14)?;
    let policy = context.queues.get(&queue).cloned().ok_or(-15)?;
    if payload.len() > policy.max_payload_bytes {
        return Err(-16);
    }
    let delay = Duration::from_millis(delay_ms);
    if delay > policy.max_delay {
        return Err(-17);
    }
    let message = EnqueueMessage {
        deployment: context.deployment,
        queue_name: queue,
        payload,
        delay,
        max_attempts: policy.max_attempts,
    };
    gateway
        .runtime
        .block_on(gateway.store.enqueue(message))
        .map(|_| ())
        .map_err(|error| {
            tracing::error!(?error, "host-owned durable queue enqueue failed");
            -18
        })
}

impl QueueStore {
    /// Create a reconnecting pool, prove one connection, and install the
    /// queue schema before any deployment becomes visible.
    pub async fn connect(url: &str, max_connections: u32) -> Result<Self> {
        if max_connections == 0 {
            return Err(anyhow!("Postgres max_connections must be positive"));
        }
        let pg = url
            .parse::<tokio_postgres::Config>()
            .context("parsing Postgres queue URL")?;
        let manager = Manager::from_config(
            pg,
            NoTls,
            ManagerConfig {
                // Verify recycled connections so a network partition cannot
                // hand a known-dead connection to a lease transition.
                recycling_method: RecyclingMethod::Verified,
            },
        );
        let pool = Pool::builder(manager)
            .max_size(max_connections as usize)
            .build()
            .context("building Postgres queue pool")?;
        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    async fn migrate(&self) -> Result<()> {
        let client = self
            .pool
            .get()
            .await
            .context("connecting to Postgres queue store")?;
        let version = client
            .query_opt(
                "SELECT to_regclass('perch_queue_schema_migrations') IS NOT NULL",
                &[],
            )
            .await
            .context("probing Postgres queue schema")?
            .map(|row| row.get::<_, bool>(0))
            .unwrap_or(false);
        if version {
            let latest: Option<i32> = client
                .query_one(
                    "SELECT max(version) FROM perch_queue_schema_migrations",
                    &[],
                )
                .await
                .context("reading Postgres queue schema version")?
                .get(0);
            if latest.unwrap_or(0) > SCHEMA_VERSION {
                return Err(anyhow!(
                    "Postgres queue schema version {} is newer than supported version {}",
                    latest.unwrap_or(0),
                    SCHEMA_VERSION
                ));
            }
        }

        let migration = format!(
            "BEGIN; SELECT pg_advisory_xact_lock({MIGRATION_LOCK}); {MIGRATION_SQL} COMMIT;"
        );
        client
            .batch_execute(&migration)
            .await
            .context("installing Postgres queue schema")?;
        Ok(())
    }

    pub fn pool_status(&self) -> QueuePoolStatus {
        let status = self.pool.status();
        QueuePoolStatus {
            max_size: status.max_size,
            size: status.size,
            available: status.available,
            waiting: status.waiting,
        }
    }

    pub async fn enqueue(&self, message: EnqueueMessage) -> Result<String> {
        validate_identity("deployment", &message.deployment)?;
        validate_identity("queue", &message.queue_name)?;
        if message.max_attempts == 0 || message.max_attempts > i32::MAX as u32 {
            return Err(anyhow!(
                "queue max_attempts must be between 1 and {}",
                i32::MAX
            ));
        }
        let delay_ms = duration_millis_i64(message.delay)?;
        let id = nanoid!();
        let client = self.pool.get().await.context("getting queue connection")?;
        client
            .execute(
                r#"
                INSERT INTO perch_queue_messages (
                    id, deployment, queue_name, payload, attempts,
                    max_attempts, available_at
                )
                VALUES ($1, $2, $3, $4, 0, $5,
                        clock_timestamp() + $6::bigint * interval '1 millisecond')
                "#,
                &[
                    &id,
                    &message.deployment,
                    &message.queue_name,
                    &message.payload,
                    &(message.max_attempts as i32),
                    &delay_ms,
                ],
            )
            .await
            .context("enqueuing durable message")?;
        Ok(id)
    }

    pub async fn claim(
        &self,
        deployment: &str,
        queue_name: &str,
        lease_owner: &str,
        visibility_timeout: Duration,
    ) -> Result<Option<ClaimedMessage>> {
        validate_identity("deployment", deployment)?;
        validate_identity("queue", queue_name)?;
        validate_identity("lease owner", lease_owner)?;
        let lease_ms = duration_millis_i64(visibility_timeout)?;
        if lease_ms == 0 {
            return Err(anyhow!("queue visibility timeout must be positive"));
        }
        let lease_token = nanoid!();
        let client = self.pool.get().await.context("getting queue connection")?;
        let row = client
            .query_opt(
                CLAIM_SQL,
                &[
                    &deployment,
                    &queue_name,
                    &lease_owner,
                    &lease_token,
                    &lease_ms,
                ],
            )
            .await
            .context("claiming durable queue message")?;
        row.map(|row| {
            let attempts: i32 = row.get(4);
            let max_attempts: i32 = row.get(5);
            if attempts <= 0 || max_attempts <= 0 {
                return Err(anyhow!("queue row contains an invalid attempt count"));
            }
            Ok(ClaimedMessage {
                id: row.get(0),
                deployment: row.get(1),
                queue_name: row.get(2),
                payload: row.get(3),
                attempt: attempts as u32,
                max_attempts: max_attempts as u32,
                lease_token: row.get(6),
                recovered_expired_lease: row.get(7),
            })
        })
        .transpose()
    }

    pub async fn ack(&self, message: &ClaimedMessage) -> Result<bool> {
        let client = self.pool.get().await.context("getting queue connection")?;
        let removed = client
            .execute(
                "DELETE FROM perch_queue_messages WHERE id = $1 AND lease_token = $2 AND attempts = $3",
                &[&message.id, &message.lease_token, &(message.attempt as i32)],
            )
            .await
            .context("acknowledging durable queue message")?;
        Ok(removed == 1)
    }

    /// Release a claimed-but-undelivered message and undo the attempt reserved
    /// by `claim`. This is used when a generation is stopped between claim and
    /// application dispatch; ordinary application failures use `retry`.
    pub async fn release_without_attempt(
        &self,
        message: &ClaimedMessage,
        delay: Duration,
    ) -> Result<bool> {
        let delay_ms = duration_millis_i64(delay)?;
        let client = self.pool.get().await.context("getting queue connection")?;
        let changed = client
            .execute(
                r#"
                UPDATE perch_queue_messages
                SET attempts = GREATEST(attempts - 1, 0),
                    available_at = clock_timestamp() + $4::bigint * interval '1 millisecond',
                    lease_owner = NULL,
                    lease_token = NULL,
                    lease_expires_at = NULL
                WHERE id = $1 AND lease_token = $2 AND attempts = $3
                "#,
                &[
                    &message.id,
                    &message.lease_token,
                    &(message.attempt as i32),
                    &delay_ms,
                ],
            )
            .await
            .context("releasing undelivered queue message")?;
        Ok(changed == 1)
    }

    pub async fn retry(
        &self,
        message: &ClaimedMessage,
        error: &str,
        base_delay: Duration,
        max_delay: Duration,
    ) -> Result<RetryOutcome> {
        if message.attempt >= message.max_attempts {
            return self.dead_letter(message, error).await;
        }
        let delay = retry_delay(&message.id, message.attempt, base_delay, max_delay);
        let delay_ms = duration_millis_i64(delay)?;
        let error = bounded_error(error);
        let client = self.pool.get().await.context("getting queue connection")?;
        let changed = client
            .execute(
                r#"
                UPDATE perch_queue_messages
                SET available_at = clock_timestamp() + $4::bigint * interval '1 millisecond',
                    lease_owner = NULL,
                    lease_token = NULL,
                    lease_expires_at = NULL,
                    last_error = $5
                WHERE id = $1 AND lease_token = $2 AND attempts = $3
                "#,
                &[
                    &message.id,
                    &message.lease_token,
                    &(message.attempt as i32),
                    &delay_ms,
                    &error,
                ],
            )
            .await
            .context("scheduling durable queue retry")?;
        Ok(if changed == 1 {
            RetryOutcome::Scheduled { delay }
        } else {
            RetryOutcome::LeaseLost
        })
    }

    /// Preserve the active lease after an uncertain timeout. The delivery
    /// becomes claimable only when its visibility timeout expires, preventing
    /// an immediate duplicate while native in-process work may still run.
    pub async fn mark_lease_error(&self, message: &ClaimedMessage, error: &str) -> Result<bool> {
        let error = bounded_error(error);
        let client = self.pool.get().await.context("getting queue connection")?;
        let changed = client
            .execute(
                r#"
                UPDATE perch_queue_messages
                SET last_error = $4
                WHERE id = $1 AND lease_token = $2 AND attempts = $3
                "#,
                &[
                    &message.id,
                    &message.lease_token,
                    &(message.attempt as i32),
                    &error,
                ],
            )
            .await
            .context("recording uncertain queue delivery timeout")?;
        Ok(changed == 1)
    }

    pub async fn dead_letter(&self, message: &ClaimedMessage, error: &str) -> Result<RetryOutcome> {
        let error = bounded_error(error);
        let client = self.pool.get().await.context("getting queue connection")?;
        let inserted = client
            .execute(
                DEAD_LETTER_SQL,
                &[
                    &message.id,
                    &message.lease_token,
                    &(message.attempt as i32),
                    &error,
                ],
            )
            .await
            .context("moving durable queue message to the dead-letter table")?;
        Ok(if inserted == 1 {
            RetryOutcome::DeadLettered
        } else {
            RetryOutcome::LeaseLost
        })
    }

    pub async fn stats(&self, deployment: &str, queue_name: &str) -> Result<QueueStats> {
        let client = self.pool.get().await.context("getting queue connection")?;
        let pending = client
            .query_one(
                r#"
                SELECT
                    count(*)::bigint,
                    count(*) FILTER (
                        WHERE available_at <= clock_timestamp()
                          AND (lease_expires_at IS NULL OR lease_expires_at <= clock_timestamp())
                    )::bigint,
                    count(*) FILTER (
                        WHERE lease_expires_at > clock_timestamp()
                    )::bigint,
                    COALESCE(
                        EXTRACT(EPOCH FROM (
                            clock_timestamp() - min(created_at) FILTER (
                                WHERE available_at <= clock_timestamp()
                                  AND (lease_expires_at IS NULL OR lease_expires_at <= clock_timestamp())
                            )
                        )),
                        0
                    )::float8
                FROM perch_queue_messages
                WHERE deployment = $1 AND queue_name = $2
                "#,
                &[&deployment, &queue_name],
            )
            .await
            .context("reading durable queue statistics")?;
        let dead_letters: i64 = client
            .query_one(
                "SELECT count(*)::bigint FROM perch_queue_dead_letters WHERE deployment = $1 AND queue_name = $2",
                &[&deployment, &queue_name],
            )
            .await
            .context("reading dead-letter statistics")?
            .get(0);
        Ok(QueueStats {
            total_depth: nonnegative_i64(pending.get(0)),
            visible_depth: nonnegative_i64(pending.get(1)),
            active_leases: nonnegative_i64(pending.get(2)),
            oldest_visible_age_seconds: pending.get::<_, f64>(3).max(0.0),
            dead_letters: nonnegative_i64(dead_letters),
        })
    }

    pub async fn prune_dead_letters(
        &self,
        deployment: &str,
        queue_name: &str,
        retention: Duration,
    ) -> Result<u64> {
        let retention_ms = duration_millis_i64(retention)?;
        let client = self.pool.get().await.context("getting queue connection")?;
        client
            .execute(
                r#"
                DELETE FROM perch_queue_dead_letters
                WHERE deployment = $1 AND queue_name = $2
                  AND failed_at < clock_timestamp() - $3::bigint * interval '1 millisecond'
                "#,
                &[&deployment, &queue_name, &retention_ms],
            )
            .await
            .context("pruning durable queue dead letters")
    }

    pub async fn list_dead_letters(
        &self,
        deployment: &str,
        queue_name: &str,
        limit: u32,
        before: Option<(i64, &str)>,
    ) -> Result<Vec<DeadLetterSummary>> {
        validate_identity("deployment", deployment)?;
        validate_identity("queue", queue_name)?;
        if !(1..=200).contains(&limit) {
            return Err(anyhow!("dead-letter page limit must be between 1 and 200"));
        }
        let (before_ms, before_id) = before
            .map(|(failed_at_ms, id)| (Some(failed_at_ms), Some(id)))
            .unwrap_or((None, None));
        let limit = i64::from(limit);
        let client = self.pool.get().await.context("getting queue connection")?;
        let rows = client
            .query(
                r#"
                SELECT
                    id, deployment, queue_name, octet_length(payload)::bigint,
                    attempts, max_attempts,
                    (EXTRACT(EPOCH FROM created_at) * 1000)::bigint,
                    (EXTRACT(EPOCH FROM failed_at) * 1000)::bigint,
                    final_error
                FROM perch_queue_dead_letters
                WHERE deployment = $1 AND queue_name = $2
                  AND (
                    $3::bigint IS NULL
                    OR (failed_at, id) < (
                        to_timestamp($3::double precision / 1000.0),
                        $4::text
                    )
                  )
                ORDER BY failed_at DESC, id DESC
                LIMIT $5
                "#,
                &[&deployment, &queue_name, &before_ms, &before_id, &limit],
            )
            .await
            .context("listing durable queue dead letters")?;
        rows.into_iter().map(dead_letter_summary_from_row).collect()
    }

    /// Atomically move one dead letter back to the live queue. The original
    /// delivery ID and attempt budget are retained, while attempts restart at
    /// zero. If that ID is already live the DLQ row is left untouched.
    pub async fn replay_dead_letter(
        &self,
        deployment: &str,
        queue_name: &str,
        id: &str,
    ) -> Result<DeadLetterReplayOutcome> {
        validate_identity("deployment", deployment)?;
        validate_identity("queue", queue_name)?;
        validate_identity("message", id)?;
        let mut client = self.pool.get().await.context("getting queue connection")?;
        let transaction = client
            .transaction()
            .await
            .context("starting dead-letter replay transaction")?;
        let inserted = transaction
            .execute(
                r#"
                INSERT INTO perch_queue_messages (
                    id, deployment, queue_name, payload, attempts, max_attempts,
                    available_at, created_at
                )
                SELECT id, deployment, queue_name, payload, 0, max_attempts,
                       clock_timestamp(), created_at
                FROM perch_queue_dead_letters
                WHERE id = $1 AND deployment = $2 AND queue_name = $3
                ON CONFLICT (id) DO NOTHING
                "#,
                &[&id, &deployment, &queue_name],
            )
            .await
            .context("restoring dead letter to durable queue")?;
        if inserted == 0 {
            let source_exists: bool = transaction
                .query_one(
                    r#"
                    SELECT EXISTS (
                        SELECT 1 FROM perch_queue_dead_letters
                        WHERE id = $1 AND deployment = $2 AND queue_name = $3
                    )
                    "#,
                    &[&id, &deployment, &queue_name],
                )
                .await
                .context("checking no-op dead-letter replay")?
                .get(0);
            transaction
                .rollback()
                .await
                .context("rolling back no-op dead-letter replay")?;
            return Ok(if source_exists {
                DeadLetterReplayOutcome::AlreadyLive
            } else {
                DeadLetterReplayOutcome::NotFound
            });
        }
        let removed = transaction
            .execute(
                r#"
                DELETE FROM perch_queue_dead_letters
                WHERE id = $1 AND deployment = $2 AND queue_name = $3
                "#,
                &[&id, &deployment, &queue_name],
            )
            .await
            .context("removing replayed dead letter")?;
        if removed != 1 {
            transaction
                .rollback()
                .await
                .context("rolling back incomplete dead-letter replay")?;
            return Err(anyhow!("dead-letter replay lost its source row"));
        }
        transaction
            .commit()
            .await
            .context("committing dead-letter replay")?;
        Ok(DeadLetterReplayOutcome::Replayed)
    }

    pub async fn purge_dead_letter(
        &self,
        deployment: &str,
        queue_name: &str,
        id: &str,
    ) -> Result<bool> {
        validate_identity("deployment", deployment)?;
        validate_identity("queue", queue_name)?;
        validate_identity("message", id)?;
        let client = self.pool.get().await.context("getting queue connection")?;
        let removed = client
            .execute(
                r#"
                DELETE FROM perch_queue_dead_letters
                WHERE id = $1 AND deployment = $2 AND queue_name = $3
                "#,
                &[&id, &deployment, &queue_name],
            )
            .await
            .context("purging durable queue dead letter")?;
        Ok(removed == 1)
    }
}

fn dead_letter_summary_from_row(row: tokio_postgres::Row) -> Result<DeadLetterSummary> {
    let payload_bytes: i64 = row.get(3);
    let attempts: i32 = row.get(4);
    let max_attempts: i32 = row.get(5);
    if payload_bytes < 0 || attempts < 0 || max_attempts <= 0 {
        return Err(anyhow!("invalid attempt counters in dead-letter row"));
    }
    Ok(DeadLetterSummary {
        id: row.get(0),
        deployment: row.get(1),
        queue_name: row.get(2),
        payload_bytes: payload_bytes as u64,
        attempts: attempts as u32,
        max_attempts: max_attempts as u32,
        created_at_ms: row.get(6),
        failed_at_ms: row.get(7),
        final_error: row.get(8),
    })
}

fn validate_identity(kind: &str, value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 255 || value.contains('\0') {
        return Err(anyhow!("invalid {kind} identity"));
    }
    Ok(())
}

fn duration_millis_i64(duration: Duration) -> Result<i64> {
    i64::try_from(duration.as_millis()).context("queue duration exceeds PostgreSQL range")
}

fn bounded_error(error: &str) -> String {
    const MAX: usize = 4096;
    if error.len() <= MAX {
        return error.to_string();
    }
    let mut end = MAX;
    while !error.is_char_boundary(end) {
        end -= 1;
    }
    error[..end].to_string()
}

fn nonnegative_i64(value: i64) -> u64 {
    value.max(0) as u64
}

fn retry_delay(message_id: &str, attempt: u32, base: Duration, cap: Duration) -> Duration {
    if base.is_zero() || cap.is_zero() {
        return Duration::ZERO;
    }
    let exponent = attempt.saturating_sub(1).min(30);
    let exponential = base.saturating_mul(1u32 << exponent).min(cap);
    // Stable 0–20% jitter avoids synchronized retry waves while keeping tests
    // and operator reasoning deterministic for a message ID.
    let hash = message_id
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| {
            hash.wrapping_mul(0x100_0000_01b3) ^ u64::from(byte)
        });
    let jitter_ceiling_ms = exponential.as_millis() / 5;
    let jitter_ms = if jitter_ceiling_ms == 0 {
        0
    } else {
        u128::from(hash) % (jitter_ceiling_ms + 1)
    };
    exponential
        .saturating_add(Duration::from_millis(
            jitter_ms.min(u128::from(u64::MAX)) as u64
        ))
        .min(cap)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn schema_and_claim_are_explicit_and_transactional() {
        assert!(MIGRATION_SQL.contains("deployment TEXT NOT NULL"));
        assert!(MIGRATION_SQL.contains("payload BYTEA NOT NULL"));
        assert!(MIGRATION_SQL.contains("perch_queue_dead_letters"));
        assert!(CLAIM_SQL.contains("FOR UPDATE SKIP LOCKED"));
        assert!(CLAIM_SQL.contains("attempts = message.attempts + 1"));
    }

    #[test]
    fn retry_backoff_is_bounded_and_deterministic() {
        let base = Duration::from_millis(100);
        let cap = Duration::from_secs(1);
        let first = retry_delay("message", 1, base, cap);
        assert!(first >= base && first <= Duration::from_millis(120));
        assert_eq!(first, retry_delay("message", 1, base, cap));
        assert!(retry_delay("message", 2, base, cap) >= Duration::from_millis(200));
        assert_eq!(retry_delay("message", 100, base, cap), cap);
    }

    #[test]
    fn diagnostics_are_utf8_safe_and_bounded() {
        let error = "é".repeat(3000);
        let bounded = bounded_error(&error);
        assert!(bounded.len() <= 4096);
        assert!(bounded.is_char_boundary(bounded.len()));
    }

    #[tokio::test]
    #[ignore = "requires PERCH_TEST_POSTGRES_URL"]
    async fn postgres_proves_leases_retries_dlq_and_binary_payloads() {
        let url = std::env::var("PERCH_TEST_POSTGRES_URL")
            .expect("PERCH_TEST_POSTGRES_URL must point to a disposable test database");
        let store = QueueStore::connect(&url, 8).await.unwrap();
        let pool = store.pool_status();
        assert_eq!(pool.max_size, 8);
        assert!(pool.size >= 1);
        assert!(pool.available <= pool.size);
        assert_eq!(pool.waiting, 0);
        let deployment = format!("queue-test-{}", nanoid!());
        let queue = "binary";

        let client = store.pool.get().await.unwrap();
        client
            .execute(
                "DELETE FROM perch_queue_messages WHERE deployment = $1",
                &[&deployment],
            )
            .await
            .unwrap();
        client
            .execute(
                "DELETE FROM perch_queue_dead_letters WHERE deployment = $1",
                &[&deployment],
            )
            .await
            .unwrap();
        drop(client);

        let id = store
            .enqueue(EnqueueMessage {
                deployment: deployment.clone(),
                queue_name: queue.into(),
                payload: vec![0, 1, 0xff, 0],
                delay: Duration::ZERO,
                max_attempts: 2,
            })
            .await
            .unwrap();
        let first = store
            .claim(
                &deployment,
                queue,
                "generation-a",
                Duration::from_millis(100),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.id, id);
        assert_eq!(first.deployment, deployment);
        assert_eq!(first.queue_name, queue);
        assert_eq!(first.payload, vec![0, 1, 0xff, 0]);
        assert_eq!(first.attempt, 1);
        assert!(!first.recovered_expired_lease);
        assert!(
            store
                .claim(
                    &deployment,
                    queue,
                    "generation-b",
                    Duration::from_millis(100)
                )
                .await
                .unwrap()
                .is_none(),
            "an active lease must exclude every other consumer"
        );

        assert!(store
            .release_without_attempt(&first, Duration::ZERO)
            .await
            .unwrap());
        let first_again = store
            .claim(
                &deployment,
                queue,
                "generation-b",
                Duration::from_millis(100),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            first_again.attempt, 1,
            "an undelivered claim is not an attempt"
        );
        assert!(matches!(
            store
                .retry(
                    &first_again,
                    "transient",
                    Duration::from_millis(10),
                    Duration::from_millis(10)
                )
                .await
                .unwrap(),
            RetryOutcome::Scheduled { .. }
        ));
        assert!(store
            .claim(
                &deployment,
                queue,
                "generation-a",
                Duration::from_millis(100)
            )
            .await
            .unwrap()
            .is_none());
        tokio::time::sleep(Duration::from_millis(25)).await;
        let exhausted = store
            .claim(
                &deployment,
                queue,
                "generation-a",
                Duration::from_millis(100),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(exhausted.attempt, 2);
        assert_eq!(
            store
                .retry(
                    &exhausted,
                    "permanent",
                    Duration::from_millis(10),
                    Duration::from_millis(10)
                )
                .await
                .unwrap(),
            RetryOutcome::DeadLettered
        );
        let stats = store.stats(&deployment, queue).await.unwrap();
        assert_eq!(stats.total_depth, 0);
        assert_eq!(stats.dead_letters, 1);
        let dead_letters = store
            .list_dead_letters(&deployment, queue, 10, None)
            .await
            .unwrap();
        assert_eq!(dead_letters.len(), 1);
        assert_eq!(dead_letters[0].id, id);
        assert_eq!(dead_letters[0].payload_bytes, 4);
        assert_eq!(dead_letters[0].attempts, 2);
        assert_eq!(dead_letters[0].max_attempts, 2);
        assert_eq!(dead_letters[0].final_error, "permanent");
        assert!(store
            .list_dead_letters(
                &deployment,
                queue,
                10,
                Some((dead_letters[0].failed_at_ms, &dead_letters[0].id)),
            )
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            store
                .replay_dead_letter(&deployment, queue, &id)
                .await
                .unwrap(),
            DeadLetterReplayOutcome::Replayed
        );
        assert_eq!(
            store
                .replay_dead_letter(&deployment, queue, &id)
                .await
                .unwrap(),
            DeadLetterReplayOutcome::NotFound
        );
        let replayed = store
            .claim(
                &deployment,
                queue,
                "operator-replay",
                Duration::from_millis(100),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(replayed.id, id);
        assert_eq!(replayed.attempt, 1);
        assert_eq!(replayed.payload, vec![0, 1, 0xff, 0]);
        assert!(store.ack(&replayed).await.unwrap());

        let purge_id = store
            .enqueue(EnqueueMessage {
                deployment: deployment.clone(),
                queue_name: queue.into(),
                payload: b"purge".to_vec(),
                delay: Duration::ZERO,
                max_attempts: 1,
            })
            .await
            .unwrap();
        let purge_claim = store
            .claim(
                &deployment,
                queue,
                "operator-purge",
                Duration::from_millis(100),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(purge_claim.id, purge_id);
        assert_eq!(
            store.dead_letter(&purge_claim, "purge me").await.unwrap(),
            RetryOutcome::DeadLettered
        );
        assert!(store
            .purge_dead_letter(&deployment, queue, &purge_id)
            .await
            .unwrap());
        assert!(!store
            .purge_dead_letter(&deployment, queue, &purge_id)
            .await
            .unwrap());

        let prune_id = store
            .enqueue(EnqueueMessage {
                deployment: deployment.clone(),
                queue_name: queue.into(),
                payload: b"prune".to_vec(),
                delay: Duration::ZERO,
                max_attempts: 1,
            })
            .await
            .unwrap();
        let prune_claim = store
            .claim(
                &deployment,
                queue,
                "retention-prune",
                Duration::from_millis(100),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(prune_claim.id, prune_id);
        assert_eq!(
            store.dead_letter(&prune_claim, "prune me").await.unwrap(),
            RetryOutcome::DeadLettered
        );

        store
            .enqueue(EnqueueMessage {
                deployment: deployment.clone(),
                queue_name: queue.into(),
                payload: b"lease".to_vec(),
                delay: Duration::ZERO,
                max_attempts: 3,
            })
            .await
            .unwrap();
        let leased = store
            .claim(
                &deployment,
                queue,
                "crashed-generation",
                Duration::from_millis(30),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(leased.attempt, 1);
        tokio::time::sleep(Duration::from_millis(60)).await;
        let recovered = store
            .claim(
                &deployment,
                queue,
                "recovery-generation",
                Duration::from_millis(100),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(recovered.attempt, 2);
        assert!(recovered.recovered_expired_lease);
        assert!(store.ack(&recovered).await.unwrap());

        for value in 0..12u8 {
            store
                .enqueue(EnqueueMessage {
                    deployment: deployment.clone(),
                    queue_name: queue.into(),
                    payload: vec![value],
                    delay: Duration::ZERO,
                    max_attempts: 2,
                })
                .await
                .unwrap();
        }
        let mut claims = Vec::new();
        for worker in 0..12 {
            let store = store.clone();
            let deployment = deployment.clone();
            claims.push(tokio::spawn(async move {
                store
                    .claim(
                        &deployment,
                        queue,
                        &format!("parallel-{worker}"),
                        Duration::from_secs(1),
                    )
                    .await
                    .unwrap()
                    .unwrap()
            }));
        }
        let mut claimed = Vec::new();
        for claim in claims {
            claimed.push(claim.await.unwrap());
        }
        assert_eq!(
            claimed
                .iter()
                .map(|message| &message.id)
                .collect::<HashSet<_>>()
                .len(),
            12,
            "SKIP LOCKED claims must never duplicate a row"
        );
        for message in claimed {
            assert!(store.ack(&message).await.unwrap());
        }

        assert!(store
            .claim(
                "some-other-deployment",
                queue,
                "isolation",
                Duration::from_millis(100)
            )
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            store
                .prune_dead_letters(&deployment, queue, Duration::ZERO)
                .await
                .unwrap(),
            1
        );
    }
}
