use std::{
    path::PathBuf,
    process::Stdio,
    time::{Duration, Instant},
};

use color_eyre::eyre::{Context, Result, bail};
use rand::{RngExt, distr::Alphanumeric};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
    sync::mpsc,
};
use tracing::{error, info, warn};

use crate::config::AppConfig;

#[derive(Debug)]
pub struct ChildProcess {
    pub process: Child,
    pub port: u16,
    pub secret: String,
}

#[derive(Debug, Clone)]
pub enum ChildLogEvent {
    Stdout(String),
    Stderr(String),
}

pub fn choose_rpc_port() -> u16 {
    rand::rng().random_range(6800..=6899)
}

pub fn generate_secret() -> String {
    rand::rng()
        .sample_iter(&Alphanumeric)
        .take(32)
        .map(char::from)
        .collect()
}

pub async fn spawn_aria2(
    config: &AppConfig,
    session_file: PathBuf,
) -> Result<(ChildProcess, mpsc::UnboundedReceiver<ChildLogEvent>)> {
    let aria2_bin = which::which(&config.daemon.aria2_bin)
        .wrap_err_with(|| format!("failed to find '{}'", config.daemon.aria2_bin))?;
    let port = choose_rpc_port();
    let secret = generate_secret();

    let mut command = Command::new(aria2_bin);
    command
        .arg("--enable-rpc=true")
        .arg("--rpc-listen-all=false")
        .arg(format!("--rpc-secret={secret}"))
        .arg(format!("--rpc-listen-port={port}"))
        .arg(format!(
            "--dir={}",
            expand_tilde(&config.daemon.download_dir).display()
        ))
        .arg("--continue=true")
        .arg("--rpc-save-upload-metadata=true")
        .arg("--bt-save-metadata=true")
        .arg(format!(
            "--max-download-result={}",
            config.daemon.stopped_history_limit
        ))
        .arg(format!("--save-session={}", session_file.display()))
        .arg(format!("--input-file={}", session_file.display()))
        .arg("--save-session-interval=60")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = command.spawn().wrap_err("failed to spawn aria2c")?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (tx, rx) = mpsc::unbounded_channel();

    if let Some(stdout) = stdout {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = tx.send(ChildLogEvent::Stdout(line));
            }
        });
    }

    if let Some(stderr) = stderr {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = tx.send(ChildLogEvent::Stderr(line));
            }
        });
    }

    info!("spawned aria2c on rpc port {port}");

    Ok((
        ChildProcess {
            process: child,
            port,
            secret,
        },
        rx,
    ))
}

pub async fn wait_for_rpc_ready<F, Fut>(timeout: Duration, mut probe: F) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if probe().await.is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for aria2 rpc readiness");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

pub async fn log_child_output(mut rx: mpsc::UnboundedReceiver<ChildLogEvent>) {
    while let Some(event) = rx.recv().await {
        match event {
            ChildLogEvent::Stdout(line) => info!("aria2c: {line}"),
            ChildLogEvent::Stderr(line) => warn!("aria2c: {line}"),
        }
    }
    error!("aria2c log stream closed");
}

fn expand_tilde(value: &str) -> PathBuf {
    if let Some(stripped) = value.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(stripped);
        }
    }
    PathBuf::from(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;

    #[test]
    fn generates_secret() {
        let secret = generate_secret();
        assert_eq!(secret.len(), 32);
    }

    #[test]
    fn picks_port_in_range() {
        let port = choose_rpc_port();
        assert!((6800..=6899).contains(&port));
    }

    #[test]
    fn config_default_aria2_bin() {
        let config = AppConfig::default();
        assert_eq!(config.daemon.aria2_bin, "aria2c");
    }
}
