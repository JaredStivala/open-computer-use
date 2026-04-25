use anyhow::{Context, Result};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{fs, path::Path};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
};
use tracing_subscriber::{EnvFilter, fmt};

pub fn init_tracing() {
    let _ = fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .try_init();
}

pub fn load_dotenv() {
    let _ = dotenvy::dotenv();
}

#[cfg(unix)]
pub fn monotonic_ns() -> u128 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    if rc == 0 {
        (ts.tv_sec as u128 * 1_000_000_000) + ts.tv_nsec as u128
    } else {
        fallback_monotonic_ns()
    }
}

#[cfg(not(unix))]
pub fn monotonic_ns() -> u128 {
    fallback_monotonic_ns()
}

fn fallback_monotonic_ns() -> u128 {
    use std::sync::OnceLock;
    use std::time::Instant;

    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_nanos()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AgentConfig {
    pub sockets: SocketConfig,
    pub timeouts_ms: TimeoutConfig,
    pub reasoning: ReasoningConfig,
    pub task: TaskConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SocketConfig {
    pub bus: String,
    pub capture: String,
    pub a11y: String,
    pub input: String,
    pub action: String,
    pub verify: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TimeoutConfig {
    pub verification_default: u64,
    pub model_request: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReasoningConfig {
    pub provider: String,
    pub base_url: String,
    pub model: String,
    pub stream: bool,
    pub dispatch_early: bool,
    #[serde(default = "default_vision_enabled")]
    pub vision_enabled: bool,
    #[serde(default)]
    pub min_request_interval_ms: u64,
}

fn default_vision_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TaskConfig {
    pub step_retry_budget: u32,
    pub task_retry_budget: u32,
}

impl AgentConfig {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let raw = fs::read_to_string(path.as_ref())
            .with_context(|| format!("failed to read config {}", path.as_ref().display()))?;
        Ok(toml::from_str(&raw)?)
    }
}

pub async fn bind_socket(path: &str) -> Result<UnixListener> {
    if let Some(parent) = Path::new(path).parent() {
        fs::create_dir_all(parent)?;
    }
    let _ = fs::remove_file(path);
    Ok(UnixListener::bind(path)?)
}

pub async fn recv_msg<T: DeserializeOwned>(stream: &mut UnixStream) -> Result<T> {
    let len = stream.read_u32().await? as usize;
    let mut buf = vec![0_u8; len];
    stream.read_exact(&mut buf).await?;
    Ok(rmp_serde::from_slice(&buf)?)
}

pub async fn send_msg<T: Serialize>(stream: &mut UnixStream, msg: &T) -> Result<()> {
    let body = rmp_serde::to_vec_named(msg)?;
    stream.write_u32(body.len() as u32).await?;
    stream.write_all(&body).await?;
    Ok(())
}

pub async fn connect_socket(path: &str) -> Result<UnixStream> {
    Ok(UnixStream::connect(path).await?)
}
