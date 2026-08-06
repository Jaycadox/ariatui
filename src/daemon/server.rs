use color_eyre::eyre::{Context, Result};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};
use tracing::{error, info};

use crate::daemon::permissions::{PeerIdentity, validate_request};
use crate::daemon::{ApiEnvelope, ApiError, ApiResponse, SharedDaemonState};

pub async fn run(state: SharedDaemonState) -> Result<()> {
    let socket_path = &state.app.paths.socket_path;
    if socket_path.exists() {
        let _ = tokio::fs::remove_file(socket_path).await;
    }
    if let Some(parent) = socket_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let listener = UnixListener::bind(socket_path)
        .wrap_err_with(|| format!("failed to bind socket {}", socket_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o666))?;
    }
    info!("daemon listening on {}", socket_path.display());

    loop {
        let (stream, _) = listener.accept().await?;
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(error) = handle_client(state, stream).await {
                error!("client failed: {error:?}");
            }
        });
    }
}

async fn handle_client(state: SharedDaemonState, stream: UnixStream) -> Result<()> {
    let credentials = stream.peer_cred()?;
    let peer = PeerIdentity {
        uid: credentials.uid(),
        gid: credentials.gid(),
        pid: credentials.pid().and_then(|pid| u32::try_from(pid).ok()),
    };
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        let mut envelope: ApiEnvelope = serde_json::from_str(&line)?;
        let response = match validate_request(&state, &mut envelope.request, peer).await {
            Err(error) => ApiResponse {
                id: envelope.id,
                ok: false,
                result: None,
                payload: None,
                error: Some(ApiError {
                    message: error.to_string(),
                }),
            },
            Ok(()) => match state.execute(envelope.request).await {
                Ok(reply) => ApiResponse {
                    id: envelope.id,
                    ok: true,
                    result: Some(reply.snapshot),
                    payload: reply.payload,
                    error: None,
                },
                Err(error) => ApiResponse {
                    id: envelope.id,
                    ok: false,
                    result: None,
                    payload: None,
                    error: Some(ApiError {
                        message: error.to_string(),
                    }),
                },
            },
        };
        let encoded = serde_json::to_vec(&response)?;
        writer.write_all(&encoded).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
    }
    Ok(())
}
