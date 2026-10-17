//! Unix-socket serving: bind, 0700 directory, announce, serve both
//! services on one socket.

use std::path::Path;

use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server;

/// Serves the proxy and sidecar services on a unix socket at `path`.
///
/// The parent directory is created (0700) and any stale socket at `path`
/// is probed: a live daemon there fails the bind with a clear error; a
/// dead socket is unlinked first. The bound socket is announced on stdout
/// as `listening on <path>` — the same contract the server binary offers
/// tests and scripts.
pub async fn serve(
    path: &Path,
    proxy: crate::proxy::ProxyService,
    sidecar: crate::sidecar::SidecarService,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    // 0700: only this uid may even connect. The peer-uid check on write
    // RPCs is the second lock on the same door.
    set_private_mode(parent)?;

    // A stale socket from a crashed daemon would fail the bind; probe it.
    if path.exists() {
        if is_live_daemon(path).await {
            return Err(format!(
                "another daemon is already listening at {} — stop it first \
                 (or point GARDENBED_DAEMON elsewhere)",
                path.display()
            )
            .into());
        }
        std::fs::remove_file(path)?;
    }

    let listener = UnixListener::bind(path)?;
    println!("listening on {}", path.display());

    Server::builder()
        .add_service(gardenbed_protocol::gardenbed_server::GardenbedServer::new(
            proxy,
        ))
        .add_service(crate::daemon_sidecar_server::DaemonSidecarServer::new(
            sidecar,
        ))
        .serve_with_incoming_shutdown(UnixListenerStream::new(listener), shutdown_signal())
        .await?;

    // Clean shutdown removes the socket so a restart does not see a stale
    // file (best-effort; the stale probe above covers crashes anyway).
    let _ = std::fs::remove_file(path);
    Ok(())
}

/// `chmod 0700` on a directory (no-op on non-unix).
fn set_private_mode(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// True when something answers a `Ping` on the socket at `path`.
async fn is_live_daemon(path: &Path) -> bool {
    let endpoint = format!("unix://{}", path.display());
    let Ok(mut client) = crate::daemon_sidecar_client::DaemonSidecarClient::connect(endpoint).await
    else {
        return false;
    };
    client
        .ping(tonic::Request::new(crate::pb::PingReq {}))
        .await
        .map(|_| true)
        .unwrap_or(false)
}

/// Resolves when SIGINT or SIGTERM arrives.
async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler installs");
        tokio::select! {
            _ = ctrl_c => {},
            _ = term.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        _ = ctrl_c.await;
    }
}
