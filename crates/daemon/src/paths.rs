//! Where the daemon's socket and shared cache live.
//!
//! Discovery is environment + one fixed socket path (v0.2.md D19) — no
//! config key, so v0.1 `.bed/config.toml` files keep loading byte-compatibly.

use std::path::PathBuf;

/// The daemon's unix socket path.
///
/// Resolution order:
///
/// 1. `GARDENBED_DAEMON` — override. An **empty** value disables the daemon
///    (`bed` never probes it; the embedded client talks to the configured
///    server directly).
/// 2. `$XDG_RUNTIME_DIR/gardenbed/daemon.sock` when `XDG_RUNTIME_DIR` is
///    set and absolute.
/// 3. `$TMPDIR/gardenbed-<uid>/daemon.sock` (or `/tmp` when `TMPDIR` is
///    unset/relative) — the no-XDG fallback; per-uid so the 0700 directory
///    stays private on multi-user machines.
///
/// Returns `None` only for the empty-`GARDENBED_DAEMON` case.
pub fn socket_path() -> Option<PathBuf> {
    if let Ok(value) = std::env::var("GARDENBED_DAEMON") {
        return if value.is_empty() { None } else { Some(PathBuf::from(value)) };
    }
    Some(default_socket_path())
}

/// The socket path ignoring `GARDENBED_DAEMON` (used by `serve` itself and
/// tests that need the computed default).
pub fn default_socket_path() -> PathBuf {
    if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR") {
        let runtime = PathBuf::from(runtime);
        if runtime.is_absolute() {
            return runtime.join("gardenbed").join("daemon.sock");
        }
    }
    tmp_fallback().join("daemon.sock")
}

/// `$TMPDIR/gardenbed-<uid>` (or `/tmp/...`), mirroring `systemd`'s
/// no-XDG-RUNTIME_DIR convention.
fn tmp_fallback() -> PathBuf {
    let base = match std::env::var("TMPDIR") {
        Ok(tmp) if PathBuf::from(&tmp).is_absolute() => PathBuf::from(tmp),
        _ => PathBuf::from("/tmp"),
    };
    let uid = current_uid();
    base.join(format!("gardenbed-{uid}"))
}

/// The shared object cache root: `$XDG_CACHE_HOME/gardenbed`, else
/// `$HOME/.cache/gardenbed`, else the tmp fallback.
pub fn cache_root() -> PathBuf {
    if let Ok(cache) = std::env::var("XDG_CACHE_HOME") {
        let cache = PathBuf::from(cache);
        if cache.is_absolute() {
            return cache.join("gardenbed");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        let home = PathBuf::from(home);
        if home.is_absolute() {
            return home.join(".cache").join("gardenbed");
        }
    }
    tmp_fallback().join("cache")
}

/// This process's uid (0 on non-unix).
pub fn current_uid() -> u32 {
    #[cfg(unix)]
    {
        // SAFETY: `getuid` takes no arguments and cannot fail.
        unsafe { libc::getuid() }
    }
    #[cfg(not(unix))]
    {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_override_wins_and_empty_disables() {
        // GARDENBED_DAEMON: set wins, empty disables, unset falls back to
        // the XDG default. Env mutation is unsafe in this edition: these
        // tests hold the module's env lock and force single-threaded
        // isolation via the serial guard.
        let guard = env_lock();
        unsafe {
            std::env::set_var("GARDENBED_DAEMON", "/tmp/override.sock");
            assert_eq!(
                socket_path().as_deref(),
                Some(std::path::Path::new("/tmp/override.sock"))
            );
            std::env::set_var("GARDENBED_DAEMON", "");
            assert_eq!(socket_path(), None);
            std::env::remove_var("GARDENBED_DAEMON");
        }
        let path = socket_path().expect("default path resolves");
        assert!(path.ends_with("daemon.sock"), "{path:?}");
        drop(guard);
    }

    /// One test process mutates envs; tests run in parallel. The lock is
    /// best-effort deduplication of the same vars this module touches.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn cache_root_is_xdg_aware() {
        let guard = env_lock();
        unsafe {
            std::env::set_var("XDG_CACHE_HOME", "/tmp/xdg-cache");
            assert_eq!(cache_root(), PathBuf::from("/tmp/xdg-cache/gardenbed"));
            std::env::remove_var("XDG_CACHE_HOME");
            std::env::remove_var("HOME");
        }
        let root = cache_root();
        let shown = root.to_string_lossy();
        assert!(
            shown.starts_with("/tmp/gardenbed-") && shown.ends_with("/cache"),
            "{root:?}"
        );
        drop(guard);
    }
}
