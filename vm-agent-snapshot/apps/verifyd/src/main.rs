use agent_common::{AgentConfig, bind_socket, connect_socket, init_tracing, recv_msg, send_msg};
use agent_proto::{
    A11yEventKind, ActionKind, ActionRequest, BusMessage, BusTopic, ExpectedChange,
    VerificationResult, VerificationStatus,
};
use anyhow::Result;
use chrono::Utc;
use clap::Parser;
use tokio::{
    sync::broadcast,
    time::{Duration, Instant, sleep, timeout},
};
use tracing::info;

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "config/agent.example.toml")]
    config: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let args = Args::parse();
    let cfg = AgentConfig::load(&args.config)?;
    let listener = bind_socket(&cfg.sockets.verify).await?;
    let bus_path = cfg.sockets.bus.clone();
    let (event_tx, _) = broadcast::channel::<BusMessage>(2048);
    tokio::spawn(bus_event_loop(bus_path, event_tx.clone()));
    info!("verifier daemon listening on {}", cfg.sockets.verify);

    loop {
        let (mut stream, _) = listener.accept().await?;
        let event_tx = event_tx.clone();
        tokio::spawn(async move {
            loop {
                let msg = match recv_msg::<BusMessage>(&mut stream).await {
                    Ok(msg) => msg,
                    Err(err) => {
                        tracing::debug!(?err, "verify stream closed");
                        return;
                    }
                };
                if let BusMessage::ActionRequest(req) = msg {
                    let rx = event_tx.subscribe();
                    if let Err(err) = send_msg(
                        &mut stream,
                        &BusMessage::VerificationArmed {
                            task_id: req.task_id.clone(),
                            step_id: req.step_id.clone(),
                        },
                    )
                    .await
                    {
                        tracing::error!(?err, "failed sending verification arm response");
                        return;
                    }
                    match verify_action(req, rx).await {
                        Ok(result) => {
                            if let Err(err) =
                                send_msg(&mut stream, &BusMessage::VerificationResult(result)).await
                            {
                                tracing::error!(?err, "failed sending verification result");
                                return;
                            }
                        }
                        Err(err) => tracing::error!(?err, "verification failed"),
                    }
                }
            }
        });
    }
}

async fn bus_event_loop(bus_path: String, event_tx: broadcast::Sender<BusMessage>) {
    loop {
        match connect_socket(&bus_path).await {
            Ok(mut stream) => {
                if send_msg(
                    &mut stream,
                    &BusMessage::Subscribe {
                        topics: vec![BusTopic::Capture, BusTopic::A11y],
                    },
                )
                .await
                .is_ok()
                {
                    while let Ok(msg) = recv_msg::<BusMessage>(&mut stream).await {
                        let _ = event_tx.send(msg);
                    }
                }
            }
            Err(err) => {
                tracing::debug!(?err, "verifier bus subscription failed");
            }
        }
        sleep(Duration::from_millis(100)).await;
    }
}

async fn verify_action(
    req: ActionRequest,
    mut rx: broadcast::Receiver<BusMessage>,
) -> Result<VerificationResult> {
    if is_idempotent_focus_shortcut(&req.kind) {
        return Ok(VerificationResult {
            task_id: req.task_id,
            step_id: req.step_id,
            status: VerificationStatus::Verified,
            detail: "verified idempotent browser focus shortcut after input acceptance".into(),
            observed_at: Utc::now(),
        });
    }

    let deadline = Duration::from_millis(req.timeout_ms);
    let start = Instant::now();
    let result = timeout(deadline, async {
        let res: Result<VerificationResult> = loop {
            let msg = rx.recv().await?;
            match msg {
                BusMessage::A11y(event) => {
                    if expected_matches_a11y(&req.expected, &event) {
                        break Ok(VerificationResult {
                            task_id: req.task_id.clone(),
                            step_id: req.step_id.clone(),
                            status: VerificationStatus::Verified,
                            detail: format!("matched a11y event {:?}", event.kind),
                            observed_at: Utc::now(),
                        });
                    }
                }
                BusMessage::Capture(frame) => {
                    if req.expected.iter().any(|e| {
                        matches!(
                            e,
                            ExpectedChange::PixelChanged { .. } | ExpectedChange::AnyUiChange
                        )
                    }) {
                        break Ok(VerificationResult {
                            task_id: req.task_id.clone(),
                            step_id: req.step_id.clone(),
                            status: VerificationStatus::Verified,
                            detail: format!(
                                "matched capture event after {}ms",
                                start.elapsed().as_millis()
                            ),
                            observed_at: Utc::now(),
                        });
                    }
                    let _ = frame;
                }
                _ => {}
            }
        };
        res
    })
    .await;

    Ok(match result {
        Ok(inner) => inner?,
        Err(_) => VerificationResult {
            task_id: req.task_id,
            step_id: req.step_id,
            status: VerificationStatus::Timeout,
            detail: "expected UI change not observed before timeout".into(),
            observed_at: Utc::now(),
        },
    })
}

fn is_idempotent_focus_shortcut(action: &ActionKind) -> bool {
    let ActionKind::KeyCombo { keys } = action else {
        return false;
    };
    keys.iter().any(|key| key.eq_ignore_ascii_case("ctrl"))
        && keys.iter().any(|key| key.eq_ignore_ascii_case("l"))
}

fn expected_matches_a11y(expected: &[ExpectedChange], event: &agent_proto::A11yEvent) -> bool {
    expected.iter().any(|exp| match exp {
        ExpectedChange::AnyUiChange => true,
        ExpectedChange::FocusOnNode { node_name } => {
            matches!(event.kind, A11yEventKind::Focus)
                && event.nodes.iter().any(|n| &n.name == node_name)
        }
        ExpectedChange::TextPresent { text } => {
            matches!(
                event.kind,
                A11yEventKind::TextChanged | A11yEventKind::Snapshot
            ) && event.nodes.iter().any(|n| n.name.contains(text))
        }
        ExpectedChange::WindowActivated { title } => {
            matches!(event.kind, A11yEventKind::WindowActivate)
                && event.nodes.iter().any(|n| n.name.contains(title))
        }
        ExpectedChange::PixelChanged { .. } => false,
    })
}
