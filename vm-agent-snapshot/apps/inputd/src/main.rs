use agent_common::{AgentConfig, bind_socket, init_tracing, monotonic_ns, recv_msg, send_msg};
use agent_proto::{ActionResult, BusMessage};
use anyhow::Result;
use clap::Parser;
#[cfg(target_os = "linux")]
use std::sync::Arc;
#[cfg(target_os = "linux")]
use tokio::sync::Mutex;
use tracing::info;

#[cfg(target_os = "linux")]
mod linux;

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "config/agent.example.toml")]
    config: String,
    #[arg(long, default_value_t = 1920)]
    screen_width: i32,
    #[arg(long, default_value_t = 1080)]
    screen_height: i32,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let args = Args::parse();
    let cfg = AgentConfig::load(&args.config)?;
    let listener = bind_socket(&cfg.sockets.input).await?;
    #[cfg(target_os = "linux")]
    let injector = Arc::new(Mutex::new(linux::UInputInjector::new(
        args.screen_width,
        args.screen_height,
    )?));
    info!("input daemon listening on {}", cfg.sockets.input);

    loop {
        let (mut stream, _) = listener.accept().await?;
        #[cfg(target_os = "linux")]
        let injector = injector.clone();
        tokio::spawn(async move {
            loop {
                let msg = match recv_msg::<BusMessage>(&mut stream).await {
                    Ok(msg) => msg,
                    Err(err) => {
                        tracing::debug!(?err, "input stream closed");
                        return;
                    }
                };
                if let BusMessage::ActionRequest(req) = msg {
                    #[cfg(target_os = "linux")]
                    let accepted = injector.lock().await.handle(&req).is_ok();
                    #[cfg(not(target_os = "linux"))]
                    let accepted = true;
                    let result = ActionResult {
                        task_id: req.task_id,
                        step_id: req.step_id,
                        accepted,
                        injected_at_ns: Some(monotonic_ns()),
                        detail: if accepted {
                            "accepted by input subsystem".into()
                        } else {
                            "input injection failed".into()
                        },
                    };
                    if let Err(err) = send_msg(&mut stream, &BusMessage::ActionResult(result)).await
                    {
                        tracing::error!(?err, "failed sending input result");
                        return;
                    }
                }
            }
        });
    }
}
