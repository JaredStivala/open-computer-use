use agent_common::{AgentConfig, bind_socket, connect_socket, init_tracing, send_msg};
#[cfg(not(target_os = "linux"))]
use agent_proto::A11yNode;
use agent_proto::{A11yEvent, A11yEventKind, BusMessage};
use anyhow::Result;
#[cfg(not(target_os = "linux"))]
use chrono::Utc;
use clap::Parser;
use std::sync::Arc;
#[cfg(not(target_os = "linux"))]
use std::{collections::BTreeMap, time::Duration};
use tokio::sync::{RwLock, broadcast};
#[cfg(not(target_os = "linux"))]
use tokio::time::sleep;
use tracing::info;

#[cfg(target_os = "linux")]
mod linux;

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "config/agent.example.toml")]
    config: String,
    #[arg(long, default_value = ":0")]
    display_id: String,
    #[arg(long, default_value_t = 6000)]
    snapshot_max_nodes: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let args = Args::parse();
    let cfg = AgentConfig::load(&args.config)?;
    let listener = bind_socket(&cfg.sockets.a11y).await?;
    let (tx, _) = broadcast::channel::<BusMessage>(512);
    // Maintain an accumulated tree so late-connecting clients receive a
    // full Snapshot rather than the most recent single-node delta.
    let tree = Arc::new(RwLock::new(std::collections::HashMap::<
        String,
        agent_proto::A11yNode,
    >::new()));
    let display_id_for_writer = args.display_id.clone();
    let tree_writer = tree.clone();
    let mut latest_rx = tx.subscribe();
    tokio::spawn(async move {
        while let Ok(msg) = latest_rx.recv().await {
            let BusMessage::A11y(event) = msg else {
                continue;
            };
            let mut t = tree_writer.write().await;
            if matches!(event.kind, A11yEventKind::Snapshot) {
                t.clear();
            }
            for node in event.nodes {
                t.insert(node.id.clone(), node);
            }
            tracing::debug!(
                size = t.len(),
                kind = ?event.kind,
                display_id = %display_id_for_writer,
                "a11y tree updated"
            );
        }
    });
    let display_id = args.display_id.clone();
    let bus_path = cfg.sockets.bus.clone();
    let producer_tx = tx.clone();
    #[cfg(target_os = "linux")]
    let snapshot_max_nodes = args.snapshot_max_nodes;
    tokio::spawn(async move {
        #[cfg(target_os = "linux")]
        let run = run_linux(display_id, producer_tx, bus_path, snapshot_max_nodes);
        #[cfg(not(target_os = "linux"))]
        let run = run_stub(display_id, producer_tx, bus_path);
        if let Err(err) = run.await {
            tracing::error!(?err, "a11y producer failed");
        }
    });
    info!("a11y daemon listening on {}", cfg.sockets.a11y);

    let display_id_for_clients = args.display_id.clone();
    loop {
        let (mut stream, _) = listener.accept().await?;
        let snapshot_event = {
            let t = tree.read().await;
            BusMessage::A11y(A11yEvent {
                ts: chrono::Utc::now(),
                display_id: display_id_for_clients.clone(),
                kind: A11yEventKind::Snapshot,
                nodes: t.values().cloned().collect(),
            })
        };
        if let Err(err) = send_msg(&mut stream, &snapshot_event).await {
            tracing::debug!(?err, "a11y client closed before snapshot");
            continue;
        }
        let mut rx = tx.subscribe();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(msg) => {
                        if let Err(err) = send_msg(&mut stream, &msg).await {
                            tracing::debug!(?err, "a11y client closed");
                            return;
                        }
                    }
                    Err(err) => {
                        tracing::debug!(?err, "a11y broadcast closed");
                        return;
                    }
                }
            }
        });
    }
}

#[cfg(target_os = "linux")]
async fn run_linux(
    display_id: String,
    tx: broadcast::Sender<BusMessage>,
    bus_path: String,
    snapshot_max_nodes: usize,
) -> Result<()> {
    let mut bus_rx = tx.subscribe();
    tokio::spawn(async move {
        forward_to_bus(bus_path, &mut bus_rx).await;
    });
    linux::run_a11y_loop(display_id, tx, snapshot_max_nodes).await
}

#[cfg(not(target_os = "linux"))]
async fn run_stub(
    display_id: String,
    tx: broadcast::Sender<BusMessage>,
    bus_path: String,
) -> Result<()> {
    let mut bus_rx = tx.subscribe();
    tokio::spawn(async move {
        forward_to_bus(bus_path, &mut bus_rx).await;
    });
    loop {
        let event = A11yEvent {
            ts: Utc::now(),
            display_id: display_id.clone(),
            kind: A11yEventKind::Snapshot,
            nodes: vec![A11yNode {
                id: "root".into(),
                parent: None,
                role: "window".into(),
                name: "stub-root".into(),
                description: Some("replace with AT-SPI2 tree".into()),
                bounds: None,
                state: BTreeMap::new(),
            }],
        };
        let msg = BusMessage::A11y(event);
        let _ = tx.send(msg.clone());
        sleep(Duration::from_secs(1)).await;
    }
}

async fn forward_to_bus(bus_path: String, rx: &mut broadcast::Receiver<BusMessage>) {
    let mut bus = connect_socket(&bus_path).await.ok();
    while let Ok(msg) = rx.recv().await {
        if bus.is_none() {
            bus = connect_socket(&bus_path).await.ok();
        }
        if let Some(stream) = bus.as_mut()
            && send_msg(stream, &msg).await.is_err()
        {
            bus = None;
        }
    }
}
