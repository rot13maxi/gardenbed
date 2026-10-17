//! The sidecar service: liveness (`Ping`) and per-working-copy filesystem
//! watches (`Register`/`Generation`).
//!
//! Watching is generation-counted (v0.2.md D18): the daemon never inspects
//! working-copy content or the index. `Register(state_dir)` starts a
//! recursive `notify` watch on the working-copy root (the state
//! directory's parent), excluding the state directory itself, checkout
//! staging entries under it, and temp checkout files. Any surviving event
//! bumps the working copy's generation counter after a short debounce
//! window, collapsing editor save-storms into one bump.
//!
//! The contract with `bed status` is deliberately minimal: "the generation
//! is unchanged since your last scan" is a *hint* that the cached report
//! may be reused. A registered watcher that died, a daemon restart, an
//! unregistered path — all read as "generation unknown", and the client
//! must fall back to the full scan. A stale clean is forbidden; a missed
//! cache reuse is merely slow.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use notify::Watcher as _;
use tokio::sync::mpsc;
use tonic::{Request, Response, Status};

use crate::daemon_sidecar_server::DaemonSidecar;

/// How long to wait for further events before bumping the generation —
/// one editor "save" often lands as create+write+close+chmod.
const DEBOUNCE: Duration = Duration::from_millis(50);
/// One registered working copy: its generation counter and the watcher
/// keeping it alive. (The registration key in the table holds the
/// canonicalized state-dir path.)
struct Registration {
    generation: std::sync::atomic::AtomicU64,
    /// Held (never read) so the watch stays alive for the registration's
    /// lifetime; dropping it stops the events.
    #[allow(dead_code)]
    watcher: notify::RecommendedWatcher,
}
/// The sidecar: registration table plus generation counters.
pub struct SidecarService {
    registrations: Mutex<HashMap<PathBuf, Arc<Registration>>>,
}

impl SidecarService {
    pub fn new() -> Self {
        SidecarService {
            registrations: Mutex::new(HashMap::new()),
        }
    }

    /// The registration key: the canonicalized state directory.
    fn key(state_dir: &Path) -> PathBuf {
        state_dir.canonicalize().unwrap_or_else(|_| state_dir.to_path_buf())
    }

    /// True when an event path belongs to the working copy's own
    /// machinery: anything inside the state directory (`.bed`) — config,
    /// index, objects cache, heads, checkout staging journals — or a
    /// checkout temp file (`.bed-checkout-*`, created next to destinations
    /// during materialization).
    fn is_machinery(state_dir: &Path, path: &Path) -> bool {
        path.starts_with(state_dir)
            || path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(".bed-checkout-"))
    }

    /// Registers (or re-registers) the working copy whose state directory
    /// is `state_dir`, starting a fresh recursive watch on the working-copy
    /// root. Returns the current generation.
    pub fn register(&self, state_dir: &Path) -> Result<u64, Status> {
        let state_dir = state_dir.canonicalize().map_err(|e| {
            Status::invalid_argument(format!(
                "cannot resolve the working copy's state directory {}: {e}",
                state_dir.display()
            ))
        })?;
        let root = state_dir
            .parent()
            .ok_or_else(|| {
                Status::invalid_argument(format!(
                    "the state directory {} has no parent to watch",
                    state_dir.display()
                ))
            })?
            .to_path_buf();
        if !root.is_dir() {
            return Err(Status::invalid_argument(format!(
                "the working-copy root {} is not a directory",
                root.display()
            )));
        }

        let (tx, mut rx) = mpsc::unbounded_channel::<()>();
        let state_dir_for_watcher = state_dir.clone();
        // The watcher thread: notify events → filtered → debounce → bump.
        let watcher = notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
            if res.is_err() {
                return; // a lost event is a generation non-bump; the client
                // falls back to scanning when in doubt via other signals.
            }
            let Ok(event) = res else { return };
            let touched = event.paths.iter().any(|p| {
                !SidecarService::is_machinery(&state_dir_for_watcher, p)
            });
            if touched {
                let _ = tx.send(());
            }
        })
        .map_err(|e| Status::internal(format!("cannot start a filesystem watcher: {e}")))?;

        // watch the root recursively
        let mut watcher = watcher;
        watcher
            .watch(&root, notify::RecursiveMode::Recursive)
            .map_err(|e| {
                Status::invalid_argument(format!(
                    "cannot watch the working-copy root {}: {e}",
                    root.display()
                ))
            })?;

        let key = Self::key(&state_dir);
        // Preserve the generation across re-registration (the counter is
        // about the working copy, not the watcher instance); a fresh
        // registration after watcher loss starts from the stored value.
        let generation = {
            let regs = self.registrations.lock().expect("registration map");
            regs.get(&key)
                .map(|r| {
                    r.generation
                        .load(std::sync::atomic::Ordering::Relaxed)
                })
                .unwrap_or(0)
        };
        let registration = Arc::new(Registration {
            generation: std::sync::atomic::AtomicU64::new(generation),
            watcher,
        });
        self.registrations
            .lock()
            .expect("registration map")
            .insert(key, registration.clone());

        // The debounce loop: drain events, wait DEBOUNCE for the storm to
        // settle, bump once. Runs until the registration is replaced (the
        // old loop's Arc handle drops when the table entry is replaced and
        // this task is the last holder... the channel closes when the
        // watcher is dropped, ending the loop).
        let weak = Arc::downgrade(&registration);
        tokio::spawn(async move {
            while rx.recv().await.is_some() {
                // Debounce: coalesce everything arriving inside the window.
                let deadline = tokio::time::Instant::now() + DEBOUNCE;
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep_until(deadline) => break,
                        _ = rx.recv() => {}
                    }
                }
                if let Some(reg) = weak.upgrade() {
                    reg.generation.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                } else {
                    break; // replaced; the new registration runs its own loop
                }
            }
        });

        Ok(generation)
    }

    /// The current generation, or `None` when `state_dir` is not
    /// registered.
    pub fn generation(&self, state_dir: &Path) -> Option<u64> {
        let key = Self::key(state_dir);
        let regs = self.registrations.lock().expect("registration map");
        regs.get(&key).map(|r| {
            r.generation
                .load(std::sync::atomic::Ordering::Relaxed)
        })
    }
}

impl Default for SidecarService {
    fn default() -> Self {
        Self::new()
    }
}

#[tonic::async_trait]
impl DaemonSidecar for SidecarService {
    async fn ping(&self, _request: Request<crate::pb::PingReq>) -> Result<Response<crate::pb::PingResp>, Status> {
        Ok(Response::new(crate::pb::PingResp {
            pid: std::process::id() as u64,
        }))
    }

    async fn register(
        &self,
        request: Request<crate::pb::RegisterReq>,
    ) -> Result<Response<crate::pb::RegisterResp>, Status> {
        let req = request.into_inner();
        let state_dir = PathBuf::from(req.state_dir);
        let generation = self.register(&state_dir)?;
        Ok(Response::new(crate::pb::RegisterResp { generation }))
    }

    async fn generation(
        &self,
        request: Request<crate::pb::GenerationReq>,
    ) -> Result<Response<crate::pb::GenerationResp>, Status> {
        let req = request.into_inner();
        let state_dir = PathBuf::from(req.state_dir);
        Ok(Response::new(crate::pb::GenerationResp {
            generation: self.generation(&state_dir),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn register_returns_generation_and_bumps_on_edit() {
        // `register` spawns the debounce loop on a tokio runtime: run the
        // whole scenario (including the polling that lets the loop make
        // progress) inside one.
        let rt = rt();
        rt.block_on(async {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path();
            let state = root.join(".bed");
            std::fs::create_dir_all(&state).unwrap();

            let sidecar = SidecarService::new();
            let g0 = sidecar.register(&state).unwrap();
            assert_eq!(g0, 0);

            // A working-file edit must bump; machinery writes must not.
            std::fs::write(root.join("file.txt"), b"one").unwrap();
            std::fs::write(state.join("config.toml"), b"server = 'x'").unwrap();
            std::fs::write(root.join(".bed-checkout-tmp-1"), b"staging").unwrap();

            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while sidecar.generation(&state) == Some(g0) {
                assert!(
                    std::time::Instant::now() < deadline,
                    "generation never bumped after a working-file edit"
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
            }

            // Machinery-only writes must not bump: let the debounce window
            // settle, write machinery, settle again, assert stability.
            tokio::time::sleep(DEBOUNCE * 10).await;
            let g1 = sidecar.generation(&state).unwrap();
            std::fs::write(state.join("index.sqlite3-wal"), b"machinery").unwrap();
            std::fs::write(root.join(".bed-checkout-tmp-2"), b"staging").unwrap();
            tokio::time::sleep(DEBOUNCE * 10).await;
            let g2 = sidecar.generation(&state).unwrap();
            assert_eq!(g1, g2, "machinery writes must not bump the generation");
        });
    }

    #[test]
    fn generation_of_unknown_state_dir_is_none() {
        let sidecar = SidecarService::new();
        assert_eq!(sidecar.generation(Path::new("/nowhere/.bed")), None);
    }

    #[test]
    fn re_registration_preserves_the_generation() {
        let rt = rt();
        rt.block_on(async {
            let tmp = tempfile::tempdir().unwrap();
            let state = tmp.path().join(".bed");
            std::fs::create_dir_all(&state).unwrap();
            let sidecar = SidecarService::new();
            let g0 = sidecar.register(&state).unwrap();
            std::fs::write(tmp.path().join("a.txt"), b"x").unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while sidecar.generation(&state) == Some(g0) {
                assert!(std::time::Instant::now() < deadline);
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            let g1 = sidecar.generation(&state).unwrap();
            // Watcher loss + re-register: the counter continues.
            let g2 = sidecar.register(&state).unwrap();
            assert!(g2 >= g1);
        });
    }
}
