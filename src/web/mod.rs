pub mod server;

use std::{
    net::IpAddr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use color_eyre::eyre::{Result, bail, eyre};
use hmac::{Hmac, Mac};
use rand::{RngExt, distr::Alphanumeric};
use sha2::Sha256;
use tokio::{net::TcpListener, sync::oneshot};
use tracing::{error, info, warn};

use crate::daemon::{
    DaemonState, SharedDaemonState,
    snapshot::{WebUiSnapshot, WebUiStatus},
};

pub const AUTH_COOKIE_NAME: &str = "ariatui_auth";
pub const PAIR_COOKIE_NAME: &str = "ariatui_pair";
pub const PAIRING_TTL_SECS: u64 = 300;
const SESSION_COOKIE_VERSION: &str = "v1";
type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, PartialEq, Eq)]
struct DesiredWebConfig {
    enabled: bool,
    bind_address: String,
    port: u16,
    cookie_days: u32,
}

struct ActiveWebServer {
    config: DesiredWebConfig,
    shutdown: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<Result<()>>,
}

pub fn validate_bind_address(input: &str) -> Result<IpAddr> {
    input
        .trim()
        .parse::<IpAddr>()
        .map_err(|_| color_eyre::eyre::eyre!("bind address must be a valid IPv4 or IPv6 address"))
}

pub fn validate_cookie_days(days: u32) -> Result<()> {
    if (1..=365).contains(&days) {
        Ok(())
    } else {
        bail!("cookie lifetime must be between 1 and 365 days")
    }
}

pub fn format_listener_url(bind_address: &str, port: u16) -> String {
    let host = if bind_address.contains(':') && !bind_address.starts_with('[') {
        format!("[{bind_address}]")
    } else {
        bind_address.to_string()
    };
    format!("http://{host}:{port}")
}

pub fn generate_login_token() -> String {
    rand::rng()
        .sample_iter(Alphanumeric)
        .take(32)
        .map(char::from)
        .collect()
}

pub fn generate_pair_request_id() -> String {
    generate_login_token()
}

pub fn generate_pair_pin() -> String {
    format!("{:04}", rand::random::<u16>() % 10_000)
}

pub async fn auth_summary(state: &DaemonState) -> (Vec<String>, usize) {
    cleanup_expired_auth(state).await;
    let pending_pair_pins = state
        .web_pairings
        .lock()
        .await
        .values()
        .filter(|pairing| pairing.approved_session_token.is_none())
        .map(|pairing| pairing.pin.clone())
        .collect::<Vec<_>>();
    let active_session_count = state.web_sessions.lock().await.len();
    (pending_pair_pins, active_session_count)
}

pub async fn cleanup_expired_auth(state: &DaemonState) {
    let now = std::time::Instant::now();
    state
        .web_pairings
        .lock()
        .await
        .retain(|_, pairing| pairing.expires_at > now);
    state
        .web_sessions
        .lock()
        .await
        .retain(|_, expiry| *expiry > now);
}

pub async fn create_or_get_pairing(
    state: &DaemonState,
    existing_request_id: Option<&str>,
) -> Result<(String, String)> {
    cleanup_expired_auth(state).await;
    let mut pairings = state.web_pairings.lock().await;
    if let Some(request_id) = existing_request_id {
        if let Some(pairing) = pairings.get(request_id) {
            return Ok((request_id.to_string(), pairing.pin.clone()));
        }
    }

    let pin = loop {
        let candidate = generate_pair_pin();
        if !pairings.values().any(|pairing| pairing.pin == candidate) {
            break candidate;
        }
    };
    let request_id = generate_pair_request_id();
    pairings.insert(
        request_id.clone(),
        crate::daemon::reconcile::WebPairing {
            pin: pin.clone(),
            expires_at: std::time::Instant::now() + Duration::from_secs(PAIRING_TTL_SECS),
            approved_session_token: None,
        },
    );
    Ok((request_id, pin))
}

pub async fn pairing_status(state: &DaemonState, request_id: &str) -> Result<PairingStatus> {
    cleanup_expired_auth(state).await;
    let pairings = state.web_pairings.lock().await;
    let Some(pairing) = pairings.get(request_id) else {
        return Ok(PairingStatus::Expired);
    };
    if let Some(token) = pairing.approved_session_token.clone() {
        return Ok(PairingStatus::Approved { auth_token: token });
    }
    Ok(PairingStatus::Pending)
}

pub async fn approve_pairing_pin(state: &DaemonState, pin: &str) -> Result<()> {
    cleanup_expired_auth(state).await;
    let pin = pin.trim();
    if pin.len() != 4 || !pin.chars().all(|ch| ch.is_ascii_digit()) {
        return Err(eyre!("pin must be exactly 4 digits"));
    }
    let persisted = state.app.state.read().await.clone();
    let expires_at = std::time::Instant::now()
        + Duration::from_secs(persisted.web_ui_cookie_days as u64 * 86_400);
    let session_token = issue_session_cookie_value(state, persisted.web_ui_cookie_days).await?;
    let mut pairings = state.web_pairings.lock().await;
    let Some(pairing) = pairings
        .values_mut()
        .find(|pairing| pairing.pin == pin && pairing.approved_session_token.is_none())
    else {
        return Err(eyre!("no pending browser pairing matches pin {pin}"));
    };
    pairing.approved_session_token = Some(session_token.clone());
    drop(pairings);
    state
        .web_sessions
        .lock()
        .await
        .insert(session_token, expires_at);
    Ok(())
}

pub async fn session_is_valid(state: &DaemonState, token: &str) -> bool {
    cleanup_expired_auth(state).await;
    if verify_session_cookie_value(state, token).await {
        return true;
    }
    state.web_sessions.lock().await.contains_key(token)
}

pub async fn remove_session(state: &DaemonState, token: &str) {
    state.web_sessions.lock().await.remove(token);
}

pub async fn ensure_session_secret(state: &DaemonState) -> Result<String> {
    let mut persisted = state.app.state.write().await;
    if persisted.web_ui_session_secret.trim().is_empty() {
        persisted.web_ui_session_secret = generate_login_token();
        persisted.save(&state.app.paths.state_file)?;
    }
    Ok(persisted.web_ui_session_secret.clone())
}

pub async fn issue_session_cookie_value(state: &DaemonState, cookie_days: u32) -> Result<String> {
    let secret = ensure_session_secret(state).await?;
    let expires_at = SystemTime::now()
        .checked_add(Duration::from_secs(cookie_days as u64 * 86_400))
        .ok_or_else(|| eyre!("failed to calculate session expiry"))?
        .duration_since(UNIX_EPOCH)
        .map_err(|_| eyre!("system clock is before unix epoch"))?
        .as_secs();
    let nonce = generate_pair_request_id();
    let payload = format!("{SESSION_COOKIE_VERSION}.{expires_at}.{nonce}");
    let signature = sign_session_payload(&secret, &payload)?;
    Ok(format!("{payload}.{signature}"))
}

pub async fn verify_session_cookie_value(state: &DaemonState, token: &str) -> bool {
    let secret = {
        let persisted = state.app.state.read().await;
        persisted.web_ui_session_secret.clone()
    };
    if secret.trim().is_empty() {
        return false;
    }
    verify_session_cookie_value_with_secret(&secret, token)
}

fn sign_session_payload(secret: &str, payload: &str) -> Result<String> {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .map_err(|_| eyre!("invalid session secret"))?;
    mac.update(payload.as_bytes());
    Ok(URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes()))
}

fn verify_session_cookie_value_with_secret(secret: &str, token: &str) -> bool {
    let mut parts = token.split('.');
    let Some(version) = parts.next() else {
        return false;
    };
    let Some(expiry) = parts.next() else {
        return false;
    };
    let Some(nonce) = parts.next() else {
        return false;
    };
    let Some(signature) = parts.next() else {
        return false;
    };
    if parts.next().is_some() || version != SESSION_COOKIE_VERSION {
        return false;
    }
    let Ok(expiry_unix) = expiry.parse::<u64>() else {
        return false;
    };
    let now_unix = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs(),
        Err(_) => return false,
    };
    if expiry_unix <= now_unix {
        return false;
    }
    let payload = format!("{version}.{expiry}.{nonce}");
    let Ok(signature_bytes) = URL_SAFE_NO_PAD.decode(signature.as_bytes()) else {
        return false;
    };
    let Ok(mut mac) = HmacSha256::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(payload.as_bytes());
    mac.verify_slice(&signature_bytes).is_ok()
}

#[derive(Debug, Clone)]
pub enum PairingStatus {
    Pending,
    Approved { auth_token: String },
    Expired,
}

pub async fn supervise(state: SharedDaemonState) -> Result<()> {
    let mut active: Option<ActiveWebServer> = None;
    loop {
        let desired = {
            let persisted = state.app.state.read().await;
            DesiredWebConfig {
                enabled: persisted.web_ui_enabled,
                bind_address: persisted.web_ui_bind_address.clone(),
                port: persisted.web_ui_port,
                cookie_days: persisted.web_ui_cookie_days,
            }
        };

        if let Some(current) = active.take() {
            let mut keep = Some(current);
            let should_restart = keep.as_ref().is_some_and(|server| {
                !desired.enabled || server.config != desired || server.task.is_finished()
            });
            if should_restart {
                if let Some(server) = keep.take() {
                    let _ = server.shutdown.send(());
                    if server.task.is_finished() {
                        match server.task.await {
                            Ok(Ok(())) => {}
                            Ok(Err(error)) => {
                                warn!("web server exited: {error}");
                            }
                            Err(error) => {
                                warn!("web server task join failed: {error}");
                            }
                        }
                    } else {
                        server.task.abort();
                    }
                }
            }
            active = keep;
        }

        if desired.enabled && active.is_none() {
            match start_server(state.clone(), desired.clone()).await {
                Ok(server) => {
                    info!(
                        "web ui listening on {}",
                        format_listener_url(&desired.bind_address, desired.port)
                    );
                    set_web_snapshot(
                        state.as_ref(),
                        WebUiStatus::Listening,
                        None,
                        Some(format_listener_url(&desired.bind_address, desired.port)),
                    )
                    .await;
                    active = Some(server);
                }
                Err(error) => {
                    error!("failed to start web ui: {error}");
                    set_web_snapshot(
                        state.as_ref(),
                        WebUiStatus::Failed,
                        Some(error.to_string()),
                        Some(format_listener_url(&desired.bind_address, desired.port)),
                    )
                    .await;
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        } else if !desired.enabled {
            set_web_snapshot(state.as_ref(), WebUiStatus::Disabled, None, None).await;
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn start_server(
    state: SharedDaemonState,
    config: DesiredWebConfig,
) -> Result<ActiveWebServer> {
    validate_bind_address(&config.bind_address)?;
    validate_cookie_days(config.cookie_days)?;
    if config.port == 0 {
        bail!("web ui port must be between 1 and 65535");
    }
    set_web_snapshot(
        state.as_ref(),
        WebUiStatus::Starting,
        None,
        Some(format_listener_url(&config.bind_address, config.port)),
    )
    .await;

    let listener = TcpListener::bind((config.bind_address.parse::<IpAddr>()?, config.port)).await?;
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let app = server::router(state);
    let task = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await?;
        Ok(())
    });

    Ok(ActiveWebServer {
        config,
        shutdown: shutdown_tx,
        task,
    })
}

pub async fn set_web_snapshot(
    state: &crate::daemon::DaemonState,
    status: WebUiStatus,
    last_error: Option<String>,
    url_override: Option<String>,
) {
    let persisted = state.app.state.read().await.clone();
    let mut snapshot = state.snapshot.write().await;
    snapshot.web_ui = WebUiSnapshot {
        enabled: persisted.web_ui_enabled,
        bind_address: persisted.web_ui_bind_address.clone(),
        port: persisted.web_ui_port,
        cookie_days: persisted.web_ui_cookie_days,
        status,
        url: url_override.unwrap_or_else(|| {
            format_listener_url(&persisted.web_ui_bind_address, persisted.web_ui_port)
        }),
        auth_configured: true,
        pending_pair_pins: Vec::new(),
        active_session_count: 0,
        last_error,
    };
}
