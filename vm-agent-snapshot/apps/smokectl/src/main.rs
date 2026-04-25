use agent_common::{AgentConfig, connect_socket, init_tracing, recv_msg, send_msg};
use agent_proto::{
    ActionKind, ActionRequest, ActionResult, BusMessage, ExpectedChange, MouseButton,
    VerificationStatus,
};
use anyhow::{Context, Result, bail};
use clap::Parser;
use serde_json::json;
use tokio::time::{Duration, sleep, timeout};

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "config/agent.example.toml")]
    config: String,
    #[arg(long, default_value = ":99")]
    display_id: String,
    #[arg(long, default_value_t = 100)]
    x: i32,
    #[arg(long, default_value_t = 100)]
    y: i32,
    #[arg(long, default_value_t = 2000)]
    timeout_ms: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let args = Args::parse();
    let cfg = AgentConfig::load(&args.config)?;
    let mut input = connect_socket(&cfg.sockets.input)
        .await
        .context("connect input socket")?;
    let mut verify = connect_socket(&cfg.sockets.verify)
        .await
        .context("connect verify socket")?;

    let task_id = format!("smoke-{}", chrono::Utc::now().timestamp_millis());
    let reset_req = ActionRequest {
        task_id: task_id.clone(),
        step_id: "reset-pointer".into(),
        display_id: args.display_id.clone(),
        goal: "linux real smoke: reset pointer to top-left".into(),
        rationale:
            "relative motion is deterministic on dummy Xorg after pointer acceleration is disabled"
                .into(),
        kind: ActionKind::MovePointer {
            x: -10_000,
            y: -10_000,
            absolute: false,
        },
        expected: vec![ExpectedChange::AnyUiChange],
        timeout_ms: args.timeout_ms,
    };
    let reset_result = send_input_action(&mut input, &reset_req).await?;

    sleep(Duration::from_millis(50)).await;

    let move_req = ActionRequest {
        task_id: task_id.clone(),
        step_id: "move-pointer".into(),
        display_id: args.display_id.clone(),
        goal: "linux real smoke: move pointer to pixel probe".into(),
        rationale: "place pointer over the probe before verifier is armed".into(),
        kind: ActionKind::MovePointer {
            x: args.x,
            y: args.y,
            absolute: false,
        },
        expected: vec![ExpectedChange::AnyUiChange],
        timeout_ms: args.timeout_ms,
    };
    let move_result = send_input_action(&mut input, &move_req).await?;

    sleep(Duration::from_millis(50)).await;

    let click_req = ActionRequest {
        task_id: task_id.clone(),
        step_id: "click-probe".into(),
        display_id: args.display_id,
        goal: "linux real smoke: click X11 pixel probe and observe XDamage".into(),
        rationale: "the probe flips pixels on click, which must produce a capture event".into(),
        kind: ActionKind::Click {
            button: MouseButton::Left,
            count: 1,
            x: None,
            y: None,
        },
        expected: vec![ExpectedChange::AnyUiChange],
        timeout_ms: args.timeout_ms,
    };

    send_msg(&mut verify, &BusMessage::ActionRequest(click_req.clone())).await?;
    match timeout(
        Duration::from_millis(1000),
        recv_msg::<BusMessage>(&mut verify),
    )
    .await??
    {
        BusMessage::VerificationArmed { task_id, step_id }
            if task_id == click_req.task_id && step_id == click_req.step_id => {}
        other => bail!("unexpected verifier arm response: {other:?}"),
    }

    let click_result = send_input_action(&mut input, &click_req).await?;

    let verification = match timeout(
        Duration::from_millis(args.timeout_ms.saturating_add(1000)),
        recv_msg::<BusMessage>(&mut verify),
    )
    .await??
    {
        BusMessage::VerificationResult(result) => result,
        other => bail!("unexpected verification response: {other:?}"),
    };

    let success = matches!(verification.status, VerificationStatus::Verified);
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "success": success,
            "reset_result": reset_result,
            "move_result": move_result,
            "click_result": click_result,
            "verification": verification
        }))?
    );

    if !success {
        bail!("linux real smoke failed verification");
    }
    Ok(())
}

async fn send_input_action(
    input: &mut tokio::net::UnixStream,
    req: &ActionRequest,
) -> Result<ActionResult> {
    send_msg(input, &BusMessage::ActionRequest(req.clone())).await?;
    let result = match recv_msg::<BusMessage>(input).await? {
        BusMessage::ActionResult(result) => result,
        other => bail!("unexpected input response for {}: {other:?}", req.step_id),
    };
    if !result.accepted {
        bail!("{} was rejected: {}", req.step_id, result.detail);
    }
    Ok(result)
}
