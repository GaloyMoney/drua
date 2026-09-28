use std::sync::Arc;

use obix::out::{EphemeralEventType, Outbox};
use obix::MailboxConfig;
use serde::{Deserialize, Serialize};
use sqlx::types::Json;
use tokio::sync::OnceCell;

use crate::git::GitEngine;

use super::SpaceError;

const LIBRARY_HEAD_EVENT_TYPE: EphemeralEventType =
    EphemeralEventType::new("drua_library_head");

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LibraryHeadFence {
    head: String,
}

/// Cross-replica read-your-writes barrier for space reads.
///
/// A successful write is not considered complete at the `Spaces` API boundary
/// until the pushed Git head is persisted as an obix ephemeral event. Reads
/// consult that durable fence before touching the local bare clone. If the
/// local clone does not contain the required head, the read synchronously
/// fetches origin before serving data.
///
/// The obix NOTIFY/LISTEN machinery remains an acceleration mechanism; the
/// persisted row is the correctness boundary. This matters because a wake-up
/// can arrive before another replica has actually advanced its local HEAD.
#[derive(Clone)]
pub(super) struct HeadFence {
    git: Arc<GitEngine>,
    pool: sqlx::PgPool,
    outbox: Arc<OnceCell<Outbox<LibraryHeadFence>>>,
}

impl HeadFence {
    pub(super) fn new(git: &Arc<GitEngine>, pool: &sqlx::PgPool) -> Self {
        return Self {
            git: Arc::clone(git),
            pool: pool.clone(),
            outbox: Arc::new(OnceCell::new()),
        };
    }

    async fn outbox(&self) -> Result<&Outbox<LibraryHeadFence>, SpaceError> {
        return self
            .outbox
            .get_or_try_init(|| async {
                let config = MailboxConfig::builder()
                    .build()
                    .map_err(|e| SpaceError::Git(format!("obix mailbox config: {e}")))?;
                let outbox = Outbox::<LibraryHeadFence>::init(&self.pool, config).await?;
                return Ok(outbox);
            })
            .await;
    }

    /// Publish the pushed remote head as the durable visibility fence.
    ///
    /// `fetch_and_head` deliberately takes the GitEngine repo mutex. If another
    /// local batch starts immediately after our write, this call waits for that
    /// batch and records the same or a newer *remote* head; it never snapshots
    /// an unpushed local commit.
    pub(super) async fn after_write(&self) -> Result<(), SpaceError> {
        let head = self
            .git
            .fetch_and_head()
            .await
            .map_err(|e| SpaceError::Git(e.to_string()))?
            .ok_or_else(|| SpaceError::Git("library head missing after successful write".into()))?;

        self.outbox()
            .await?
            .publish_ephemeral(
                LIBRARY_HEAD_EVENT_TYPE.clone(),
                LibraryHeadFence { head },
            )
            .await?;

        return Ok(());
    }

    /// Ensure the local clone is at least as new as the latest acknowledged
    /// write before a hot read is served.
    pub(super) async fn before_read(&self) -> Result<(), SpaceError> {
        let required_head = match self.required_head().await? {
            Some(head) => head,
            None => return Ok(()),
        };

        if self.local_contains(&required_head).await? {
            return Ok(());
        }

        // The fence is durable and was published only after a successful push.
        // If origin is temporarily unreachable, fail the read rather than serve
        // data known to be stale. A successful fetch makes origin authoritative;
        // this also handles a legitimate force-push that replaced the fenced
        // commit with a non-descendant head.
        self.git
            .fetch_and_head()
            .await
            .map_err(|e| SpaceError::Git(e.to_string()))?;

        return Ok(());
    }

    async fn required_head(&self) -> Result<Option<String>, SpaceError> {
        let payload = sqlx::query_scalar::<_, Json<LibraryHeadFence>>(
            "SELECT payload FROM ephemeral_outbox_events WHERE event_type = $1",
        )
        .bind(LIBRARY_HEAD_EVENT_TYPE.as_str())
        .fetch_optional(&self.pool)
        .await?;

        return Ok(payload.map(|Json(fence)| fence.head));
    }

    async fn local_contains(&self, required_head: &str) -> Result<bool, SpaceError> {
        let repo_path = self.git.repo_path().to_path_buf();
        let required_head = required_head.to_string();

        return tokio::task::spawn_blocking(move || -> Result<bool, SpaceError> {
            let repo = git2::Repository::open_bare(&repo_path)
                .map_err(|e| SpaceError::Git(format!("open bare: {e}")))?;
            let required_oid = git2::Oid::from_str(&required_head)
                .map_err(|e| SpaceError::Git(format!("parse required head {required_head}: {e}")))?;
            let local_oid = match repo.head().ok().and_then(|head| head.target()) {
                Some(oid) => oid,
                None => return Ok(false),
            };

            if local_oid == required_oid {
                return Ok(true);
            }

            if repo.find_commit(required_oid).is_err() {
                return Ok(false);
            }

            let contains = repo
                .graph_descendant_of(local_oid, required_oid)
                .map_err(|e| SpaceError::Git(format!("compare local head to fence: {e}")))?;
            return Ok(contains);
        })
        .await
        .map_err(|e| SpaceError::Git(format!("head fence join: {e}")))?;
    }
}
