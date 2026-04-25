use agent_common::{AgentConfig, bind_socket, init_tracing, load_dotenv, recv_msg, send_msg};
use agent_proto::{
    ActionKind, ActionRequest, BusMessage, ExpectedChange, FrameEncoding, ModelActionEnvelope,
    ReasoningTurn,
};
use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use bytes::Bytes;
use clap::Parser;
use futures_util::StreamExt;
use reqwest::{Client, Response, StatusCode};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant, sleep};
use tracing::info;

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "config/agent.example.toml")]
    config: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    load_dotenv();
    init_tracing();
    let args = Args::parse();
    let cfg = AgentConfig::load(&args.config)?;
    let listener = bind_socket(&cfg.sockets.action).await?;
    info!("reasoning daemon listening on {}", cfg.sockets.action);

    let last_request_at: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    loop {
        let (mut stream, _) = listener.accept().await?;
        let cfg = cfg.clone();
        let last_request_at = last_request_at.clone();
        tokio::spawn(async move {
            let client = Client::new();
            loop {
                let msg = match recv_msg::<BusMessage>(&mut stream).await {
                    Ok(msg) => msg,
                    Err(err) => {
                        tracing::debug!(?err, "reasoning stream closed");
                        return;
                    }
                };
                if let BusMessage::ReasoningTurn(turn) = msg {
                    enforce_request_pacing(&cfg, &last_request_at).await;
                    match infer_action(&client, &cfg, turn.clone()).await {
                        Ok(model_action) => {
                            if let Err(err) =
                                send_msg(&mut stream, &BusMessage::ModelAction(model_action)).await
                            {
                                tracing::error!(?err, "failed sending model action");
                                return;
                            }
                        }
                        Err(err) => {
                            tracing::error!(?err, "reasoning failed");
                            let model_action =
                                failure_action(turn, format!("reasoning failed: {err:#}"));
                            if let Err(send_err) =
                                send_msg(&mut stream, &BusMessage::ModelAction(model_action)).await
                            {
                                tracing::error!(?send_err, "failed sending reasoning failure");
                                return;
                            }
                        }
                    }
                }
            }
        });
    }
}

async fn enforce_request_pacing(cfg: &AgentConfig, last_request_at: &Arc<Mutex<Option<Instant>>>) {
    let interval_ms = cfg.reasoning.min_request_interval_ms;
    if interval_ms == 0 {
        return;
    }
    let interval = Duration::from_millis(interval_ms);
    let mut guard = last_request_at.lock().await;
    if let Some(prev) = *guard {
        let elapsed = prev.elapsed();
        if elapsed < interval {
            let wait = interval - elapsed;
            tracing::debug!(wait_ms = wait.as_millis() as u64, "pacing model request");
            sleep(wait).await;
        }
    }
    *guard = Some(Instant::now());
}

async fn infer_action(
    client: &Client,
    cfg: &AgentConfig,
    turn: ReasoningTurn,
) -> Result<ModelActionEnvelope> {
    let provider =
        std::env::var("AGENT_MODEL_PROVIDER").unwrap_or_else(|_| cfg.reasoning.provider.clone());
    let base_url =
        std::env::var("AGENT_MODEL_BASE_URL").unwrap_or_else(|_| cfg.reasoning.base_url.clone());
    let model = std::env::var("AGENT_MODEL_NAME").unwrap_or_else(|_| cfg.reasoning.model.clone());
    let api_key = match provider.as_str() {
        "groq" => {
            std::env::var("GROQ_API_KEY").context("missing GROQ_API_KEY for provider=groq")?
        }
        "xai" => std::env::var("XAI_API_KEY").context("missing XAI_API_KEY for provider=xai")?,
        "cerebras" => std::env::var("CEREBRAS_API_KEY")
            .context("missing CEREBRAS_API_KEY for provider=cerebras")?,
        "openrouter" => std::env::var("OPENROUTER_API_KEY")
            .context("missing OPENROUTER_API_KEY for provider=openrouter")?,
        other => bail!(
            "unsupported provider {other}; expected groq, xai, cerebras, or openrouter"
        ),
    };
    let user_content = build_user_content(&turn, cfg.reasoning.vision_enabled);
    let mut payload = json!({
        "model": model,
        "stream": cfg.reasoning.stream,
        "messages": [
            {"role":"system","content":"You are the action planner for a GUI agent. Return one JSON object only with keys action and reasoning_summary."},
            {"role":"user","content":user_content}
        ],
        "response_format": {"type":"json_object"}
    });
    // OpenRouter routing: when the user has pinned a specific upstream
    // provider (e.g. AGENT_PROVIDER_ORDER="google-ai-studio"), force it.
    // This avoids OpenRouter's auto-routing landing on a slower or less
    // reliable backend for the same model id.
    if provider == "openrouter" {
        if let Ok(order) = std::env::var("AGENT_PROVIDER_ORDER") {
            let providers: Vec<String> = order
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if !providers.is_empty() {
                payload["provider"] = json!({
                    "order": providers,
                    "allow_fallbacks": false,
                });
            }
        }
    }

    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let resp = send_model_request_with_retries(client, &url, &api_key, &payload).await?;

    let raw_response = if cfg.reasoning.stream {
        consume_streaming_response(resp).await?
    } else {
        resp.json::<Value>().await?
    };

    let mut action = parse_action(&turn, &raw_response)?;
    // Snap Click coordinates to the actual visible interactive element the
    // model meant to hit. We extract an optional `target` field the model
    // can emit instead of (or alongside) raw coords; otherwise we snap by
    // bounding-box-contains, then by nearest-within-30px. This eliminates
    // the +/- pixel drift between a11y bounds and clickable hitboxes.
    if matches!(action.kind, ActionKind::Click { .. }) {
        let visible = collect_visible_interactive(&turn);
        let target_name = raw_response
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .and_then(|c| parse_json_content(c).ok())
            .and_then(|v| {
                v.pointer("/action/target")
                    .or_else(|| v.pointer("/action/Click/target"))
                    .or_else(|| v.pointer("/action/name"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
        if let ActionKind::Click {
            ref mut x, ref mut y, ..
        } = action.kind
        {
            if let Some((nx, ny, src)) =
                snap_click_to_visible(&visible, target_name.as_deref(), *x, *y)
            {
                if (*x, *y) != (Some(nx), Some(ny)) {
                    tracing::info!(
                        from_x = ?*x,
                        from_y = ?*y,
                        to_x = nx,
                        to_y = ny,
                        source = %src,
                        "snapped click to visible interactive element"
                    );
                }
                *x = Some(nx);
                *y = Some(ny);
                action.rationale = format!("{} | snap={}", action.rationale, src);
            } else {
                tracing::warn!(
                    raw_x = ?*x,
                    raw_y = ?*y,
                    target = ?target_name,
                    "click did not snap to any visible interactive element"
                );
            }
        }
    }
    let action_kind_label = match &action.kind {
        ActionKind::Click { x, y, .. } => format!("Click(x={x:?},y={y:?})"),
        ActionKind::TypeText { text } => {
            format!("TypeText({:?})", text.chars().take(40).collect::<String>())
        }
        ActionKind::KeyCombo { keys } => format!("KeyCombo({:?})", keys),
        ActionKind::MovePointer { x, y, .. } => format!("MovePointer({x},{y})"),
        ActionKind::Scroll { dx, dy } => format!("Scroll({dx},{dy})"),
        ActionKind::Drag { .. } => "Drag".into(),
        ActionKind::Finish { success, .. } => format!("Finish(success={success})"),
        ActionKind::Noop => "Noop".into(),
    };
    let raw_content_preview = raw_response
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .map(|s| s.chars().take(200).collect::<String>())
        .unwrap_or_default();
    tracing::info!(
        action_kind = %action_kind_label,
        rationale = %action.rationale,
        raw_preview = %raw_content_preview,
        "model produced action"
    );
    let reasoning_summary = raw_response
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .unwrap_or("streamed tool dispatch")
        .to_string();

    Ok(ModelActionEnvelope {
        action,
        reasoning_summary,
        raw_response,
    })
}

async fn send_model_request_with_retries(
    client: &Client,
    url: &str,
    api_key: &str,
    payload: &Value,
) -> Result<Response> {
    let mut backoff_ms = 1000_u64;
    for attempt in 1..=6 {
        let resp = client
            .post(url)
            .bearer_auth(api_key)
            .json(payload)
            .send()
            .await?;
        if resp.status() != StatusCode::TOO_MANY_REQUESTS {
            return Ok(resp.error_for_status()?);
        }

        let wait_ms = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .map(|seconds| seconds.saturating_mul(1000))
            .unwrap_or(backoff_ms)
            .clamp(1000, 30_000);
        tracing::warn!(attempt, wait_ms, "model provider rate limited request");
        if attempt == 6 {
            let body = resp.text().await.unwrap_or_default();
            bail!("model provider returned 429 Too Many Requests after retries: {body}");
        }
        sleep(Duration::from_millis(wait_ms)).await;
        backoff_ms = (backoff_ms * 2).min(30_000);
    }
    bail!("model request retry loop exhausted")
}

fn failure_action(turn: ReasoningTurn, summary: String) -> ModelActionEnvelope {
    // A failed model call is not a GUI action. The supervisor treats this
    // explicit Noop as a failed step, then retries from a fresh observation.
    ModelActionEnvelope {
        action: ActionRequest {
            task_id: turn.task_id,
            step_id: format!("step-{}", chrono::Utc::now().timestamp_millis()),
            display_id: turn.display_id,
            goal: turn.goal,
            rationale: format!("reasoning failed; no GUI action was taken: {summary}"),
            kind: ActionKind::Noop,
            expected: vec![],
            timeout_ms: 500,
        },
        reasoning_summary: summary,
        raw_response: json!({"error":"reasoning_failed"}),
    }
}

fn build_user_content(turn: &ReasoningTurn, vision_enabled: bool) -> Value {
    let prompt = build_prompt(turn);
    if !vision_enabled {
        return json!(prompt);
    }
    let Some(frame) = &turn.frame else {
        return json!(prompt);
    };
    if !matches!(frame.encoding, FrameEncoding::Jpeg) || frame.bytes.is_empty() {
        return json!(prompt);
    }
    let data_url = format!("data:image/jpeg;base64,{}", BASE64.encode(&frame.bytes));
    json!([
        {"type":"text","text":prompt},
        {"type":"image_url","image_url":{"url":data_url}}
    ])
}

fn build_prompt(turn: &ReasoningTurn) -> String {
    const MAX_INTERACTIVE_NODES: usize = 120;
    const MAX_HISTORY_ENTRIES: usize = 12;
    let viewport_w: i32 = std::env::var("AGENT_SCREEN_WIDTH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1024);
    let viewport_h: i32 = std::env::var("AGENT_SCREEN_HEIGHT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(768);
    let mut s = String::new();
    s.push_str(&format!(
        "Goal: {}\nDisplay: {}\n",
        turn.goal, turn.display_id
    ));
    if let Some(title) = turn.active_window_title.as_deref() {
        s.push_str(&format!(
            "Current page (authoritative — trust this over your past rationales): {title}\n"
        ));
    }
    let total_nodes = turn.a11y_snapshot.len();
    let mut role_hist: std::collections::BTreeMap<&str, usize> =
        std::collections::BTreeMap::new();
    let mut nodes_with_bounds = 0;
    let mut nodes_with_name = 0;
    for n in &turn.a11y_snapshot {
        *role_hist.entry(n.role.as_str()).or_insert(0) += 1;
        if n.bounds.is_some() {
            nodes_with_bounds += 1;
        }
        if !n.name.trim().is_empty() {
            nodes_with_name += 1;
        }
    }
    let mut top_roles: Vec<_> = role_hist.iter().collect();
    top_roles.sort_by_key(|(_, c)| std::cmp::Reverse(**c));
    let role_summary: String = top_roles
        .iter()
        .take(8)
        .map(|(r, c)| format!("{r}={c}"))
        .collect::<Vec<_>>()
        .join(",");
    tracing::info!(
        total_nodes,
        nodes_with_bounds,
        nodes_with_name,
        role_summary = %role_summary,
        "a11y snapshot stats"
    );
    s.push_str(&format!(
        "Viewport: {viewport_w}x{viewport_h} (only links inside this viewport are clickable)\n"
    ));
    s.push_str("Interactive elements (role | name | center_x,center_y):\n");
    let mut shown = 0;
    let mut filtered_offscreen = 0;
    for node in &turn.a11y_snapshot {
        if shown >= MAX_INTERACTIVE_NODES {
            break;
        }
        if !is_interactive_role(&node.role) {
            continue;
        }
        let name = node.name.trim();
        if name.is_empty() {
            continue;
        }
        let Some(bounds) = &node.bounds else {
            continue;
        };
        if bounds.width == 0 || bounds.height == 0 {
            continue;
        }
        let cx = bounds.x + (bounds.width as i32) / 2;
        let cy = bounds.y + (bounds.height as i32) / 2;
        // Drop links that are not inside the visible viewport — clicking
        // them would land outside the X screen and fail.
        if cx < 0 || cy < 0 || cx >= viewport_w || cy >= viewport_h {
            filtered_offscreen += 1;
            continue;
        }
        let trimmed = name.chars().take(140).collect::<String>();
        s.push_str(&format!("- {} | {} | {},{}\n", node.role, trimmed, cx, cy));
        shown += 1;
    }
    tracing::info!(
        shown,
        filtered_offscreen,
        "interactive nodes shown to model"
    );
    s.push_str("Action history (most recent last; verified means *some* UI change, navigated means the active window title actually changed):\n");
    let history = &turn.action_history;
    let skip = history.len().saturating_sub(MAX_HISTORY_ENTRIES);
    for entry in history.iter().skip(skip) {
        let kind = action_kind_short(&entry.action.kind);
        let verified = matches!(
            entry
                .verification_result
                .as_ref()
                .map(|r| r.status.clone()),
            Some(agent_proto::VerificationStatus::Verified)
        );
        s.push_str(&format!(
            "- {} verified={} navigated={}\n",
            kind, verified, entry.caused_navigation,
        ));
    }
    s.push_str(
        "Return JSON only using serde externally-tagged enums. Unit variants are strings.\n",
    );
    s.push_str("Action kind examples: \"Noop\", {\"Click\":{\"button\":\"Left\",\"count\":1,\"x\":320,\"y\":240}}, {\"TypeText\":{\"text\":\"hello\"}}, {\"KeyCombo\":{\"keys\":[\"CTRL\",\"L\"]}}, {\"MovePointer\":{\"x\":100,\"y\":200,\"absolute\":true}}, {\"Finish\":{\"success\":true,\"summary\":\"goal achieved\"}}.\n");
    s.push_str("Expected change examples: \"AnyUiChange\", {\"FocusOnNode\":{\"node_name\":\"Search\"}}, {\"TextPresent\":{\"text\":\"hello\"}}, {\"WindowActivated\":{\"title\":\"Firefox\"}}.\n");
    s.push_str("You have a screenshot of the current display attached. Use it to see the actual rendered page and decide which on-screen element advances the goal. The Interactive elements list above mirrors visible elements with their exact pixel center coordinates; when you choose to Click an element you saw in the image, copy that element's center_x,center_y from the list verbatim. Never invent coordinates that aren't in the list.\n");
    s.push_str("Preferred Click format: include the element's exact name as `target`. The runtime will look up its real coords from the Interactive elements list and snap the click to it. Example: {\"Click\":{\"button\":\"Left\",\"count\":1,\"x\":471,\"y\":421,\"target\":\"legume\"}}. If you also pass x,y, the runtime will snap to the closest matching element in the list within 30px to absorb minor drift.\n");
    s.push_str("Plan one safe action at a time. Pick elements that are part of the main content area, not generic site chrome.\n");
    s.push_str("If your last 2-3 clicks all show navigated=false in the Action history, the current page is a dead end (no useful links). In a browser context emit KeyCombo with keys [\"alt\",\"Left\"] to go back to the previous page, then try a different link.\n");
    s.push_str("Return shape:\n");
    s.push_str(r#"{"action":{"task_id":"use-current-task-id","step_id":"step-1","display_id":":0","goal":"goal text","rationale":"why this is the next safe action","kind":"Noop","expected":["AnyUiChange"],"timeout_ms":2000},"reasoning_summary":"brief"}"#);
    s.push_str("\nUse Finish only when the visible GUI/a11y state proves the goal is complete.");
    s
}

async fn consume_streaming_response(resp: reqwest::Response) -> Result<Value> {
    let mut stream = resp.bytes_stream();
    let mut sse_buffer = String::new();
    let mut content = String::new();
    while let Some(chunk) = stream.next().await {
        let chunk: Bytes = chunk?;
        sse_buffer.push_str(&String::from_utf8_lossy(&chunk));

        while let Some(newline) = sse_buffer.find('\n') {
            let line = sse_buffer[..newline].trim_end_matches('\r').to_string();
            sse_buffer.drain(..=newline);
            let Some(rest) = line.strip_prefix("data: ") else {
                continue;
            };
            let data = rest.trim();
            if data == "[DONE]" {
                return wrap_streamed_content(content);
            }

            let event: Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if let Some(delta) = event
                .pointer("/choices/0/delta/content")
                .and_then(Value::as_str)
            {
                content.push_str(delta);
                // Early dispatch: only return once the JSON is fully closed
                // (balanced braces and parses) AND the action object has a
                // `kind` field — meaning the model has finished writing the
                // tool call. Anything earlier and we'd dispatch a partial
                // action with missing coordinates.
                if let Some(snippet) = extract_complete_json_object(&content) {
                    if let Ok(parsed) = serde_json::from_str::<Value>(snippet) {
                        if parsed
                            .get("action")
                            .and_then(Value::as_object)
                            .map(|a| a.contains_key("kind") || a.values().any(|v| v.is_object()))
                            .unwrap_or(false)
                        {
                            return wrap_streamed_content(snippet.to_string());
                        }
                    }
                }
            }
        }
    }
    wrap_streamed_content(content)
}

/// Returns the substring covering the first complete top-level JSON object
/// in `content`, ignoring braces inside strings. Returns None if the object
/// is not yet closed.
fn extract_complete_json_object(content: &str) -> Option<&str> {
    let bytes = content.as_bytes();
    let mut start = None;
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape = false;
    for (i, &b) in bytes.iter().enumerate() {
        if in_string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => {
                if depth == 0 {
                    start = Some(i);
                }
                depth += 1;
            }
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    if let Some(s) = start {
                        return Some(&content[s..=i]);
                    }
                }
            }
            _ => {}
        }
    }
    None
}

fn wrap_streamed_content(content: String) -> Result<Value> {
    if content.trim().is_empty() {
        bail!("empty streaming response");
    }
    Ok(json!({"choices":[{"message":{"content":content}}]}))
}

fn parse_action(turn: &ReasoningTurn, raw: &Value) -> Result<ActionRequest> {
    if let Some(action_value) = raw.pointer("/action") {
        return decode_action(turn, action_value.clone());
    }

    let content = raw
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .context("missing model content")?;
    let content_json = match parse_json_content(content) {
        Ok(value) => value,
        Err(parse_err) => {
            if let Some(action) = fallback_action_from_text(turn, content) {
                tracing::warn!(
                    err = ?parse_err,
                    "salvaged action from malformed model JSON"
                );
                return Ok(action);
            }
            return Err(parse_err);
        }
    };
    if let Some(action_value) = content_json.get("action") {
        return decode_action(turn, action_value.clone());
    }

    bail!("model content did not contain an action object")
}

fn fallback_action_from_text(turn: &ReasoningTurn, content: &str) -> Option<ActionRequest> {
    let lower = content.to_ascii_lowercase();
    let mut action = ActionRequest {
        task_id: turn.task_id.clone(),
        step_id: format!("step-{}", chrono::Utc::now().timestamp_millis()),
        display_id: turn.display_id.clone(),
        goal: turn.goal.clone(),
        rationale: "salvaged from malformed model JSON".into(),
        kind: ActionKind::Noop,
        expected: vec![],
        timeout_ms: 2000,
    };

    if lower.contains("\"finish\"") || lower.contains("goal achieved") {
        let success = !lower.contains("\"success\":false") && !lower.contains("\"success\": false");
        action.kind = ActionKind::Finish {
            success,
            summary: "model reported goal complete".into(),
        };
        return Some(action);
    }

    if lower.contains("\"click\"") || lower.contains("click") {
        let x = find_i32_field(content, "x");
        let y = find_i32_field(content, "y");
        if x.is_some() || y.is_some() {
            action.kind = ActionKind::Click {
                button: agent_proto::MouseButton::Left,
                count: find_i32_field(content, "count")
                    .and_then(|n| u8::try_from(n).ok())
                    .unwrap_or(1),
                x,
                y,
            };
            action.expected.push(default_expected_change());
            return Some(action);
        }
    }

    if lower.contains("\"typetext\"") || lower.contains("\"type\"") {
        if let Some(text) = find_string_field(content, "text") {
            action.kind = ActionKind::TypeText { text };
            action.expected.push(default_expected_change());
            return Some(action);
        }
    }

    if lower.contains("\"keycombo\"") || lower.contains("\"keys\"") {
        if lower.contains("enter") {
            action.kind = ActionKind::KeyCombo {
                keys: vec!["Enter".into()],
            };
            action.expected.push(default_expected_change());
            return Some(action);
        }
        if lower.contains("ctrl") && lower.contains("\"l\"") {
            action.kind = ActionKind::KeyCombo {
                keys: vec!["CTRL".into(), "L".into()],
            };
            action.expected.push(default_expected_change());
            return Some(action);
        }
        if lower.contains("alt") && lower.contains("left") {
            action.kind = ActionKind::KeyCombo {
                keys: vec!["ALT".into(), "Left".into()],
            };
            action.expected.push(default_expected_change());
            return Some(action);
        }
    }

    None
}

fn find_i32_field(content: &str, field: &str) -> Option<i32> {
    let needle = format!("\"{field}\"");
    let start = content.find(&needle)? + needle.len();
    let rest = &content[start..];
    let colon = rest.find(':')? + 1;
    let rest = rest[colon..].trim_start();
    let len = rest
        .char_indices()
        .take_while(|(_, ch)| ch.is_ascii_digit() || *ch == '-')
        .last()
        .map(|(idx, ch)| idx + ch.len_utf8())?;
    rest[..len].parse().ok()
}

fn find_string_field(content: &str, field: &str) -> Option<String> {
    let needle = format!("\"{field}\"");
    let start = content.find(&needle)? + needle.len();
    let rest = &content[start..];
    let colon = rest.find(':')? + 1;
    let rest = rest[colon..].trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn decode_action(turn: &ReasoningTurn, value: Value) -> Result<ActionRequest> {
    let normalized = normalize_action_value(turn, value);
    let mut action: ActionRequest = serde_json::from_value(normalized)?;
    action.task_id = turn.task_id.clone();
    action.display_id = turn.display_id.clone();
    action.goal = turn.goal.clone();
    if action.step_id.trim().is_empty() {
        action.step_id = format!("step-{}", chrono::Utc::now().timestamp_millis());
    }
    if action.timeout_ms == 0 {
        action.timeout_ms = 2000;
    }
    if !matches!(action.kind, ActionKind::Finish { .. } | ActionKind::Noop)
        && action.expected.is_empty()
    {
        action.expected.push(default_expected_change());
    }
    Ok(action)
}

fn normalize_action_value(turn: &ReasoningTurn, value: Value) -> Value {
    if let Some(kind) = value.as_str() {
        return json!({
            "task_id": turn.task_id,
            "step_id": format!("step-{}", chrono::Utc::now().timestamp_millis()),
            "display_id": turn.display_id,
            "goal": turn.goal,
            "rationale": "model returned shorthand action",
            "kind": normalize_kind(Value::String(kind.to_string()), true, "goal complete"),
            "expected": ["AnyUiChange"],
            "timeout_ms": 2000
        });
    }

    let mut obj = match value {
        Value::Object(obj) => obj,
        other => {
            return json!({
                "task_id": turn.task_id,
                "step_id": format!("step-{}", chrono::Utc::now().timestamp_millis()),
                "display_id": turn.display_id,
                "goal": turn.goal,
                "rationale": format!("unsupported action shape: {other:?}"),
                "kind": "Noop",
                "expected": [],
                "timeout_ms": 500
            });
        }
    };

    let finish_success = obj.get("success").and_then(Value::as_bool).unwrap_or(true);
    let finish_summary = obj
        .get("summary")
        .and_then(Value::as_str)
        .unwrap_or("model reported goal complete")
        .to_string();

    obj.insert("task_id".into(), Value::String(turn.task_id.clone()));
    obj.insert("display_id".into(), Value::String(turn.display_id.clone()));
    obj.insert("goal".into(), Value::String(turn.goal.clone()));
    obj.entry("step_id").or_insert_with(|| {
        Value::String(format!("step-{}", chrono::Utc::now().timestamp_millis()))
    });
    obj.entry("rationale")
        .or_insert_with(|| Value::String("model selected next action".into()));
    obj.entry("timeout_ms").or_insert_with(|| json!(2000));

    let kind = obj.remove("kind").unwrap_or(Value::String("Noop".into()));
    let normalized_kind = normalize_kind_with_fields(kind, &obj, finish_success, &finish_summary);
    obj.insert("kind".into(), normalized_kind);

    let expected = obj
        .remove("expected")
        .unwrap_or_else(|| json!([]));
    obj.insert("expected".into(), normalize_expected(expected));

    Value::Object(obj)
}

fn normalize_kind_with_fields(
    kind: Value,
    fields: &serde_json::Map<String, Value>,
    finish_success: bool,
    finish_summary: &str,
) -> Value {
    let Value::String(name) = &kind else {
        return normalize_kind(kind, finish_success, finish_summary);
    };
    match canonical_action_variant(name).as_deref() {
        Some("Click") => json!({
            "Click": {
                "button": fields
                    .get("button")
                    .and_then(Value::as_str)
                    .map(canonical_button)
                    .unwrap_or_else(|| "Left".into()),
                "count": fields.get("count").and_then(Value::as_u64).unwrap_or(1),
                "x": numeric_field_to_i32_value(fields.get("x")),
                "y": numeric_field_to_i32_value(fields.get("y"))
            }
        }),
        Some("TypeText") => json!({
            "TypeText": {
                "text": fields.get("text").and_then(Value::as_str).unwrap_or_default()
            }
        }),
        Some("KeyCombo") => json!({
            "KeyCombo": {
                "keys": fields.get("keys").cloned().unwrap_or_else(|| json!([]))
            }
        }),
        Some("MovePointer") => json!({
            "MovePointer": {
                "x": numeric_field_to_i32(fields.get("x")).unwrap_or(0),
                "y": numeric_field_to_i32(fields.get("y")).unwrap_or(0),
                "absolute": fields.get("absolute").and_then(Value::as_bool).unwrap_or(true)
            }
        }),
        Some("Scroll") => json!({
            "Scroll": {
                "dx": numeric_field_to_i32(fields.get("dx")).unwrap_or(0),
                "dy": numeric_field_to_i32(fields.get("dy")).unwrap_or(0)
            }
        }),
        Some("Drag") => json!({
            "Drag": {
                "from": fields.get("from").cloned().unwrap_or_else(|| json!([0, 0])),
                "to": fields.get("to").cloned().unwrap_or_else(|| json!([0, 0]))
            }
        }),
        _ => normalize_kind(kind, finish_success, finish_summary),
    }
}

fn normalize_kind(kind: Value, finish_success: bool, finish_summary: &str) -> Value {
    match kind {
        Value::String(name) => match canonical_action_variant(&name).as_deref() {
            Some("Finish") => {
                json!({"Finish": {"success": finish_success, "summary": finish_summary}})
            }
            Some("Noop") => json!("Noop"),
            Some(other) => Value::String(other.to_string()),
            None => Value::String(name),
        },
        Value::Object(obj) if obj.len() == 1 => {
            let (key, mut inner) = obj.into_iter().next().expect("len checked");
            let variant = canonical_action_variant(&key).unwrap_or(key);
            if variant == "Noop" {
                return json!("Noop");
            }
            if variant == "Finish" && inner.is_null() {
                return json!({"Finish": {"success": finish_success, "summary": finish_summary}});
            }
            if variant == "Click"
                && let Value::Object(params) = &mut inner
            {
                if let Some(button) = params.get_mut("button")
                    && let Some(button_name) = button.as_str()
                {
                    *button = Value::String(canonical_button(button_name));
                }
                params.entry("count").or_insert_with(|| json!(1));
                params.entry("x").or_insert(Value::Null);
                params.entry("y").or_insert(Value::Null);
            }
            json!({variant: inner})
        }
        other => other,
    }
}

fn numeric_field_to_i32(value: Option<&Value>) -> Option<i32> {
    value.and_then(|value| match value {
        Value::Number(number) => {
            if let Some(integer) = number.as_i64() {
                Some(integer as i32)
            } else {
                number.as_f64().map(|float| float.round() as i32)
            }
        }
        _ => None,
    })
}

fn numeric_field_to_i32_value(value: Option<&Value>) -> Value {
    numeric_field_to_i32(value).map_or(Value::Null, |value| json!(value))
}

fn normalize_expected(expected: Value) -> Value {
    let items = match expected {
        Value::Array(items) => items,
        item => vec![item],
    };
    Value::Array(items.into_iter().map(normalize_expected_one).collect())
}

fn normalize_expected_one(expected: Value) -> Value {
    match expected {
        Value::String(name) => match canonical_expected_variant(&name).as_deref() {
            Some("AnyUiChange") => json!("AnyUiChange"),
            Some("FocusOnNode") => json!({"FocusOnNode": {"node_name": ""}}),
            Some("TextPresent") => json!({"TextPresent": {"text": ""}}),
            Some("WindowActivated") => json!({"WindowActivated": {"title": ""}}),
            Some("PixelChanged") => json!({"PixelChanged": {"region": default_region_value()}}),
            Some(other) => Value::String(other.to_string()),
            None => Value::String(name),
        },
        Value::Object(obj) if obj.len() == 1 => {
            let (key, inner) = obj.into_iter().next().expect("len checked");
            let variant = canonical_expected_variant(&key).unwrap_or_else(|| "AnyUiChange".into());
            if variant == "AnyUiChange" || inner.is_null() {
                json!("AnyUiChange")
            } else {
                json!({variant: inner})
            }
        }
        other => other,
    }
}

fn default_expected_change() -> ExpectedChange {
    ExpectedChange::PixelChanged {
        region: agent_proto::Rect {
            x: 0,
            y: 0,
            width: std::env::var("AGENT_SCREEN_WIDTH")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1024),
            height: std::env::var("AGENT_SCREEN_HEIGHT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(768),
        },
    }
}

fn default_region_value() -> Value {
    json!({
        "x": 0,
        "y": 0,
        "width": std::env::var("AGENT_SCREEN_WIDTH")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(1024),
        "height": std::env::var("AGENT_SCREEN_HEIGHT")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(768)
    })
}

fn canonical_action_variant(name: &str) -> Option<String> {
    let normalized = name
        .chars()
        .filter(|ch| *ch != '_' && *ch != '-' && !ch.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    match normalized.as_str() {
        "movepointer" | "move" => Some("MovePointer".into()),
        "click" => Some("Click".into()),
        "scroll" => Some("Scroll".into()),
        "typetext" | "type" => Some("TypeText".into()),
        "keycombo" | "keys" => Some("KeyCombo".into()),
        "drag" => Some("Drag".into()),
        "finish" | "done" | "complete" => Some("Finish".into()),
        "noop" | "nooperation" | "wait" => Some("Noop".into()),
        _ => None,
    }
}

fn canonical_expected_variant(name: &str) -> Option<String> {
    let normalized = name
        .chars()
        .filter(|ch| *ch != '_' && *ch != '-' && !ch.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    match normalized.as_str() {
        "focusonnode" | "focus" => Some("FocusOnNode".into()),
        "textpresent" | "text" => Some("TextPresent".into()),
        "windowactivated" | "windowactivate" | "window" => Some("WindowActivated".into()),
        "pixelchanged" | "pixel" => Some("PixelChanged".into()),
        "anyuichange" | "change" | "ui" => Some("AnyUiChange".into()),
        _ => None,
    }
}

fn canonical_button(name: &str) -> String {
    match name.to_ascii_lowercase().as_str() {
        "left" | "primary" => "Left".into(),
        "middle" => "Middle".into(),
        "right" | "secondary" => "Right".into(),
        _ => "Left".into(),
    }
}

#[derive(Debug, Clone)]
struct VisibleElement {
    role: String,
    name: String,
    cx: i32,
    cy: i32,
    width: u32,
    height: u32,
}

fn collect_visible_interactive(turn: &ReasoningTurn) -> Vec<VisibleElement> {
    let viewport_w: i32 = std::env::var("AGENT_SCREEN_WIDTH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1024);
    let viewport_h: i32 = std::env::var("AGENT_SCREEN_HEIGHT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(768);
    let mut out = Vec::new();
    for node in &turn.a11y_snapshot {
        if !is_interactive_role(&node.role) {
            continue;
        }
        let name = node.name.trim();
        if name.is_empty() {
            continue;
        }
        // Generic "this is an image link, not a text link" filter: any
        // element whose name ends in a common raster/vector image
        // extension is the alt-text of an <img> wrapped in an anchor —
        // clicking it leaves the article. This is universal across web
        // pages, not app-specific.
        if name_looks_like_image(name) {
            continue;
        }
        let Some(bounds) = &node.bounds else { continue };
        if bounds.width == 0 || bounds.height == 0 {
            continue;
        }
        let cx = bounds.x + (bounds.width as i32) / 2;
        let cy = bounds.y + (bounds.height as i32) / 2;
        if cx < 0 || cy < 0 || cx >= viewport_w || cy >= viewport_h {
            continue;
        }
        // Drop oversized "links" — anything wrapping >25% of viewport is
        // structurally a card, not a word-level click target. Universal
        // (not app-specific): a real text link is small.
        let area = bounds.width as i64 * bounds.height as i64;
        if area > (viewport_w as i64 * viewport_h as i64) / 4 {
            continue;
        }
        out.push(VisibleElement {
            role: node.role.clone(),
            name: name.to_string(),
            cx,
            cy,
            width: bounds.width,
            height: bounds.height,
        });
    }
    out
}

/// Snap a model-provided click to the actual interactive element it most
/// likely meant: prefer an element whose name matches the model's optional
/// `target` field; otherwise pick the visible element whose bounding box
/// contains the click; otherwise the closest visible element by center
/// distance, within a reasonable radius. Never silently moves a click that
/// is far from any interactive element — that case keeps the original
/// coords so a misplaced click is observable as a verification failure.
fn snap_click_to_visible(
    elements: &[VisibleElement],
    target_name: Option<&str>,
    raw_x: Option<i32>,
    raw_y: Option<i32>,
) -> Option<(i32, i32, String)> {
    if elements.is_empty() {
        return None;
    }
    if let Some(target) = target_name {
        let target_lc = target.to_ascii_lowercase();
        if let Some(e) = elements
            .iter()
            .find(|e| e.name.to_ascii_lowercase() == target_lc)
        {
            return Some((e.cx, e.cy, format!("name-match:{}", e.name)));
        }
        if let Some(e) = elements
            .iter()
            .find(|e| e.name.to_ascii_lowercase().contains(&target_lc))
        {
            return Some((e.cx, e.cy, format!("name-contains:{}", e.name)));
        }
    }
    let (Some(x), Some(y)) = (raw_x, raw_y) else {
        return None;
    };
    if let Some(e) = elements.iter().find(|e| {
        let half_w = (e.width as i32) / 2;
        let half_h = (e.height as i32) / 2;
        (x >= e.cx - half_w && x <= e.cx + half_w)
            && (y >= e.cy - half_h && y <= e.cy + half_h)
    }) {
        return Some((e.cx, e.cy, format!("contains:{}", e.name)));
    }
    let mut same_line_best: Option<(&VisibleElement, i32)> = None;
    for e in elements {
        let line_tolerance = ((e.height as i32) / 2 + 8).max(18);
        let dy = (y - e.cy).abs();
        if dy > line_tolerance {
            continue;
        }
        let dx = (x - e.cx).abs();
        if dx > 260 {
            continue;
        }
        if same_line_best
            .map(|(_, best_dx)| dx < best_dx)
            .unwrap_or(true)
        {
            same_line_best = Some((e, dx));
        }
    }
    if let Some((e, dx)) = same_line_best {
        return Some((e.cx, e.cy, format!("same-line(dx={dx}):{}", e.name)));
    }
    let mut best: Option<(&VisibleElement, i64)> = None;
    for e in elements {
        let dx = (x - e.cx) as i64;
        let dy = (y - e.cy) as i64;
        let d2 = dx * dx + dy * dy;
        if best.map(|(_, bd)| d2 < bd).unwrap_or(true) {
            best = Some((e, d2));
        }
    }
    if let Some((e, d2)) = best {
        if d2 <= 30 * 30 {
            return Some((e.cx, e.cy, format!("nearest:{}", e.name)));
        }
    }
    None
}

fn name_looks_like_image(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let lower = lower.trim_end_matches(')').trim_end_matches('"').trim();
    lower.ends_with(".jpg")
        || lower.ends_with(".jpeg")
        || lower.ends_with(".png")
        || lower.ends_with(".gif")
        || lower.ends_with(".svg")
        || lower.ends_with(".webp")
        || lower.ends_with(".bmp")
        || lower.ends_with(".tiff")
        || lower.ends_with(".ico")
}

fn action_kind_short(kind: &ActionKind) -> String {
    match kind {
        ActionKind::Click { x, y, .. } => format!(
            "Click({},{})",
            x.map(|v| v.to_string()).unwrap_or_else(|| "?".into()),
            y.map(|v| v.to_string()).unwrap_or_else(|| "?".into()),
        ),
        ActionKind::TypeText { text } => {
            format!("TypeText({:?})", text.chars().take(40).collect::<String>())
        }
        ActionKind::KeyCombo { keys } => format!("KeyCombo({})", keys.join("+")),
        ActionKind::MovePointer { x, y, .. } => format!("MovePointer({x},{y})"),
        ActionKind::Scroll { dx, dy } => format!("Scroll({dx},{dy})"),
        ActionKind::Drag { .. } => "Drag".into(),
        ActionKind::Finish { success, .. } => format!("Finish(success={success})"),
        ActionKind::Noop => "Noop".into(),
    }
}

fn is_interactive_role(role: &str) -> bool {
    let r = role.to_ascii_lowercase();
    matches!(
        r.as_str(),
        "link"
            | "push button"
            | "button"
            | "toggle button"
            | "menu item"
            | "menu button"
            | "check box"
            | "radio button"
            | "tab"
            | "combo box"
            | "tree item"
            | "entry"
            | "password text"
            | "spin button"
    ) || r.contains("link")
        || r.contains("button")
        || r.contains("entry")
}

fn parse_json_content(content: &str) -> Result<Value> {
    if let Ok(value) = serde_json::from_str::<Value>(content) {
        return Ok(value);
    }

    let start = content
        .find('{')
        .context("model content has no JSON object")?;
    let end = content
        .rfind('}')
        .context("model content has no JSON object terminator")?;
    Ok(serde_json::from_str(&content[start..=end])?)
}
