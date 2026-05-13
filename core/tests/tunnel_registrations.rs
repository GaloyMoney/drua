use std::time::Duration;

use drua_core::tunnel::{RegisteredToolSet, TunnelRegistrations};

const PG_CON: &str = "postgres://user:password@localhost:5432/drua";

async fn pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| PG_CON.to_string());
    sqlx::PgPool::connect(&url).await.expect("connect to pg")
}

async fn fresh_deployment(pool: &sqlx::PgPool) -> String {
    let id = format!("test-tunnel-{}", uuid::Uuid::new_v4());
    sqlx::query("DELETE FROM tunnel_registrations WHERE deployment_id = $1")
        .bind(&id)
        .execute(pool)
        .await
        .expect("clean prior row");
    id
}

fn unique_owner(tag: &str) -> String {
    format!("10.0.0.0:{}-{}", tag, uuid::Uuid::new_v4())
}

fn registration() -> RegisteredToolSet {
    RegisteredToolSet {
        name: "kubernetes".to_string(),
        prefix: "k8s".to_string(),
        category: "infrastructure".to_string(),
        category_description: "kube".to_string(),
        tools: vec![],
    }
}

async fn wait_for_registry_notify(
    listener: &mut sqlx::postgres::PgListener,
    deployment_id: &str,
) -> bool {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let notification = listener.recv().await.unwrap();
            if notification.payload() == deployment_id {
                break true;
            }
        }
    })
    .await
    .unwrap_or(false)
}

#[tokio::test]
async fn registry_lifecycle_heartbeats_takeover_and_owner_cleanup() {
    let pool = pool().await;
    let deployment_id = fresh_deployment(&pool).await;
    let other_deployment_id = fresh_deployment(&pool).await;
    let expired_deployment_id = fresh_deployment(&pool).await;
    let regs = TunnelRegistrations::new(pool.clone());
    let owner_a = unique_owner("life-a");
    let owner_b = unique_owner("life-b");

    let stale_session = uuid::Uuid::new_v4();
    regs.upsert(
        &deployment_id,
        stale_session,
        &owner_a,
        &[registration()],
        Duration::from_secs(2),
    )
    .await
    .unwrap();

    let row1 = regs
        .fetch_active(&deployment_id)
        .await
        .unwrap()
        .expect("row exists");
    assert_eq!(row1.session_id, stale_session);
    assert_eq!(row1.owner_pod_addr, owner_a);

    let extended = regs
        .heartbeat(&deployment_id, stale_session, Duration::from_secs(120))
        .await
        .unwrap();
    assert!(extended);
    let row2 = regs
        .fetch_active(&deployment_id)
        .await
        .unwrap()
        .expect("still active");
    assert!(
        row2.expires_at > row1.expires_at,
        "heartbeat must move expires_at forward"
    );

    let new_session = uuid::Uuid::new_v4();
    regs.upsert(
        &deployment_id,
        new_session,
        &owner_b,
        &[registration()],
        Duration::from_secs(60),
    )
    .await
    .unwrap();

    let still_owner = regs
        .heartbeat(&deployment_id, stale_session, Duration::from_secs(60))
        .await
        .unwrap();
    assert!(!still_owner, "stale session must lose heartbeat race");

    let deleted = regs
        .delete_if_owner(&deployment_id, stale_session)
        .await
        .unwrap();
    assert_eq!(deleted, 0);

    let row = regs.fetch_active(&deployment_id).await.unwrap().unwrap();
    assert_eq!(row.session_id, new_session);
    assert_eq!(row.owner_pod_addr, owner_b);

    regs.upsert(
        &other_deployment_id,
        uuid::Uuid::new_v4(),
        &owner_a,
        &[registration()],
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    let cleared = regs.delete_all_owned_by(&owner_a).await.unwrap();
    assert_eq!(cleared, 1);
    assert!(regs
        .fetch_active(&other_deployment_id)
        .await
        .unwrap()
        .is_none());
    assert!(regs.fetch_active(&deployment_id).await.unwrap().is_some());

    let expired_session = uuid::Uuid::new_v4();
    regs.upsert(
        &expired_deployment_id,
        expired_session,
        &unique_owner("reap"),
        &[registration()],
        Duration::from_secs(0),
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let view = regs.fetch_active(&expired_deployment_id).await.unwrap();
    assert!(view.is_none(), "expires_at <= now() makes row inactive");
    let revived = regs
        .heartbeat(
            &expired_deployment_id,
            expired_session,
            Duration::from_secs(60),
        )
        .await
        .unwrap();
    assert!(
        !revived,
        "heartbeat must not revive ownership after the row expires"
    );
    regs.reap_expired().await.unwrap();
    let still_in_table: Option<(uuid::Uuid,)> =
        sqlx::query_as("SELECT session_id FROM tunnel_registrations WHERE deployment_id = $1")
            .bind(&expired_deployment_id)
            .fetch_optional(&pool)
            .await
            .unwrap();
    assert!(still_in_table.is_none(), "reaper deleted our expired row");

    regs.delete_if_owner(&deployment_id, new_session)
        .await
        .unwrap();
}

fn try_test_client() -> Option<reqwest::Client> {
    reqwest::Client::builder().build().ok()
}

#[tokio::test]
async fn reconcile_one_installs_drops_and_defers_local_takeover() {
    use drua_core::tunnel::{
        reconcile_all_deployments, reconcile_one_deployment, CoreReconcileTarget, InternalAuth,
        ReconcileCtx, TunnelRegistry,
    };

    let pool = pool().await;
    let deployment_id = fresh_deployment(&pool).await;
    let sweep_deployment_id = fresh_deployment(&pool).await;
    let regs = TunnelRegistrations::new(pool.clone());
    let toolsets = std::sync::Arc::new(drua_core::toolset::ToolSets::empty_for_test());
    let Some(http) = try_test_client() else {
        eprintln!("skipping: no system CAs available");
        return;
    };
    let http = std::sync::Arc::new(http);
    let tunnels = std::sync::Arc::new(drua_core::tunnel::TunnelRegistry::new());
    let auth = std::sync::Arc::new(InternalAuth::SharedSecret {
        secret: "x".to_string(),
    });
    let peer_target = CoreReconcileTarget::new(
        std::sync::Arc::clone(&toolsets),
        std::sync::Arc::clone(&tunnels),
        std::sync::Arc::clone(&http),
        std::sync::Arc::clone(&auth),
    );
    let owner = unique_owner("reconcile-owner");
    let peer = unique_owner("reconcile-peer");
    let peer_ctx = ReconcileCtx {
        regs: &regs,
        self_pod_addr: Some(&peer),
        target: &peer_target,
    };

    reconcile_one_deployment(&peer_ctx, &deployment_id)
        .await
        .unwrap();
    assert!(toolsets.toolset_names_for_test().is_empty());

    let session = uuid::Uuid::new_v4();
    regs.upsert(
        &deployment_id,
        session,
        &owner,
        &[registration()],
        Duration::from_secs(60),
    )
    .await
    .unwrap();

    reconcile_one_deployment(&peer_ctx, &deployment_id)
        .await
        .unwrap();
    let names = toolsets.toolset_names_for_test();
    assert_eq!(names.len(), 1);
    assert!(
        names[0].starts_with(&deployment_id.replace('-', "_")),
        "proxy name encodes deployment_id: {}",
        names[0]
    );

    let toolsets_a = std::sync::Arc::new(drua_core::toolset::ToolSets::empty_for_test());
    let tunnels_a = std::sync::Arc::new(TunnelRegistry::new());
    let owner_target = CoreReconcileTarget::new(
        std::sync::Arc::clone(&toolsets_a),
        std::sync::Arc::clone(&tunnels_a),
        std::sync::Arc::clone(&http),
        std::sync::Arc::clone(&auth),
    );
    let owner_ctx = ReconcileCtx {
        regs: &regs,
        self_pod_addr: Some(&owner),
        target: &owner_target,
    };
    reconcile_one_deployment(&owner_ctx, &deployment_id)
        .await
        .unwrap();
    assert!(
        toolsets_a.toolset_names_for_test().is_empty(),
        "owner pod's listener must not install Proxy for its own row"
    );

    regs.delete_if_owner(&deployment_id, session).await.unwrap();
    reconcile_one_deployment(&peer_ctx, &deployment_id)
        .await
        .unwrap();
    assert!(toolsets.proxy_deployment_ids_for_test().is_empty());

    let local_session = uuid::Uuid::new_v4();
    let (close_tx, mut close_rx) = tokio::sync::mpsc::channel::<()>(1);
    let (ws_tx, _ws_rx) = tokio::sync::mpsc::channel::<String>(8);
    let local_handle = drua_core::tunnel::TunnelHandle::new(ws_tx);
    tunnels
        .claim(&deployment_id, local_session, close_tx, local_handle)
        .await;
    let ctx = ReconcileCtx {
        regs: &regs,
        self_pod_addr: Some(&owner),
        target: &peer_target,
    };

    let peer_session = uuid::Uuid::new_v4();
    regs.upsert(
        &deployment_id,
        peer_session,
        &peer,
        &[registration()],
        Duration::from_secs(60),
    )
    .await
    .unwrap();

    reconcile_one_deployment(&ctx, &deployment_id)
        .await
        .unwrap();
    assert!(
        close_rx.try_recv().is_ok(),
        "close_tx must have been signaled"
    );
    assert!(
        toolsets.toolset_names_for_test().is_empty(),
        "Proxy install is deferred until cleanup runs"
    );

    tunnels.release(&deployment_id, local_session);
    reconcile_one_deployment(&ctx, &deployment_id)
        .await
        .unwrap();
    let names = toolsets.toolset_names_for_test();
    assert_eq!(names.len(), 1);
    assert!(names[0].contains("kubernetes"));

    let sweep_local_session = uuid::Uuid::new_v4();
    let (close_tx, mut close_rx) = tokio::sync::mpsc::channel::<()>(1);
    let (ws_tx, _ws_rx) = tokio::sync::mpsc::channel::<String>(8);
    let local_handle = drua_core::tunnel::TunnelHandle::new(ws_tx);
    tunnels
        .claim(
            &sweep_deployment_id,
            sweep_local_session,
            close_tx,
            local_handle,
        )
        .await;
    let sweep_peer_session = uuid::Uuid::new_v4();
    regs.upsert(
        &sweep_deployment_id,
        sweep_peer_session,
        &peer,
        &[registration()],
        Duration::from_secs(60),
    )
    .await
    .unwrap();

    reconcile_all_deployments(&ctx).await.unwrap();
    assert!(
        close_rx.try_recv().is_ok(),
        "displaced owned tunnel must be signaled to close from the full-sweep path"
    );

    regs.delete_if_owner(&deployment_id, peer_session)
        .await
        .unwrap();
    regs.delete_if_owner(&sweep_deployment_id, sweep_peer_session)
        .await
        .unwrap();
}

#[tokio::test]
async fn pg_notify_fires_on_upsert_and_owner_delete() {
    let pool = pool().await;
    let deployment_id = fresh_deployment(&pool).await;
    let regs = TunnelRegistrations::new(pool.clone());

    let mut listener = sqlx::postgres::PgListener::connect_with(&pool)
        .await
        .unwrap();
    listener.listen("tunnel_registry_changed").await.unwrap();

    let session = uuid::Uuid::new_v4();
    regs.upsert(
        &deployment_id,
        session,
        &unique_owner("notify"),
        &[registration()],
        Duration::from_secs(60),
    )
    .await
    .unwrap();

    assert!(
        wait_for_registry_notify(&mut listener, &deployment_id).await,
        "expected pg_notify with our deployment_id"
    );

    let deleted = regs.delete_if_owner(&deployment_id, session).await.unwrap();
    assert_eq!(deleted, 1);

    assert!(
        wait_for_registry_notify(&mut listener, &deployment_id).await,
        "expected pg_notify for deleted deployment_id"
    );
}
