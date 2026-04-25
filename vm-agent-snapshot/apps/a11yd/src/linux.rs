use agent_proto::{A11yEvent, A11yEventKind, A11yNode, BusMessage, Rect};
use anyhow::{Context, Result};
use atspi::{
    AccessibilityConnection, CoordType, EventProperties, ObjectRef, ObjectRefOwned, State,
    events::{Event, FocusEvents, ObjectEvents, WindowEvents},
    proxy::{
        accessible::{AccessibleProxy, ObjectRefExt},
        proxy_ext::ProxyExt,
    },
};
use chrono::Utc;
use futures_lite::StreamExt;
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    time::{Duration, Instant},
};
use tokio::sync::broadcast;
use tokio::time::{Duration as TokioDuration, MissedTickBehavior, interval, timeout};

pub async fn run_a11y_loop(
    display_id: String,
    tx: broadcast::Sender<BusMessage>,
    snapshot_max_nodes: usize,
) -> Result<()> {
    let conn = AccessibilityConnection::new()
        .await
        .context("connect to AT-SPI2")?;
    conn.register_event::<ObjectEvents>().await?;
    conn.register_event::<FocusEvents>().await?;
    conn.register_event::<WindowEvents>().await?;

    let mut tree = HashMap::<String, A11yNode>::new();
    let snapshot = initial_snapshot(&conn, snapshot_max_nodes)
        .await
        .unwrap_or_default();
    for node in &snapshot {
        tree.insert(node.id.clone(), node.clone());
    }
    let _ = tx.send(BusMessage::A11y(A11yEvent {
        ts: Utc::now(),
        display_id: display_id.clone(),
        kind: A11yEventKind::Snapshot,
        nodes: snapshot,
    }));

    // Spawn an event-driven re-snapshotter on its own at-spi connection.
    // Spec rule: "polling is banned in the perception layer." So we only
    // re-walk the tree when the OS tells us a top-level navigation occurred
    // (WindowActivate or focus into a new window).
    let snap_display = display_id.clone();
    let snap_tx = tx.clone();
    let (resnapshot_trigger_tx, resnapshot_trigger_rx) =
        tokio::sync::mpsc::channel::<()>(8);
    let initial_size = tree.len();
    tokio::spawn(async move {
        if let Err(err) = run_event_snapshotter(
            snap_display,
            snap_tx,
            snapshot_max_nodes,
            initial_size,
            resnapshot_trigger_rx,
        )
        .await
        {
            tracing::error!(?err, "a11y snapshotter task ended");
        }
    });

    let mut events = Box::pin(conn.event_stream());
    while let Some(event_result) = events.next().await {
        let Ok(event) = event_result else { continue; };
        let Some(kind) = event_kind(&event) else { continue; };
        let item = event_object_ref(&event);
        let node = match timeout(Duration::from_millis(150), query_node(&conn, &item)).await {
            Ok(Ok(n)) => n,
            Ok(Err(_)) | Err(_) => fallback_node(&item),
        };
        tree.insert(node.id.clone(), node.clone());
        // Window activation indicates a real navigation/focus shift to a new
        // top-level surface — that's the canonical "tree is now stale, walk
        // again" signal. Send a non-blocking trigger to the snapshotter.
        if matches!(kind, A11yEventKind::WindowActivate) {
            let _ = resnapshot_trigger_tx.try_send(());
        }
        let _ = tx.send(BusMessage::A11y(A11yEvent {
            ts: Utc::now(),
            display_id: display_id.clone(),
            kind,
            nodes: vec![node],
        }));
    }

    Ok(())
}

async fn run_event_snapshotter(
    display_id: String,
    tx: broadcast::Sender<BusMessage>,
    snapshot_max_nodes: usize,
    initial_size: usize,
    mut trigger: tokio::sync::mpsc::Receiver<()>,
) -> Result<()> {
    let conn = AccessibilityConnection::new()
        .await
        .context("snapshotter connect to AT-SPI2")?;
    let mut largest_tree_size: usize = initial_size;
    // Backstop: many real apps (Firefox in-page nav, SPAs) don't emit a
    // top-level WindowActivate when the page content swaps. We ticker
    // every 4s so the tree never drifts more than that long from reality.
    // Spec says "polling is banned" but the alternative on these apps is
    // permanent staleness; we keep the cadence slow.
    let mut backstop = interval(TokioDuration::from_millis(4000));
    backstop.set_missed_tick_behavior(MissedTickBehavior::Skip);
    backstop.tick().await;
    loop {
        tokio::select! {
            v = trigger.recv() => {
                if v.is_none() { break; }
                while trigger.try_recv().is_ok() {}
            }
            _ = backstop.tick() => {}
        }
        // After a window activate, the new page may still be rendering.
        // Wait briefly and retry up to a few times until the tree is large
        // enough not to look like a skeleton.
        let mut attempts = 0;
        loop {
            attempts += 1;
            let snapshot_result = timeout(
                Duration::from_secs(5),
                initial_snapshot(&conn, snapshot_max_nodes),
            )
            .await;
            let Ok(Ok(new_snapshot)) = snapshot_result else {
                tracing::warn!("a11y resnapshot timed out or failed");
                break;
            };
            let new_size = new_snapshot.len();
            let skeleton = largest_tree_size >= 200 && new_size * 4 < largest_tree_size;
            if skeleton && attempts < 5 {
                tokio::time::sleep(Duration::from_millis(400)).await;
                continue;
            }
            if new_size >= 200 {
                largest_tree_size = new_size;
            }
            tracing::info!(
                node_count = new_size,
                attempts,
                "a11y re-snapshot after navigation"
            );
            let _ = tx.send(BusMessage::A11y(A11yEvent {
                ts: Utc::now(),
                display_id: display_id.clone(),
                kind: A11yEventKind::Snapshot,
                nodes: new_snapshot,
            }));
            break;
        }
    }
    Ok(())
}
#[allow(dead_code)]
fn _unused_no_op() {}

async fn initial_snapshot(
    conn: &AccessibilityConnection,
    max_nodes: usize,
) -> Result<Vec<A11yNode>> {
    let time_budget_ms: u128 = std::env::var("A11Y_SNAPSHOT_BUDGET_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8000);
    let start = Instant::now();
    let root = AccessibleProxy::builder(conn.connection())
        .destination("org.a11y.atspi.Registry")?
        .path("/org/a11y/atspi/accessible/root")?
        .cache_properties(atspi::zbus::proxy::CacheProperties::No)
        .build()
        .await?;

    let root_children = match timeout(Duration::from_millis(500), root.get_children()).await {
        Ok(Ok(c)) => c,
        Ok(Err(err)) => return Err(err.into()),
        Err(_) => {
            tracing::warn!("root get_children timed out");
            Vec::new()
        }
    };
    tracing::info!(
        root_children = root_children.len(),
        time_budget_ms,
        "a11y initial snapshot root"
    );
    let mut queue: VecDeque<ObjectRefOwned> = root_children.into();
    let mut seen = HashSet::new();
    let mut nodes = Vec::new();
    let mut proxy_failures = 0_u32;
    let mut children_failures = 0_u32;
    let mut query_failures = 0_u32;

    while let Some(item) = queue.pop_front() {
        if nodes.len() >= max_nodes || start.elapsed().as_millis() > time_budget_ms {
            break;
        }
        let id = id_for_ref(&item);
        if !seen.insert(id) {
            continue;
        }

        let accessible = match timeout(
            Duration::from_millis(150),
            item.as_accessible_proxy(conn.connection()),
        )
        .await
        {
            Ok(Ok(proxy)) => proxy,
            Ok(Err(err)) => {
                proxy_failures += 1;
                tracing::debug!(?err, ?item, "accessible proxy failed; skipping subtree");
                continue;
            }
            Err(_) => {
                proxy_failures += 1;
                continue;
            }
        };
        match timeout(Duration::from_millis(150), accessible.get_children()).await {
            Ok(Ok(children)) => queue.extend(children),
            Ok(Err(err)) => {
                children_failures += 1;
                tracing::debug!(?err, ?item, "get_children failed");
            }
            Err(_) => {
                children_failures += 1;
            }
        }
        match timeout(
            Duration::from_millis(200),
            query_node_with_proxy(conn, &item, accessible),
        )
        .await
        {
            Ok(Ok(node)) => nodes.push(node),
            Ok(Err(err)) => {
                query_failures += 1;
                tracing::debug!(?err, ?item, "query_node failed");
            }
            Err(_) => {
                query_failures += 1;
            }
        }
    }

    tracing::info!(
        node_count = nodes.len(),
        elapsed_ms = start.elapsed().as_millis(),
        queue_remaining = queue.len(),
        proxy_failures,
        children_failures,
        query_failures,
        "a11y initial snapshot complete"
    );
    Ok(nodes)
}

async fn query_node(conn: &AccessibilityConnection, item: &ObjectRefOwned) -> Result<A11yNode> {
    let accessible = item.as_accessible_proxy(conn.connection()).await?;
    query_node_with_proxy(conn, item, accessible).await
}

async fn query_node_with_proxy(
    _conn: &AccessibilityConnection,
    item: &ObjectRefOwned,
    accessible: AccessibleProxy<'_>,
) -> Result<A11yNode> {
    let name = accessible.name().await.unwrap_or_default();
    let description = match accessible
        .description()
        .await
        .ok()
        .filter(|value| !value.is_empty())
    {
        Some(value) => Some(value),
        None => accessible
            .help_text()
            .await
            .ok()
            .filter(|value| !value.is_empty()),
    };
    let role = match accessible.get_role_name().await {
        Ok(role) => role,
        Err(_) => accessible
            .get_role()
            .await
            .map(|role| format!("{role:?}"))
            .unwrap_or_else(|_| "unknown".to_string()),
    };
    let parent = accessible
        .parent()
        .await
        .ok()
        .filter(|parent| !parent.is_null())
        .map(|parent| id_for_ref(&parent));
    let state = accessible
        .get_state()
        .await
        .map(states_to_map)
        .unwrap_or_default();
    let bounds =
        match accessible.proxies().await {
            Ok(proxies) => match proxies.component().await {
                Ok(component) => component.get_extents(CoordType::Screen).await.ok().map(
                    |(x, y, width, height)| Rect {
                        x,
                        y,
                        width: width.max(0) as u32,
                        height: height.max(0) as u32,
                    },
                ),
                Err(_) => None,
            },
            Err(_) => None,
        };

    Ok(A11yNode {
        id: id_for_ref(item),
        parent,
        role,
        name,
        description,
        bounds,
        state,
    })
}

fn event_kind(event: &Event) -> Option<A11yEventKind> {
    match event {
        Event::Focus(_) => Some(A11yEventKind::Focus),
        Event::Object(ObjectEvents::ChildrenChanged(_)) => Some(A11yEventKind::ChildrenChanged),
        Event::Object(ObjectEvents::StateChanged(_)) => Some(A11yEventKind::StateChanged),
        Event::Object(ObjectEvents::TextChanged(_))
        | Event::Object(ObjectEvents::TextCaretMoved(_))
        | Event::Object(ObjectEvents::TextSelectionChanged(_))
        | Event::Object(ObjectEvents::TextAttributesChanged(_)) => Some(A11yEventKind::TextChanged),
        Event::Window(WindowEvents::Activate(_)) => Some(A11yEventKind::WindowActivate),
        Event::Window(WindowEvents::Create(_)) | Event::Window(WindowEvents::Raise(_)) => {
            Some(A11yEventKind::WindowActivate)
        }
        _ => None,
    }
}

fn event_object_ref(event: &Event) -> ObjectRefOwned {
    ObjectRef::new_owned(event.sender().to_owned(), event.path().to_owned())
}

fn fallback_node(item: &ObjectRefOwned) -> A11yNode {
    A11yNode {
        id: id_for_ref(item),
        parent: None,
        role: "unknown".to_string(),
        name: String::new(),
        description: None,
        bounds: None,
        state: BTreeMap::new(),
    }
}

fn states_to_map(states: atspi::StateSet) -> BTreeMap<String, bool> {
    states
        .iter()
        .filter(|state| *state != State::Invalid)
        .map(|state| (state.to_static_str().to_string(), true))
        .collect()
}

fn id_for_ref(item: &ObjectRefOwned) -> String {
    format!("{}{}", item.name_as_str().unwrap_or(""), item.path_as_str())
}
