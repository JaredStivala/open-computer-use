#[cfg(not(target_os = "linux"))]
use agent_common::monotonic_ns;
use agent_common::{AgentConfig, bind_socket, connect_socket, init_tracing, send_msg};
use agent_proto::BusMessage;
#[cfg(not(target_os = "linux"))]
use agent_proto::{CaptureFrame, FrameEncoding, Rect};
use anyhow::Result;
#[cfg(not(target_os = "linux"))]
use chrono::Utc;
use clap::Parser;
use std::sync::Arc;
#[cfg(not(target_os = "linux"))]
use std::time::Duration;
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
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let args = Args::parse();
    let cfg = AgentConfig::load(&args.config)?;
    let listener = bind_socket(&cfg.sockets.capture).await?;
    let (tx, _) = broadcast::channel::<BusMessage>(512);
    let latest = Arc::new(RwLock::new(None::<BusMessage>));
    let latest_writer = latest.clone();
    let mut latest_rx = tx.subscribe();
    tokio::spawn(async move {
        while let Ok(msg) = latest_rx.recv().await {
            *latest_writer.write().await = Some(msg);
        }
    });
    let display_id = args.display_id.clone();
    let bus_path = cfg.sockets.bus.clone();
    let producer_tx = tx.clone();
    tokio::spawn(async move {
        #[cfg(target_os = "linux")]
        let run = run_linux(display_id, producer_tx, bus_path);
        #[cfg(not(target_os = "linux"))]
        let run = run_stub(display_id, producer_tx, bus_path);
        if let Err(err) = run.await {
            tracing::error!(?err, "capture producer failed");
        }
    });
    info!("capture daemon listening on {}", cfg.sockets.capture);

    loop {
        let (mut stream, _) = listener.accept().await?;
        if let Some(msg) = latest.read().await.clone()
            && let Err(err) = send_msg(&mut stream, &msg).await
        {
            tracing::debug!(?err, "capture client closed before latest frame");
            continue;
        }
        let mut rx = tx.subscribe();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(msg) => {
                        if let Err(err) = send_msg(&mut stream, &msg).await {
                            tracing::debug!(?err, "capture client closed");
                            return;
                        }
                    }
                    Err(err) => {
                        tracing::debug!(?err, "capture broadcast closed");
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
) -> Result<()> {
    let mut bus_rx = tx.subscribe();
    tokio::spawn(async move {
        forward_to_bus(bus_path, &mut bus_rx).await;
    });
    tokio::task::spawn_blocking(move || linux::run_capture_loop(display_id, tx)).await??;
    Ok(())
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
        let frame = CaptureFrame {
            ts: Utc::now(),
            display_id: display_id.clone(),
            dirty: Rect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            width: 1,
            height: 1,
            stride_bytes: 0,
            encoding: FrameEncoding::Jpeg,
            bytes: Vec::new(),
            monotonic_ns: monotonic_ns(),
        };
        let msg = BusMessage::Capture(frame);
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
