//! The daemon seam (v0.2.md D18–D19): daemon-first endpoint resolution and
//! the sidecar conversation, with transparent embedded fallback.
//!
//! `bed` never names the daemon in `config.toml` (v0.1 files keep loading
//! byte-compatibly). Discovery is environment + one fixed socket path —
//! the same rules the daemon itself uses, duplicated here because bed
//! must resolve them without linking the daemon binary:
//!
//! 1. `GARDENBED_DAEMON` — override. An **empty** value disables the
//!    daemon (`bed` never probes it; the embedded client talks to the
//!    configured server directly).
//! 2. `$XDG_RUNTIME_DIR/gardenbed/daemon.sock` when set and absolute.
//! 3. `$TMPDIR/gardenbed-<uid>/daemon.sock` (or `/tmp/...`) — per-uid so
//!    the daemon's 0700 directory stays private on multi-user machines.
//!
//! Probing means *connecting*: a present-but-dead daemon is
//! indistinguishable from an absent one, and in both cases bed falls back
//! to the embedded client. The daemon's death mid-run is handled one
//! layer up, in [`crate::rpc`]: fetch-phase RPCs retry exactly once
//! against the embedded endpoint; write RPCs are never blindly retried.

use std::path::PathBuf;

use gardenbed_daemon::daemon_sidecar_client::DaemonSidecarClient;

/// The daemon socket path bed probes, or `None` when the daemon is
/// disabled (`GARDENBED_DAEMON` set to empty). Same rules as the daemon's
/// own `paths::socket_path`.
pub fn socket_path() -> Option<PathBuf> {
    // The daemon is Unix-only (unix-socket transport). On Windows there
    // is nothing to probe: every session goes to the server directly.
    #[cfg(not(unix))]
    {
        return None;
    }
    #[cfg(unix)]
    if let Ok(value) = std::env::var("GARDENBED_DAEMON") {
        return if value.is_empty() {
            None
        } else {
            Some(PathBuf::from(value))
        };
    }
    #[cfg(unix)]
    {
        Some(default_socket_path())
    }
}

/// The default socket path, ignoring `GARDENBED_DAEMON`.
fn default_socket_path() -> PathBuf {
    if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR") {
        let runtime = PathBuf::from(runtime);
        if runtime.is_absolute() {
            return runtime.join("gardenbed").join("daemon.sock");
        }
    }
    tmp_fallback().join("daemon.sock")
}

/// `$TMPDIR/gardenbed-<uid>` (or `/tmp/...`).
fn tmp_fallback() -> PathBuf {
    let base = match std::env::var("TMPDIR") {
        Ok(tmp) if PathBuf::from(&tmp).is_absolute() => PathBuf::from(tmp),
        _ => PathBuf::from("/tmp"),
    };
    base.join(format!("gardenbed-{}", current_uid()))
}

/// This process's uid (the platform crate's: unix getuid, Windows RID).
fn current_uid() -> u32 {
    gardenbed_platform::current_uid()
}

/// The sidecar conversation, all of it: connect to the daemon's sidecar
/// over its unix socket.
///
/// Returns `Ok(None)` when the daemon is disabled or its socket is
/// absent/dead — never an error for a daemon that simply is not there.
/// The daemon is an optional tier; its absence is a normal state.
pub async fn sidecar()
-> Result<Option<DaemonSidecarClient<tonic::transport::Channel>>, tonic::Status> {
    let Some(path) = socket_path() else {
        return Ok(None);
    };
    let endpoint = format!("unix://{}", path.display());
    match DaemonSidecarClient::connect(endpoint).await {
        Ok(client) => Ok(Some(client)),
        Err(_) => Ok(None),
    }
}

/// Registers the working copy rooted at the parent of `state_dir` with the
/// daemon's watcher, returning its current generation.
///
/// `Ok(None)` when the daemon is disabled or unreachable: the caller must
/// treat "no generation" as "no fast path", never as "clean".
pub async fn register(state_dir: &std::path::Path) -> Result<Option<u64>, tonic::Status> {
    let Some(mut client) = sidecar().await? else {
        return Ok(None);
    };
    let resp = client
        .register(tonic::Request::new(gardenbed_daemon::pb::RegisterReq {
            state_dir: state_dir.to_string_lossy().into_owned(),
        }))
        .await?
        .into_inner();
    Ok(Some(resp.generation))
}

/// The current generation of a registered working copy, or `Ok(None)`
/// when the daemon is disabled/unreachable or the path is not registered
/// (watcher gone, daemon restarted). Any doubt means "full scan".
pub async fn generation(state_dir: &std::path::Path) -> Result<Option<u64>, tonic::Status> {
    let Some(mut client) = sidecar().await? else {
        return Ok(None);
    };
    let resp = client
        .generation(tonic::Request::new(gardenbed_daemon::pb::GenerationReq {
            state_dir: state_dir.to_string_lossy().into_owned(),
        }))
        .await?
        .into_inner();
    Ok(resp.generation)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_override_disables_the_daemon() {
        // The one rule that must never regress: GARDENBED_DAEMON="" means
        // bed never probes — the embedded client is the only endpoint.
        let guard = env_lock();
        unsafe {
            std::env::set_var("GARDENBED_DAEMON", "");
        }
        assert_eq!(socket_path(), None);
        unsafe {
            std::env::remove_var("GARDENBED_DAEMON");
        }
        let path = socket_path().expect("unset falls back to the default");
        assert!(path.ends_with("daemon.sock"), "{path:?}");
        drop(guard);
    }

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }
}
