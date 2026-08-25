use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use obix::out::EphemeralEventType;
use obix::{MailboxConfig, MailboxTables, Outbox};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use tokio::sync::{watch, Notify};
use tokio::task::JoinHandle;

use crate::LibraryError;

/// Ephemeral outbox row carrying the library repo's published head version.
/// One upserted row — only its current value is ever needed
const HEAD_EVENT: EphemeralEventType = EphemeralEventType::new("library_head");

/// Ceiling on how long a read waits for this replica to catch up. Past it the
/// read fails rather than silently serving content the caller was already told
/// had been overwritten.
const CATCH_UP_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(MailboxTables)]
struct OutboxTables;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HeadAdvanced {
    version: u64,
}

/// Cross-replica freshness signal for the library clone.
///
/// The pushing replica publishes a new version once its commits are on
/// upstream `main` and before it acks the write, so any version a reader
/// observes is backed by content that is already fetchable. A reader blocks
/// until this replica has applied the version published when the read began —
/// which is what lets a read served by one replica observe a write acked on
/// another.
pub(crate) struct HeadWatermark {
    outbox: Outbox<HeadAdvanced, OutboxTables>,
    pool: PgPool,
    applied: watch::Sender<u64>,
}

impl HeadWatermark {
    pub(crate) async fn init(pool: &PgPool) -> Result<Self, LibraryError> {
        let config = MailboxConfig::builder()
            .build()
            .expect("mailbox config defaults");
        Ok(Self {
            outbox: Outbox::init(pool, config).await?,
            pool: pool.clone(),
            applied: watch::channel(0).0,
        })
    }

    /// Highest version any replica has published. Read from Postgres every
    /// time: a cached value cannot reflect a write acked moments ago on another
    /// replica
    pub(crate) async fn published(&self) -> Result<u64, LibraryError> {
        let events =
            OutboxTables::load_ephemeral_events::<HeadAdvanced>(&self.pool, Some(HEAD_EVENT))
                .await?;
        // `event_type` is UNIQUE, so this is at most one row. No row means no
        // replica has ever published — version 0, which every reader trivially
        // satisfies. A Postgres failure surfaces as `Err` above, never as empty.
        Ok(events.first().map_or(0, |event| event.payload.version))
    }

    /// Publishes the next version and records that this replica already holds
    /// it — the caller committed locally before pushing.
    ///
    /// Callers must hold the cluster-wide push advisory lock: it is what keeps
    /// this read-modify-write monotonic across replicas.
    pub(crate) async fn publish(&self) -> Result<(), LibraryError> {
        let version = self.published().await? + 1;
        self.outbox
            .publish_ephemeral(HEAD_EVENT, HeadAdvanced { version })
            .await?;
        self.mark_applied(version);
        Ok(())
    }

    pub(crate) fn applied(&self) -> u64 {
        *self.applied.borrow()
    }

    /// Records that this replica's clone now carries everything published up to
    /// `version`. Only sound when `version` was read *before* the fetch being
    /// recorded, since only then is the fetch guaranteed to have carried it.
    pub(crate) fn mark_applied(&self, version: u64) {
        self.applied.send_if_modified(|applied| {
            let advanced = version > *applied;
            if advanced {
                *applied = version;
            }
            advanced
        });
    }

    pub(crate) async fn wait_until_applied(&self, version: u64) -> Result<(), LibraryError> {
        let mut applied = self.applied.subscribe();
        tokio::time::timeout(
            CATCH_UP_TIMEOUT,
            applied.wait_for(|applied| *applied >= version),
        )
        .await
        .map_err(|_| LibraryError::CatchUpTimeout {
            required: version,
            applied: self.applied(),
        })?
        .expect("watermark sender outlives its receivers");
        Ok(())
    }

    /// Wakes `fetcher` whenever any replica publishes a new version.
    pub(crate) fn spawn_peer_listener(&self, commit_notify: Arc<Notify>) -> JoinHandle<()> {
        let mut events = self.outbox.listen_ephemeral();
        tokio::spawn(async move {
            while let Some(event) = events.next().await {
                if event.event_type == HEAD_EVENT {
                    commit_notify.notify_one();
                }
            }
        })
    }
}
