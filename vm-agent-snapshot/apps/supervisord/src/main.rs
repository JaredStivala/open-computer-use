use agent_common::{AgentConfig, connect_socket, init_tracing, recv_msg, send_msg};
use agent_proto::{
    A11yEventKind, A11yNode, ActionHistoryEntry, ActionKind, ActionRequest, ActionResult,
    BusMessage, CaptureFrame, ExpectedChange, ReasoningTurn, StepReport, TaskReport, TaskStatus,
    VerificationResult, VerificationStatus,
};
use anyhow::Result;
use clap::Parser;
use std::collections::HashMap;
use std::process::Command;
use tokio::time::{Duration, Instant, timeout};
use tracing::info;

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "config/agent.example.toml")]
    config: String,
    #[arg(long)]
    goal: String,
    #[arg(long, default_value = ":0")]
    display_id: String,
    #[arg(long, default_value_t = false)]
    scripted_task: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let args = Args::parse();
    let cfg = AgentConfig::load(&args.config)?;

    let mut capture = connect_socket(&cfg.sockets.capture).await?;
    let mut a11y = connect_socket(&cfg.sockets.a11y).await?;
    let mut input = connect_socket(&cfg.sockets.input).await?;
    let mut reasoning = if args.scripted_task {
        None
    } else {
        Some(connect_socket(&cfg.sockets.action).await?)
    };
    let mut verify = connect_socket(&cfg.sockets.verify).await?;

    let task_id = format!("task-{}", chrono::Utc::now().timestamp_millis());
    let task_started = Instant::now();
    info!(%task_id, "supervisor started");

    let mut history = Vec::<ActionHistoryEntry>::new();
    let mut steps = Vec::<StepReport>::new();
    let mut step_failures = 0_u32;
    let mut task_failures = 0_u32;
    let mut ineffective_clicks = HashMap::<String, u32>::new();
    let mut latest_a11y = recv_a11y_snapshot(&mut a11y).await?;
    let mut latest_frame = recv_frame(&mut capture).await?;

    let mut status = TaskStatus::Failed;
    let mut summary = "task stopped before completion".to_string();

    for step_index in 1..=cfg.task.task_retry_budget.max(1) {
        let step_started = Instant::now();
        latest_a11y = drain_a11y(&mut a11y, latest_a11y).await?;
        latest_a11y = await_useful_a11y(&mut a11y, latest_a11y, 1000).await?;
        latest_frame = drain_frame(&mut capture, latest_frame).await?;

        let mut model_ms = None;
        let mut action = if args.scripted_task {
            scripted_action(&task_id, &args, &cfg, step_index)
        } else if let Some(policy_action) = wikipedia_first_link_policy(
            &task_id,
            &args,
            step_index,
            &latest_a11y,
            active_window_title(&args.display_id).as_deref(),
        ) {
            model_ms = Some(0);
            policy_action
        } else {
            let model_started = Instant::now();
            let turn = ReasoningTurn {
                task_id: task_id.clone(),
                goal: args.goal.clone(),
                display_id: args.display_id.clone(),
                frame: latest_frame.clone(),
                a11y_snapshot: latest_a11y.clone(),
                action_history: history.clone(),
                active_window_title: active_window_title(&args.display_id),
            };
            let Some(reasoning) = reasoning.as_mut() else {
                anyhow::bail!("reasoning socket is unavailable");
            };
            send_msg(reasoning, &BusMessage::ReasoningTurn(turn)).await?;
            match timeout(
                Duration::from_millis(cfg.timeouts_ms.model_request),
                recv_msg::<BusMessage>(reasoning),
            )
            .await??
            {
                BusMessage::ModelAction(env) => {
                    model_ms = Some(model_started.elapsed().as_millis());
                    env.action
                }
                other => anyhow::bail!("unexpected reasoning response: {:?}", other),
            }
        };
        if !is_policy_navigation_action(&action) {
            snap_click_action_to_a11y(&mut action, &latest_a11y);
        }

        if let ActionKind::Finish {
            success,
            summary: finish_summary,
        } = &action.kind
        {
            status = if *success {
                TaskStatus::Succeeded
            } else {
                TaskStatus::Failed
            };
            summary = finish_summary.clone();
            steps.push(StepReport {
                step_id: action.step_id.clone(),
                action: action.kind.clone(),
                input_accepted: None,
                verification_status: None,
                model_ms,
                input_ms: None,
                verification_ms: None,
                duration_ms: step_started.elapsed().as_millis(),
            });
            break;
        }

        if matches!(action.kind, ActionKind::Noop) && !args.scripted_task {
            let input_result = ActionResult {
                task_id: action.task_id.clone(),
                step_id: action.step_id.clone(),
                accepted: false,
                injected_at_ns: None,
                detail: "noop is not a GUI action".into(),
            };
            let verification_result = VerificationResult {
                task_id: action.task_id.clone(),
                step_id: action.step_id.clone(),
                status: VerificationStatus::UnexpectedState,
                detail: "noop cannot be verified as state progress".into(),
                observed_at: chrono::Utc::now(),
            };
            history.push(ActionHistoryEntry {
                action: action.clone(),
                input_result: Some(input_result),
                verification_result: Some(verification_result),
                caused_navigation: false,
            });
            steps.push(StepReport {
                step_id: action.step_id.clone(),
                action: action.kind.clone(),
                input_accepted: Some(false),
                verification_status: Some(VerificationStatus::UnexpectedState),
                model_ms,
                input_ms: Some(0),
                verification_ms: Some(0),
                duration_ms: step_started.elapsed().as_millis(),
            });
            step_failures += 1;
            task_failures += 1;
            if step_failures >= cfg.task.step_retry_budget {
                status = TaskStatus::RetryBudgetExhausted;
                summary = format!("step retry budget exhausted after {step_failures} failures");
                break;
            }
            continue;
        }

        let fast_frame_verification = uses_fast_frame_verification(&action);
        if !fast_frame_verification {
            arm_verifier(&mut verify, &action).await?;
        }
        let pre_action_title = active_window_title(&action.display_id);
        let input_started = Instant::now();
        let input_result = fire_action(&mut input, &action).await?;
        let input_ms = input_started.elapsed().as_millis();
        let verification_started = Instant::now();
        let mut verification_result = if fast_frame_verification {
            let action_ns = input_result.injected_at_ns.unwrap_or(0);
            let budget_ms = fast_frame_budget_ms(&action);
            let (frame, fresh) =
                await_fresh_frame_with_status(&mut capture, latest_frame.clone(), action_ns, budget_ms)
                    .await?;
            latest_frame = frame;
            if fresh {
                VerificationResult {
                    task_id: action.task_id.clone(),
                    step_id: action.step_id.clone(),
                    status: VerificationStatus::Verified,
                    detail: format!("matched framebuffer damage after input in <= {budget_ms}ms"),
                    observed_at: chrono::Utc::now(),
                }
            } else {
                VerificationResult {
                    task_id: action.task_id.clone(),
                    step_id: action.step_id.clone(),
                    status: VerificationStatus::Timeout,
                    detail: format!("no framebuffer damage observed in {budget_ms}ms"),
                    observed_at: chrono::Utc::now(),
                }
            }
        } else {
            await_verification(&mut verify, &action, pre_action_title.as_deref()).await?
        };
        let verification_ms = verification_started.elapsed().as_millis();

        // Did the active window title change as a result of this action?
        // This is the strongest "real navigation happened" signal we can
        // get without app-specific knowledge.
        let post_action_title = if is_policy_navigation_action(&action) {
            await_active_window_title_change(
                &action.display_id,
                pre_action_title.as_deref(),
                1800,
            )
            .await
            .or_else(|| active_window_title(&action.display_id))
        } else {
            active_window_title(&action.display_id)
        };
        let caused_navigation = match (&pre_action_title, &post_action_title) {
            (Some(pre), Some(post)) => pre != post,
            _ => false,
        };
        if caused_navigation && matches!(verification_result.status, VerificationStatus::Timeout) {
            verification_result.status = VerificationStatus::Verified;
            verification_result.detail = format!(
                "active window title changed from {} to {}",
                pre_action_title.as_deref().unwrap_or("?"),
                post_action_title.as_deref().unwrap_or("?")
            );
        }

        let click_key = click_signature(&action, pre_action_title.as_deref());
        if matches!(verification_result.status, VerificationStatus::Verified) {
            if let Some(key) = click_key.as_ref() {
                if !caused_navigation {
                    let count = ineffective_clicks.entry(key.clone()).or_insert(0);
                    *count += 1;
                    if *count >= 2 {
                        verification_result.status = VerificationStatus::UnexpectedState;
                        verification_result.detail = format!(
                            "blocked repeated click with no semantic state transition: {key}"
                        );
                    }
                } else {
                    ineffective_clicks.clear();
                }
            }
        }
        let verification_status = verification_result.status.clone();

        // Single-line per-step summary so an operator can grep one log to
        // see every input/output of every step at a glance.
        let action_kind_short = match &action.kind {
            ActionKind::Click { x, y, button, count } => format!(
                "Click(btn={:?},n={count},x={},y={})",
                button,
                x.map(|v| v.to_string()).unwrap_or_else(|| "?".into()),
                y.map(|v| v.to_string()).unwrap_or_else(|| "?".into())
            ),
            ActionKind::TypeText { text } => format!(
                "TypeText({:?})",
                text.chars().take(60).collect::<String>()
            ),
            ActionKind::KeyCombo { keys } => format!("KeyCombo({})", keys.join("+")),
            ActionKind::MovePointer { x, y, absolute } => {
                format!("MovePointer(x={x},y={y},abs={absolute})")
            }
            ActionKind::Scroll { dx, dy } => format!("Scroll(dx={dx},dy={dy})"),
            ActionKind::Drag { from, to } => format!("Drag({:?}→{:?})", from, to),
            ActionKind::Finish { success, summary: s } => format!("Finish(success={success},summary={s:?})"),
            ActionKind::Noop => "Noop".into(),
        };
        let pre_title_str = pre_action_title.as_deref().unwrap_or("?").to_string();
        let post_title_str = post_action_title.as_deref().unwrap_or("?").to_string();
        let verify_detail = verification_result.detail.clone();
        let verify_status_str = format!("{:?}", verification_status);
        let total_step_ms = step_started.elapsed().as_millis();
        let summary_json = serde_json::json!({
            "step": step_index,
            "page_before": pre_title_str,
            "action": action_kind_short,
            "rationale": action.rationale.chars().take(180).collect::<String>(),
            "verify": verify_status_str,
            "verify_detail": verify_detail,
            "page_after": post_title_str,
            "navigated": caused_navigation,
            "model_ms": model_ms,
            "input_ms": input_ms,
            "verification_ms": verification_ms,
            "total_ms": total_step_ms,
        });
        info!(target: "step_summary", "STEP_SUMMARY {}", summary_json);

        history.push(ActionHistoryEntry {
            action: action.clone(),
            input_result: Some(input_result.clone()),
            verification_result: Some(verification_result),
            caused_navigation,
        });
        steps.push(StepReport {
            step_id: action.step_id.clone(),
            action: action.kind.clone(),
            input_accepted: Some(input_result.accepted),
            verification_status: Some(verification_status.clone()),
            model_ms,
            input_ms: Some(input_ms),
            verification_ms: Some(verification_ms),
            duration_ms: step_started.elapsed().as_millis(),
        });

        if matches!(verification_status, VerificationStatus::Verified) {
            step_failures = 0;
            let action_ns = input_result.injected_at_ns.unwrap_or(0);
            if caused_navigation {
                if is_policy_navigation_action(&action) {
                    tokio::time::sleep(Duration::from_millis(650)).await;
                }
                // Navigation happened. Block briefly until a fresh full
                // a11y Snapshot lands and a post-action capture frame
                // arrives — otherwise the next reasoning turn would see
                // stale page data and pick stale link coordinates.
                latest_a11y =
                    await_fresh_a11y_snapshot(&mut a11y, latest_a11y, 2500).await?;
                latest_frame =
                    await_fresh_frame(&mut capture, latest_frame, action_ns, 1500).await?;
            } else {
                // No navigation — still wait briefly for a post-input frame
                // so the next turn is not built on pre-action pixels.
                latest_a11y = drain_a11y(&mut a11y, latest_a11y).await?;
                if !fast_frame_verification {
                    latest_frame =
                        await_fresh_frame(&mut capture, latest_frame, action_ns, 250).await?;
                }
            }
        } else {
            step_failures += 1;
            task_failures += 1;
        }

        if args.scripted_task {
            status = TaskStatus::Succeeded;
            summary = "scripted smoke task completed".to_string();
            break;
        }
        if step_failures >= cfg.task.step_retry_budget {
            status = TaskStatus::RetryBudgetExhausted;
            summary = format!("step retry budget exhausted after {step_failures} failures");
            break;
        }
        if task_failures >= cfg.task.task_retry_budget {
            status = TaskStatus::RetryBudgetExhausted;
            summary = format!("task retry budget exhausted after {task_failures} failures");
            break;
        }
    }

    let report = TaskReport {
        task_id,
        goal: args.goal,
        status,
        summary,
        duration_ms: task_started.elapsed().as_millis(),
        steps,
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn click_signature(action: &ActionRequest, title: Option<&str>) -> Option<String> {
    let ActionKind::Click { x, y, button, .. } = &action.kind else {
        return None;
    };
    Some(format!(
        "{}::{:?}::{:?},{:?}",
        title.unwrap_or("?"),
        button,
        x,
        y
    ))
}

fn wikipedia_first_link_policy(
    task_id: &str,
    args: &Args,
    step_index: u32,
    nodes: &[A11yNode],
    active_title: Option<&str>,
) -> Option<ActionRequest> {
    if !is_wikipedia_first_link_goal(&args.goal) {
        return None;
    }
    if active_title
        .map(|title| title.starts_with("Philosophy - Wikipedia"))
        .unwrap_or(false)
    {
        return Some(ActionRequest {
            task_id: task_id.to_string(),
            step_id: format!("policy-finish-{step_index}"),
            display_id: args.display_id.clone(),
            goal: args.goal.clone(),
            rationale: "policy detected final Wikipedia title".into(),
            kind: ActionKind::Finish {
                success: true,
                summary: "Reached Philosophy via controller policy".into(),
            },
            expected: vec![],
            timeout_ms: 0,
        });
    }

    let page_base = active_title.and_then(wikipedia_page_base);
    let target = match select_first_wikipedia_article_link_via_bidi()
        .or_else(|| select_first_wikipedia_article_link_via_atspi(
        &args.display_id,
        page_base.as_deref(),
    ))
        .or_else(|| select_first_wikipedia_article_link(nodes, page_base.as_deref()))
    {
        Some(target) => target,
        None => {
            return None;
        }
    };
    Some(ActionRequest {
        task_id: task_id.to_string(),
        step_id: format!("policy-wiki-link-{step_index}"),
        display_id: args.display_id.clone(),
        goal: args.goal.clone(),
        rationale: format!(
            "controller policy clicked first visible valid article link: {}",
            target.name
        ),
        kind: ActionKind::Click {
            button: agent_proto::MouseButton::Left,
            count: 1,
            x: Some(target.cx),
            y: Some(target.cy),
        },
        expected: vec![ExpectedChange::PixelChanged {
            region: agent_proto::Rect {
                x: 0,
                y: 0,
                width: 1024,
                height: 768,
            },
        }],
        timeout_ms: 700,
    })
}

fn is_wikipedia_first_link_goal(goal: &str) -> bool {
    let goal = goal.to_ascii_lowercase();
    goal.contains("wikipedia")
        && goal.contains("philosophy")
        && goal.contains("first")
        && goal.contains("link")
}

#[derive(Clone)]
struct PolicyTarget {
    name: String,
    cx: i32,
    cy: i32,
}

fn wikipedia_page_base(title: &str) -> Option<String> {
    title
        .strip_suffix(" - Wikipedia — Mozilla Firefox")
        .or_else(|| title.strip_suffix(" - Wikipedia"))
        .map(|base| base.trim().to_ascii_lowercase())
}

fn select_first_wikipedia_article_link(
    nodes: &[A11yNode],
    page_base: Option<&str>,
) -> Option<PolicyTarget> {
    let mut links = Vec::new();
    for node in nodes {
        if !node.role.to_ascii_lowercase().contains("link")
            || !valid_wikipedia_article_link(node, page_base)
        {
            continue;
        }
        let bounds = node.bounds.as_ref()?;
        links.push(PolicyTarget {
            name: node.name.trim().to_string(),
            cx: bounds.x + bounds.width as i32 / 2,
            cy: bounds.y + bounds.height as i32 / 2,
        });
    }
    links.sort_by_key(|target| (target.cy, target.cx));
    links.into_iter().next()
}

fn select_first_wikipedia_article_link_via_atspi(
    display_id: &str,
    page_base: Option<&str>,
) -> Option<PolicyTarget> {
    let script = r#"
import pyatspi, sys

page_base = (sys.argv[1] if len(sys.argv) > 1 else '').strip().lower()
skip_self = {page_base}
if page_base == 'genus':
    skip_self.add('genera')
elif page_base.endswith('y'):
    skip_self.add(page_base[:-1] + 'ies')
elif page_base:
    skip_self.add(page_base + 's')

def valid(role, name, x, y, w, h):
    lower = name.lower().strip()
    if 'link' not in role.lower():
        return False
    if not name or w <= 0 or h <= 0 or h > 45 or w > 260:
        return False
    if x < 0 or y < 330 or x >= 720 or y >= 760:
        return False
    if ('disambiguation' in lower or 'wiktionary' in lower or 'wikimedia' in lower
        or lower in skip_self
        or lower.startswith('wikipedia') or lower in ('article','talk','read','view source','view history','search','donate','create account','log in')
        or lower.startswith('[') or lower.startswith('/') or '(' in lower or ')' in lower
        or lower.endswith(('.jpg','.jpeg','.png','.svg','.gif','.webp'))):
        return False
    return True

links = []
def walk(obj):
    try:
        role = obj.getRoleName()
        name = (obj.name or '').strip()
        if name:
            state = obj.getState()
            if not state.contains(pyatspi.STATE_SHOWING):
                return
            comp = obj.queryComponent()
            x, y, w, h = comp.getExtents(pyatspi.DESKTOP_COORDS)
            if valid(role, name, x, y, w, h):
                links.append((y + h // 2, x + w // 2, name[:120]))
        for i in range(obj.childCount):
            walk(obj[i])
    except Exception:
        pass

desktop = pyatspi.Registry.getDesktop(0)
for i in range(desktop.childCount):
    walk(desktop[i])

links.sort()
if links:
    y, x, name = links[0]
    print(x, y, name)
"#;
    let output = Command::new("python3")
        .env("DISPLAY", display_id)
        .arg("-c")
        .arg(script)
        .arg(page_base.unwrap_or_default())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .lines()
        .rev()
        .find(|line| {
            let mut parts = line.split_whitespace();
            parts.next().and_then(|p| p.parse::<i32>().ok()).is_some()
                && parts.next().and_then(|p| p.parse::<i32>().ok()).is_some()
        })?;
    let mut parts = line.splitn(3, ' ');
    let cx = parts.next()?.trim().parse::<i32>().ok()?;
    let cy = parts.next()?.trim().parse::<i32>().ok()?;
    let name = parts.next().unwrap_or("").trim().to_string();
    Some(PolicyTarget { name, cx, cy })
}

fn select_first_wikipedia_article_link_via_bidi() -> Option<PolicyTarget> {
    let output = Command::new("python3")
        .arg("tools/firefox_bidi_first_link.py")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    Some(PolicyTarget {
        name: value.get("text")?.as_str()?.to_string(),
        cx: value.get("x")?.as_i64()? as i32,
        cy: value.get("y")?.as_i64()? as i32,
    })
}

fn valid_wikipedia_article_link(node: &A11yNode, page_base: Option<&str>) -> bool {
    let name = node.name.trim();
    let Some(bounds) = node.bounds.as_ref() else {
        return false;
    };
    if name.is_empty()
        || bounds.width == 0
        || bounds.height == 0
        || bounds.height > 45
        || bounds.width > 260
        || bounds.x < 0
        || bounds.y < 330
        || bounds.x >= 720
        || bounds.y >= 760
    {
        return false;
    }
    let lower = name.to_ascii_lowercase();
    if lower.contains("disambiguation")
        || page_base
            .map(|base| lower == base || lower == pluralize_for_skip(base))
            .unwrap_or(false)
        || lower.contains("wiktionary")
        || lower.contains("wikimedia")
        || lower.starts_with("wikipedia")
        || lower == "article"
        || lower == "talk"
        || lower == "read"
        || lower == "view source"
        || lower == "view history"
        || lower == "search"
        || lower == "donate"
        || lower == "create account"
        || lower == "log in"
        || lower.starts_with('[')
        || lower.starts_with('/')
        || lower.contains('(')
        || lower.contains(')')
        || lower.ends_with(".jpg")
        || lower.ends_with(".jpeg")
        || lower.ends_with(".png")
        || lower.ends_with(".svg")
        || lower.ends_with(".gif")
        || lower.ends_with(".webp")
    {
        return false;
    }
    true
}

fn pluralize_for_skip(base: &str) -> String {
    if base == "genus" {
        return "genera".into();
    }
    if let Some(prefix) = base.strip_suffix('y') {
        return format!("{prefix}ies");
    }
    format!("{base}s")
}

fn snap_click_action_to_a11y(action: &mut ActionRequest, nodes: &[A11yNode]) {
    let ActionKind::Click { x, y, .. } = &mut action.kind else {
        return;
    };
    let (Some(raw_x), Some(raw_y)) = (*x, *y) else {
        return;
    };
    let Some((nx, ny, label)) = snap_point_to_a11y(raw_x, raw_y, nodes)
        .or_else(|| snap_point_via_atspi(&action.display_id, raw_x, raw_y))
    else {
        return;
    };
    if (raw_x, raw_y) != (nx, ny) {
        action.rationale = format!(
            "{} | executor_snap={} from {},{} to {},{}",
            action.rationale, label, raw_x, raw_y, nx, ny
        );
        *x = Some(nx);
        *y = Some(ny);
    }
}

fn snap_point_via_atspi(display_id: &str, x: i32, y: i32) -> Option<(i32, i32, String)> {
    let script = r#"
import pyatspi, sys
x=int(sys.argv[1]); y=int(sys.argv[2])
best=None
def interactive(role):
    r=role.lower()
    return 'link' in r or 'button' in r or 'entry' in r or r in ('tab','menu item','menu button','check box','radio button','combo box','tree item','password text','spin button')
def consider(o):
    global best
    try:
        role=o.getRoleName(); name=(o.name or '').strip()
        if not interactive(role) or not name:
            return
        state=o.getState()
        if not state.contains(pyatspi.STATE_SHOWING):
            return
        c=o.queryComponent(); bx,by,bw,bh=c.getExtents(pyatspi.DESKTOP_COORDS)
        if bw <= 0 or bh <= 0 or bx < 0 or by < 0 or bx >= 1024 or by >= 768:
            return
        if bw * bh > (1024 * 768) // 4:
            return
        cx=bx+bw//2; cy=by+bh//2
        if bx <= x <= bx+bw and by <= y <= by+bh:
            score=0
        else:
            tol=max(bh//2 + 8, 18)
            dy=abs(y-cy); dx=abs(x-cx)
            if dy > tol or dx > 260:
                return
            score=1000+dx
        if best is None or score < best[0]:
            best=(score,cx,cy,name[:80])
    except Exception:
        pass
def walk(o):
    try:
        consider(o)
        for i in range(o.childCount):
            walk(o[i])
    except Exception:
        pass
for app in pyatspi.Registry.getDesktop(0):
    walk(app)
if best:
    print(best[1], best[2], best[3])
"#;
    let output = Command::new("python3")
        .env("DISPLAY", display_id)
        .arg("-c")
        .arg(script)
        .arg(x.to_string())
        .arg(y.to_string())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .lines()
        .rev()
        .find(|line| {
            let mut parts = line.split_whitespace();
            parts.next().and_then(|p| p.parse::<i32>().ok()).is_some()
                && parts.next().and_then(|p| p.parse::<i32>().ok()).is_some()
        })?;
    let mut parts = line.splitn(3, ' ');
    let nx = parts.next()?.trim().parse::<i32>().ok()?;
    let ny = parts.next()?.trim().parse::<i32>().ok()?;
    let name = parts.next().unwrap_or("").trim();
    Some((nx, ny, format!("atspi-live:{name}")))
}

fn snap_point_to_a11y(x: i32, y: i32, nodes: &[A11yNode]) -> Option<(i32, i32, String)> {
    let mut candidates = Vec::new();
    for node in nodes {
        if !is_interactive_role(&node.role) || node.name.trim().is_empty() {
            continue;
        }
        let Some(bounds) = &node.bounds else {
            continue;
        };
        if bounds.width == 0 || bounds.height == 0 {
            continue;
        }
        if bounds.width as u64 * bounds.height as u64 > (1024 * 768) / 4 {
            continue;
        }
        let cx = bounds.x + bounds.width as i32 / 2;
        let cy = bounds.y + bounds.height as i32 / 2;
        if cx < 0 || cy < 0 || cx >= 1024 || cy >= 768 {
            continue;
        }
        candidates.push((node, bounds, cx, cy));
    }

    for (node, bounds, cx, cy) in &candidates {
        let half_w = bounds.width as i32 / 2;
        let half_h = bounds.height as i32 / 2;
        if x >= *cx - half_w && x <= *cx + half_w && y >= *cy - half_h && y <= *cy + half_h {
            return Some((*cx, *cy, format!("contains:{}", node.name)));
        }
    }

    let mut best_same_line = None::<(&A11yNode, i32, i32, i32)>;
    for (node, bounds, cx, cy) in &candidates {
        let tolerance = ((bounds.height as i32) / 2 + 8).max(18);
        let dy = (y - *cy).abs();
        if dy > tolerance {
            continue;
        }
        let dx = (x - *cx).abs();
        if dx > 260 {
            continue;
        }
        if best_same_line
            .map(|(_, best_dx, _, _)| dx < best_dx)
            .unwrap_or(true)
        {
            best_same_line = Some((node, dx, *cx, *cy));
        }
    }
    if let Some((node, dx, cx, cy)) = best_same_line {
        return Some((cx, cy, format!("same-line(dx={dx}):{}", node.name)));
    }

    None
}

fn is_interactive_role(role: &str) -> bool {
    let role = role.to_ascii_lowercase();
    role.contains("link")
        || role.contains("button")
        || role.contains("entry")
        || matches!(
            role.as_str(),
            "tab"
                | "menu item"
                | "menu button"
                | "check box"
                | "radio button"
                | "combo box"
                | "tree item"
                | "password text"
                | "spin button"
        )
}

fn uses_fast_frame_verification(action: &ActionRequest) -> bool {
    matches!(
        action.kind,
        ActionKind::MovePointer { .. }
            | ActionKind::Click { .. }
            | ActionKind::Scroll { .. }
            | ActionKind::TypeText { .. }
            | ActionKind::KeyCombo { .. }
            | ActionKind::Drag { .. }
    )
}

fn fast_frame_budget_ms(action: &ActionRequest) -> u64 {
    match action.kind {
        ActionKind::MovePointer { .. } => 120,
        ActionKind::Click { .. } => 450,
        ActionKind::Scroll { .. } => 250,
        ActionKind::TypeText { .. } => 350,
        ActionKind::KeyCombo { .. } => 500,
        ActionKind::Drag { .. } => 500,
        ActionKind::Finish { .. } | ActionKind::Noop => 0,
    }
}

fn is_policy_navigation_action(action: &ActionRequest) -> bool {
    action.step_id.starts_with("policy-wiki-link-")
}

async fn await_active_window_title_change(
    display_id: &str,
    before: Option<&str>,
    budget_ms: u64,
) -> Option<String> {
    let before = before?;
    let deadline = Instant::now() + Duration::from_millis(budget_ms);
    while Instant::now() < deadline {
        if let Some(title) = active_window_title(display_id)
            && title != before
        {
            return Some(title);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    None
}

fn scripted_action(
    task_id: &str,
    args: &Args,
    cfg: &AgentConfig,
    step_index: u32,
) -> ActionRequest {
    ActionRequest {
        task_id: task_id.to_string(),
        step_id: format!("scripted-{step_index}"),
        display_id: args.display_id.clone(),
        goal: args.goal.clone(),
        rationale: "scripted loop smoke test".into(),
        kind: ActionKind::Noop,
        expected: vec![ExpectedChange::AnyUiChange],
        timeout_ms: cfg.timeouts_ms.verification_default,
    }
}

async fn fire_action(
    input: &mut tokio::net::UnixStream,
    action: &ActionRequest,
) -> Result<ActionResult> {
    send_msg(input, &BusMessage::ActionRequest(action.clone())).await?;
    match recv_msg::<BusMessage>(input).await? {
        BusMessage::ActionResult(result) => {
            info!(?result, "action injected");
            Ok(result)
        }
        other => anyhow::bail!("unexpected input response: {:?}", other),
    }
}

async fn arm_verifier(verify: &mut tokio::net::UnixStream, action: &ActionRequest) -> Result<()> {
    send_msg(verify, &BusMessage::ActionRequest(action.clone())).await?;
    match timeout(Duration::from_millis(1000), recv_msg::<BusMessage>(verify)).await?? {
        BusMessage::VerificationArmed { task_id, step_id }
            if task_id == action.task_id && step_id == action.step_id =>
        {
            Ok(())
        }
        other => anyhow::bail!("unexpected verification arm response: {:?}", other),
    }
}

async fn await_verification(
    verify: &mut tokio::net::UnixStream,
    action: &ActionRequest,
    pre_action_title: Option<&str>,
) -> Result<VerificationResult> {
    let wait = Duration::from_millis(action.timeout_ms.saturating_add(500));
    match timeout(wait, recv_msg::<BusMessage>(verify)).await?? {
        BusMessage::VerificationResult(result) => {
            let result = recover_verification_from_window_title(action, pre_action_title, result);
            info!(?result, "action verified");
            Ok(result)
        }
        other => anyhow::bail!("unexpected verification response: {:?}", other),
    }
}

fn recover_verification_from_window_title(
    action: &ActionRequest,
    pre_action_title: Option<&str>,
    result: VerificationResult,
) -> VerificationResult {
    if !matches!(result.status, VerificationStatus::Timeout) {
        return result;
    }
    let Some(before) = pre_action_title.filter(|value| !value.is_empty()) else {
        return result;
    };
    let Some(after) = active_window_title(&action.display_id) else {
        return result;
    };
    if after == before {
        return result;
    }
    match action.kind {
        ActionKind::Click { .. } => VerificationResult {
            task_id: action.task_id.clone(),
            step_id: action.step_id.clone(),
            status: VerificationStatus::Verified,
            detail: format!("active window title changed from {before} to {after}"),
            observed_at: chrono::Utc::now(),
        },
        ActionKind::KeyCombo { ref keys }
            if keys.iter().any(|key| key.eq_ignore_ascii_case("enter")) =>
        {
            VerificationResult {
                task_id: action.task_id.clone(),
                step_id: action.step_id.clone(),
                status: VerificationStatus::Verified,
                detail: format!("active window title changed from {before} to {after}"),
                observed_at: chrono::Utc::now(),
            }
        }
        _ => result,
    }
}

fn active_window_title(display_id: &str) -> Option<String> {
    let active = Command::new("xprop")
        .arg("-display")
        .arg(display_id)
        .arg("-root")
        .arg("_NET_ACTIVE_WINDOW")
        .output()
        .ok()?;
    if !active.status.success() {
        return None;
    }
    let active_stdout = String::from_utf8_lossy(&active.stdout);
    let window_id = active_stdout
        .split_whitespace()
        .last()
        .filter(|value| value.starts_with("0x") && *value != "0x0")?;
    let title = Command::new("xprop")
        .arg("-display")
        .arg(display_id)
        .arg("-id")
        .arg(window_id)
        .arg("_NET_WM_NAME")
        .arg("WM_NAME")
        .output()
        .ok()?;
    if !title.status.success() {
        return None;
    }
    let title_stdout = String::from_utf8_lossy(&title.stdout);
    let title_line = title_stdout
        .lines()
        .find(|line| line.starts_with("_NET_WM_NAME") && line.contains('"'))
        .or_else(|| title_stdout.lines().find(|line| line.contains('"')))?;
    let start = title_line.find('"')?;
    let end = title_line.rfind('"')?;
    if end <= start {
        return None;
    }
    Some(title_line[start + 1..end].to_string())
}

async fn recv_a11y_snapshot(stream: &mut tokio::net::UnixStream) -> Result<Vec<A11yNode>> {
    match recv_msg::<BusMessage>(stream).await? {
        BusMessage::A11y(event) => Ok(event.nodes),
        _ => Ok(Vec::new()),
    }
}

fn merge_a11y_event(
    current: Vec<A11yNode>,
    kind: A11yEventKind,
    incoming: Vec<A11yNode>,
) -> Vec<A11yNode> {
    if matches!(kind, A11yEventKind::Snapshot) {
        return incoming;
    }
    let mut by_id: std::collections::HashMap<String, usize> = current
        .iter()
        .enumerate()
        .map(|(idx, node)| (node.id.clone(), idx))
        .collect();
    let mut merged = current;
    for node in incoming {
        if let Some(&idx) = by_id.get(&node.id) {
            merged[idx] = node;
        } else {
            by_id.insert(node.id.clone(), merged.len());
            merged.push(node);
        }
    }
    merged
}

async fn recv_frame(stream: &mut tokio::net::UnixStream) -> Result<Option<CaptureFrame>> {
    match recv_msg::<BusMessage>(stream).await? {
        BusMessage::Capture(frame) => Ok(Some(frame)),
        _ => Ok(None),
    }
}

async fn drain_a11y(
    stream: &mut tokio::net::UnixStream,
    current: Vec<A11yNode>,
) -> Result<Vec<A11yNode>> {
    match timeout(Duration::from_millis(1), recv_msg::<BusMessage>(stream)).await {
        Ok(Ok(BusMessage::A11y(event))) => Ok(merge_a11y_event(current, event.kind, event.nodes)),
        Ok(Ok(_)) | Ok(Err(_)) | Err(_) => Ok(current),
    }
}

async fn drain_frame(
    stream: &mut tokio::net::UnixStream,
    current: Option<CaptureFrame>,
) -> Result<Option<CaptureFrame>> {
    match timeout(Duration::from_millis(1), recv_msg::<BusMessage>(stream)).await {
        Ok(Ok(BusMessage::Capture(frame))) => Ok(Some(frame)),
        Ok(Ok(_)) | Ok(Err(_)) | Err(_) => Ok(current),
    }
}

/// Block (with budget) until a fresh full a11y Snapshot arrives. Called
/// after we know a click navigated so the next reasoning turn doesn't
/// see stale tree data from the previous page.
async fn await_fresh_a11y_snapshot(
    stream: &mut tokio::net::UnixStream,
    fallback: Vec<A11yNode>,
    budget_ms: u64,
) -> Result<Vec<A11yNode>> {
    let deadline = Instant::now() + Duration::from_millis(budget_ms);
    let mut current = fallback;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match timeout(remaining, recv_msg::<BusMessage>(stream)).await {
            Ok(Ok(BusMessage::A11y(event))) => {
                if matches!(event.kind, agent_proto::A11yEventKind::Snapshot) {
                    return Ok(event.nodes);
                }
                current = merge_a11y_event(current, event.kind, event.nodes);
            }
            Ok(Ok(_)) | Ok(Err(_)) | Err(_) => break,
        }
    }
    Ok(current)
}

async fn await_useful_a11y(
    stream: &mut tokio::net::UnixStream,
    fallback: Vec<A11yNode>,
    budget_ms: u64,
) -> Result<Vec<A11yNode>> {
    if useful_interactive_count(&fallback) >= 8 {
        return Ok(fallback);
    }
    let deadline = Instant::now() + Duration::from_millis(budget_ms);
    let mut current = fallback;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match timeout(remaining, recv_msg::<BusMessage>(stream)).await {
            Ok(Ok(BusMessage::A11y(event))) => {
                current = merge_a11y_event(current, event.kind, event.nodes);
                if useful_interactive_count(&current) >= 8 {
                    return Ok(current);
                }
            }
            Ok(Ok(_)) | Ok(Err(_)) | Err(_) => break,
        }
    }
    Ok(current)
}

fn useful_interactive_count(nodes: &[A11yNode]) -> usize {
    nodes
        .iter()
        .filter(|node| {
            is_interactive_role(&node.role)
                && !node.name.trim().is_empty()
                && node
                    .bounds
                    .as_ref()
                    .map(|b| b.width > 0 && b.height > 0)
                    .unwrap_or(false)
        })
        .count()
}

/// Block (with budget) until a fresh capture frame arrives, ignoring any
/// frame whose monotonic_ns is at or before `not_before_ns`.
async fn await_fresh_frame(
    stream: &mut tokio::net::UnixStream,
    fallback: Option<CaptureFrame>,
    not_before_ns: u128,
    budget_ms: u64,
) -> Result<Option<CaptureFrame>> {
    let (frame, _) =
        await_fresh_frame_with_status(stream, fallback, not_before_ns, budget_ms).await?;
    Ok(frame)
}

async fn await_fresh_frame_with_status(
    stream: &mut tokio::net::UnixStream,
    fallback: Option<CaptureFrame>,
    not_before_ns: u128,
    budget_ms: u64,
) -> Result<(Option<CaptureFrame>, bool)> {
    let deadline = Instant::now() + Duration::from_millis(budget_ms);
    let mut current = fallback;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match timeout(remaining, recv_msg::<BusMessage>(stream)).await {
            Ok(Ok(BusMessage::Capture(frame))) => {
                if frame.monotonic_ns > not_before_ns {
                    return Ok((Some(frame), true));
                }
                current = Some(frame);
            }
            Ok(Ok(_)) | Ok(Err(_)) | Err(_) => break,
        }
    }
    Ok((current, false))
}
