use agent_common::{AgentConfig, bind_socket, init_tracing, recv_msg, send_msg};
use agent_proto::{BusMessage, BusTopic};
use anyhow::Result;
use clap::Parser;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio::{
    net::UnixStream,
    sync::{Mutex, mpsc},
};
use tracing::{debug, info};

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "config/agent.example.toml")]
    config: String,
}

#[derive(Clone)]
struct Subscriber {
    id: u64,
    topics: Vec<BusTopic>,
    tx: mpsc::Sender<BusMessage>,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let args = Args::parse();
    let cfg = AgentConfig::load(&args.config)?;
    let listener = bind_socket(&cfg.sockets.bus).await?;
    let subscribers: Arc<Mutex<Vec<Subscriber>>> = Arc::new(Mutex::new(Vec::new()));
    let next_id = Arc::new(AtomicU64::new(1));
    info!("bus daemon listening on {}", cfg.sockets.bus);

    loop {
        let (stream, _) = listener.accept().await?;
        let subscribers = subscribers.clone();
        let id = next_id.fetch_add(1, Ordering::Relaxed);
        tokio::spawn(async move {
            if let Err(err) = handle_client(id, stream, subscribers).await {
                debug!(%id, ?err, "bus client closed");
            }
        });
    }
}

async fn handle_client(
    id: u64,
    stream: UnixStream,
    subscribers: Arc<Mutex<Vec<Subscriber>>>,
) -> Result<()> {
    let result = handle_client_inner(id, stream, subscribers.clone()).await;
    subscribers.lock().await.retain(|s| s.id != id);
    result
}

async fn handle_client_inner(
    id: u64,
    mut stream: UnixStream,
    subscribers: Arc<Mutex<Vec<Subscriber>>>,
) -> Result<()> {
    let (tx, mut rx) = mpsc::channel::<BusMessage>(512);
    let first = recv_msg::<BusMessage>(&mut stream).await?;

    match first {
        BusMessage::Subscribe { topics } => {
            subscribers.lock().await.push(Subscriber {
                id,
                topics,
                tx: tx.clone(),
            });
            loop {
                tokio::select! {
                    outbound = rx.recv() => {
                        match outbound {
                            Some(msg) => send_msg(&mut stream, &msg).await?,
                            None => break Ok(()),
                        }
                    }
                    inbound = recv_msg::<BusMessage>(&mut stream) => {
                        match inbound {
                            Ok(msg) => publish(&subscribers, id, msg).await,
                            Err(err) => return Err(err),
                        }
                    }
                }
            }
        }
        msg => {
            publish(&subscribers, id, msg).await;
            loop {
                match recv_msg::<BusMessage>(&mut stream).await {
                    Ok(msg) => publish(&subscribers, id, msg).await,
                    Err(err) => return Err(err),
                }
            }
        }
    }
}

async fn publish(subscribers: &Arc<Mutex<Vec<Subscriber>>>, sender_id: u64, msg: BusMessage) {
    let Some(topic) = msg.topic() else {
        return;
    };

    let subscribers_snapshot = subscribers.lock().await.clone();
    let mut dead = Vec::new();
    for sub in subscribers_snapshot {
        if sub.id == sender_id || !sub.topics.contains(&topic) {
            continue;
        }
        if sub.tx.try_send(msg.clone()).is_err() {
            dead.push(sub.id);
        }
    }

    if !dead.is_empty() {
        subscribers.lock().await.retain(|s| !dead.contains(&s.id));
    }
}
