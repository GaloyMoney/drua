use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use sqlx::PgPool;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::error::LibraryError;
use crate::git::GitEngine;

/// obix ephemeral event type carrying the oid last pushed to
/// `refs/heads/main` — the cross-replica read-your-write fence. Published
/// by the writer inside [`GitEngine::process_batch`](crate::git::GitEngine),
/// under the per-ref push lock, right after a successful push.
const MAIN_HEAD_EVENT: obix::out::EphemeralEventType =
    obix::out::EphemeralEventType::new("library_main_head");

/// Payload of [`MAIN_HEAD_EVENT`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct MainHead {
    oid: String,
}

/// Cross-replica read-your-write fence for `refs/heads/main`. The writer
/// publishes the oid it just pushed ([`Self::record_pushed`]); a read at
/// HEAD gates on it ([`Self::ensure_main_current`]); a draft handle that's
/// behind `changesets.head_oid` waits on a fetch through
/// [`Self::wait_for_ref`] (no obix event for drafts — Postgres' `head_oid`
/// is already the fence, see handoff §11.2). The only module in this
/// crate that names `obix`.
#[derive(Clone)]
pub struct HeadFence {
    outbox: obix::Outbox<MainHead>,
    pool: PgPool,
    /// Upper bound a read or a draft handle waits for this replica to
    /// catch up to an acked write before failing with `StaleReplica`.
    catch_up_timeout: Duration,
}

impl HeadFence {
    pub async fn init(pool: &PgPool, catch_up_timeout_ms: u64) -> Result<Self, LibraryError> {
        // Only the ephemeral `library_main_head` row is ever used — no
        // persistent events are published on this mailbox — so every
        // `MailboxConfig` field keeps its obix default.
        let outbox = obix::Outbox::<MainHead>::init(
            pool,
            obix::MailboxConfig::builder()
                .build()
                .map_err(|e| LibraryError::Config(format!("mailbox config: {e}")))?,
        )
        .await?;
        Ok(Self {
            outbox,
            pool: pool.clone(),
            catch_up_timeout: Duration::from_millis(catch_up_timeout_ms),
        })
    }

    /// Cluster-wide counterpart of the writer's local wake-up: any
    /// replica's successful push publishes [`MAIN_HEAD_EVENT`]; every
    /// replica's `listen_ephemeral()` stream observes it (obix backfills
    /// the current row on subscribe, which costs one spurious wake at
    /// boot) and wakes this replica's fetcher immediately via
    /// `commit_notify`. While obix's underlying LISTEN connection is
    /// down, sync degrades to ticker cadence — obix itself handles
    /// reconnection.
    pub fn spawn_peer_listener(&self, commit_notify: Arc<Notify>) -> JoinHandle<()> {
        let mut events = self.outbox.listen_ephemeral();
        tokio::spawn(async move {
            while let Some(ev) = events.next().await {
                if ev.event_type == MAIN_HEAD_EVENT {
                    commit_notify.notify_one();
                }
            }
        })
    }

    /// Reads the current fence row (at most one, since `event_type` is
    /// UNIQUE) through obix's own default `MailboxTables` impl. Reached
    /// purely by type inference from `self.outbox`'s own `Tables`
    /// parameter, never by naming `DefaultMailboxTables` — it is private
    /// in obix 0.9.0 (`mod tables;` has no `pub` in `obix::lib.rs`), so no
    /// path to it exists outside the obix crate. `Tbl` is a "voldemort
    /// type": inferred, never spelled.
    ///
    /// Would become `self.outbox.latest_ephemeral(MAIN_HEAD_EVENT)` once
    /// obix ships that accessor (handoff-read-your-write-fence-2026-10-01.md
    /// §9 item 2.1 / §11.3) — that requires a release from the sibling
    /// obix repo, out of scope here; this extraction confines the shim to
    /// this one function in the meantime.
    async fn published(&self) -> Result<Option<String>, sqlx::Error> {
        async fn load<Tbl>(
            outbox: &obix::Outbox<MainHead, Tbl>,
            pool: &PgPool,
        ) -> Result<Option<MainHead>, sqlx::Error>
        where
            Tbl: obix::MailboxTables,
        {
            let _ = outbox; // the anchor only pins `Tbl`; the read itself is a free function on it
            let mut rows =
                Tbl::load_ephemeral_events::<MainHead>(pool, Some(MAIN_HEAD_EVENT)).await?;
            debug_assert!(
                rows.len() <= 1,
                "event_type is UNIQUE; a load_ephemeral_events filtered by it returns at most one row"
            );
            Ok(rows.pop().map(|e| e.payload))
        }
        Ok(load(&self.outbox, &self.pool).await?.map(|h| h.oid))
    }

    /// Upserts the cross-replica fence row (`event_type` is UNIQUE, so
    /// this overwrites rather than accumulates) and wakes every
    /// subscribed peer listener, with one retry (OQ-4) before giving up.
    pub async fn record_pushed(&self, oid: &str) -> Result<(), sqlx::Error> {
        if let Err(e) = self.publish_once(oid).await {
            tracing::warn!(error = %e, "library fence publish failed; retrying once");
            self.publish_once(oid).await?;
        }
        Ok(())
    }

    async fn publish_once(&self, oid: &str) -> Result<(), sqlx::Error> {
        self.outbox
            .publish_ephemeral(
                MAIN_HEAD_EVENT,
                MainHead {
                    oid: oid.to_string(),
                },
            )
            .await
    }

    /// Gate for every read at `refs/heads/main` HEAD: checks the
    /// published fence against this replica's locally observed head,
    /// waking `git`'s fetcher and parking (one fetch serving every
    /// waiter) if this replica is behind, rather than fetching itself.
    pub async fn ensure_main_current(&self, git: &GitEngine) -> Result<(), LibraryError> {
        let Some(fence) = self.published().await? else {
            // Nothing ever published — no drua write has landed on main
            // yet (or the fence row was manually cleared).
            return Ok(());
        };
        let mut rx = git.local_main();
        if rx.borrow().as_deref() == Some(fence.as_str()) {
            return Ok(()); // hot path
        }
        if git.descends_from("refs/heads/main", &fence).await? {
            return Ok(()); // ahead of the fence (human push, or a later commit)
        }

        git.commit_notify().notify_one();
        let waited = tokio::time::timeout(self.catch_up_timeout, async {
            loop {
                if rx.borrow().as_deref() == Some(fence.as_str())
                    || git.descends_from("refs/heads/main", &fence).await?
                {
                    return Ok::<(), LibraryError>(());
                }
                rx.changed()
                    .await
                    .map_err(|_| LibraryError::Git("local_main watch closed".into()))?;
            }
        })
        .await;
        match waited {
            Ok(result) => result,
            Err(_) => Err(LibraryError::StaleReplica {
                refname: "refs/heads/main".to_string(),
                required: fence,
                local: rx.borrow().clone(),
            }),
        }
    }

    /// Parks until `refname` resolves to `required` or a descendant of
    /// it, waking `git`'s fetcher and polling only on `fetch_generation`
    /// changes (one fetch serving every waiter) rather than fetching
    /// itself. The draft counterpart of [`Self::ensure_main_current`] —
    /// unlike main, a draft ref has no obix fence; `required` is
    /// `changesets.head_oid`, already durable in Postgres, so the wait
    /// only needs to re-resolve the ref on each fetch.
    ///
    /// `Ok(None)` means `refname` resolved to nothing after a fetch — a
    /// genuinely deleted ref, not a replica that is merely behind; the
    /// caller should treat that the same as "missing" today, not as
    /// `StaleReplica`. `Err(StaleReplica)` after the configured timeout.
    pub async fn wait_for_ref(
        &self,
        git: &GitEngine,
        refname: &str,
        required: &str,
    ) -> Result<Option<String>, LibraryError> {
        git.commit_notify().notify_one();
        let mut rx = git.fetch_generation();
        let mut last_seen: Option<String> = None;
        let waited = tokio::time::timeout(self.catch_up_timeout, async {
            loop {
                // `resolve_current` resolves and classifies `refname` in
                // one blocking call; a separate resolve + descends_from
                // pair here would have the same TOCTOU a concurrent
                // fetch could exploit as the one this method exists to
                // close for its callers (see `resolve_current`'s doc).
                match git.resolve_current(refname, required).await? {
                    None => {
                        last_seen = None;
                        return Ok(None);
                    }
                    Some((tip, true)) => {
                        last_seen = Some(tip.clone());
                        return Ok(Some(tip));
                    }
                    Some((tip, false)) => last_seen = Some(tip),
                }
                rx.changed()
                    .await
                    .map_err(|_| LibraryError::Git("fetch_generation watch closed".into()))?;
            }
        })
        .await;
        match waited {
            Ok(result) => result,
            Err(_) => Err(LibraryError::StaleReplica {
                refname: refname.to_string(),
                required: required.to_string(),
                local: last_seen,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://user:password@localhost:5432/drua".to_string());
        PgPool::connect(&url).await.expect("connect to pg")
    }

    #[tokio::test]
    #[ignore = "requires postgres; run with --ignored"]
    async fn publish_once_upserts_the_fence_row() {
        let pool = test_pool().await;
        sqlx::query("DELETE FROM ephemeral_outbox_events WHERE event_type = 'library_main_head'")
            .execute(&pool)
            .await
            .expect("clear fence row");
        let fence = HeadFence::init(&pool, 5_000).await.expect("fence init");

        fence
            .publish_once("deadbeef00000000000000000000000000000000")
            .await
            .expect("publish");
        let payload: serde_json::Value = sqlx::query_scalar(
            "SELECT payload FROM ephemeral_outbox_events WHERE event_type = 'library_main_head'",
        )
        .fetch_one(&pool)
        .await
        .expect("fence row exists");
        assert_eq!(payload["oid"], "deadbeef00000000000000000000000000000000");

        // `event_type` is UNIQUE and the insert is `ON CONFLICT DO UPDATE`
        // (obix-macros/src/tables.rs:166 at obix 0.9.0) — a second publish
        // overwrites the row rather than accumulating a second one.
        fence
            .publish_once("cafef00d00000000000000000000000000000000")
            .await
            .expect("publish again");
        let rows: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM ephemeral_outbox_events WHERE event_type = 'library_main_head'",
        )
        .fetch_one(&pool)
        .await
        .expect("count rows");
        assert_eq!(rows, 1, "the fence row is an upsert, not an accumulation");
        let payload: serde_json::Value = sqlx::query_scalar(
            "SELECT payload FROM ephemeral_outbox_events WHERE event_type = 'library_main_head'",
        )
        .fetch_one(&pool)
        .await
        .expect("fence row exists");
        assert_eq!(payload["oid"], "cafef00d00000000000000000000000000000000");
    }
}
