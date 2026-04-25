use agent_common::{AgentConfig, init_tracing, load_dotenv};
use agent_proto::TaskReport;
use anyhow::{Context, Result, bail};
use clap::Parser;
use futures_util::future::join_all;
use serde::Deserialize;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    process::{Child, Command},
    time::sleep,
};

#[derive(Parser, Clone)]
struct Args {
    #[arg(long, default_value = "config/agent.example.toml")]
    config: String,
    #[arg(long = "goal")]
    goals: Vec<String>,
    #[arg(long)]
    suite: Option<String>,
    #[arg(long, default_value_t = 5)]
    max_parallel: usize,
    #[arg(long, default_value_t = 90)]
    display_base: u32,
    #[arg(long, default_value = "1280x720")]
    screen: String,
    #[arg(long, default_value_t = false)]
    scripted_task: bool,
}

#[derive(Debug, Deserialize)]
struct BenchmarkSuite {
    tasks: Vec<BenchmarkTask>,
}

#[derive(Debug, Deserialize)]
struct BenchmarkTask {
    id: String,
    goal: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    load_dotenv();
    init_tracing();
    let args = Args::parse();
    let cfg = AgentConfig::load(&args.config)?;
    let goals = collect_goals(&args)?;
    if goals.is_empty() {
        bail!("provide at least one --goal or --suite");
    }
    let bin_dir = std::env::current_exe()?
        .parent()
        .context("current executable has no parent")?
        .to_path_buf();

    let started = chrono::Utc::now().timestamp_millis();
    let mut reports = Vec::new();
    let max_parallel = args.max_parallel.max(1);
    for chunk in goals.chunks(max_parallel) {
        let futures = chunk.iter().cloned().map(|(index, goal)| {
            run_task(
                index,
                goal,
                args.clone(),
                cfg.clone(),
                bin_dir.clone(),
                started,
            )
        });
        for result in join_all(futures).await {
            reports.push(result?);
        }
    }
    println!("{}", serde_json::to_string_pretty(&reports)?);
    Ok(())
}

fn collect_goals(args: &Args) -> Result<Vec<(usize, String)>> {
    let mut goals = Vec::new();
    for goal in &args.goals {
        goals.push((goals.len(), goal.clone()));
    }
    if let Some(path) = &args.suite {
        let raw = fs::read_to_string(path).with_context(|| format!("read suite {path}"))?;
        let suite: BenchmarkSuite = toml::from_str(&raw)?;
        for task in suite.tasks {
            goals.push((goals.len(), format!("[{}] {}", task.id, task.goal)));
        }
    }
    Ok(goals)
}

async fn run_task(
    index: usize,
    goal: String,
    args: Args,
    mut cfg: AgentConfig,
    bin_dir: PathBuf,
    started: i64,
) -> Result<TaskReport> {
    let display = format!(":{}", args.display_base + index as u32);
    let task_dir = Path::new("/tmp/agent/tasks").join(format!("{started}-{index}"));
    fs::create_dir_all(&task_dir)?;
    rewrite_sockets(&mut cfg, &task_dir);
    let config_path = task_dir.join("agent.toml");
    fs::write(&config_path, toml::to_string_pretty(&cfg)?)?;

    let mut children = Vec::<Child>::new();
    children.push(spawn_xephyr(&display, &args.screen).await?);
    sleep(Duration::from_millis(500)).await;

    children.push(spawn_daemon(&bin_dir, "busd", &config_path, &[], None).await?);
    sleep(Duration::from_millis(100)).await;
    children.push(
        spawn_daemon(
            &bin_dir,
            "captured",
            &config_path,
            &["--display-id", &display],
            None,
        )
        .await?,
    );
    children.push(
        spawn_daemon(
            &bin_dir,
            "a11yd",
            &config_path,
            &["--display-id", &display],
            None,
        )
        .await?,
    );
    children.push(spawn_daemon(&bin_dir, "inputd", &config_path, &[], None).await?);
    children.push(spawn_daemon(&bin_dir, "verifyd", &config_path, &[], None).await?);
    if !args.scripted_task {
        children.push(spawn_daemon(&bin_dir, "reasonerd", &config_path, &[], None).await?);
    }
    sleep(Duration::from_millis(500)).await;

    let mut supervisor_args = vec![
        "--goal".to_string(),
        goal,
        "--display-id".to_string(),
        display,
    ];
    if args.scripted_task {
        supervisor_args.push("--scripted-task".to_string());
    }
    let output = spawn_daemon(
        &bin_dir,
        "supervisord",
        &config_path,
        &supervisor_args
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        Some(Stdio::piped()),
    )
    .await?
    .wait_with_output()
    .await?;

    for mut child in children {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }

    if !output.status.success() {
        bail!(
            "supervisor failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let report = serde_json::from_slice::<TaskReport>(&output.stdout)
        .context("parse supervisor task report")?;
    Ok(report)
}

fn rewrite_sockets(cfg: &mut AgentConfig, task_dir: &Path) {
    cfg.sockets.bus = task_dir.join("bus.sock").display().to_string();
    cfg.sockets.capture = task_dir.join("capture.sock").display().to_string();
    cfg.sockets.a11y = task_dir.join("a11y.sock").display().to_string();
    cfg.sockets.input = task_dir.join("input.sock").display().to_string();
    cfg.sockets.action = task_dir.join("action.sock").display().to_string();
    cfg.sockets.verify = task_dir.join("verify.sock").display().to_string();
}

async fn spawn_xephyr(display: &str, screen: &str) -> Result<Child> {
    Command::new("Xephyr")
        .args([display, "-screen", screen, "-nolisten", "tcp"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn Xephyr")
}

async fn spawn_daemon(
    bin_dir: &Path,
    name: &str,
    config_path: &Path,
    extra_args: &[&str],
    stdout: Option<Stdio>,
) -> Result<Child> {
    let mut cmd = Command::new(bin_dir.join(name));
    cmd.arg("--config")
        .arg(config_path)
        .args(extra_args)
        .stdin(Stdio::null())
        .stdout(stdout.unwrap_or_else(Stdio::null))
        .stderr(Stdio::piped());
    cmd.spawn().with_context(|| format!("spawn {name}"))
}
