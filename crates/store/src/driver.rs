//! Driver-thread pattern for async backends behind sync traits.
//!
//! `ObjectStore` and `MetadataStore` are synchronous traits, but the
//! Postgres and S3 clients are async (tokio). Calling
//! `Runtime::block_on` from a trait method would panic whenever the caller
//! is itself inside a tokio worker (blocking a worker on its own runtime
//! is rejected by tokio), so each async-backed store owns a *private*
//! single-threaded runtime living in a dedicated driver thread.
//!
//! The thread is captured once at open as a [`tokio::runtime::Handle`];
//! every trait call `block_on`s that handle. The driver thread runs for
//! the store's lifetime (it parks inside `pending::<()>()`, which costs
//! nothing) and is torn down with the process — the stores are opened
//! once per replica and never closed.

use tokio::runtime::Handle;

/// A dedicated driver thread + its runtime handle.
#[derive(Debug)]
pub(crate) struct Driver {
    /// Handle into the driver thread's current-thread runtime.
    handle: Handle,
}

impl Driver {
    /// Spawns the driver thread and returns once its runtime is live.
    ///
    /// Panics (in the *opening* thread) only when the OS refuses the
    /// thread or the runtime cannot be built — never in library operation.
    pub(crate) fn new(thread_name: &str) -> Driver {
        let (tx, rx) = std::sync::mpsc::channel::<Handle>();
        std::thread::Builder::new()
            .name(thread_name.to_owned())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("driver runtime builds");
                // Publish the handle, then keep the runtime alive forever.
                // `pending` never resolves, so this parks at zero cost.
                let _ = tx.send(runtime.handle().clone());
                runtime.block_on(std::future::pending::<()>());
            })
            .expect("driver thread spawns");
        let handle = rx
            .recv()
            .expect("driver thread publishes its runtime handle");
        Driver { handle }
    }

    /// Runs `f` to completion on the driver runtime, blocking the caller.
    ///
    /// Safe from any thread — including tokio workers of an unrelated
    /// runtime, since the future executes on the private driver runtime.
    pub(crate) fn block_on<F: Future>(&self, f: F) -> F::Output {
        self.handle.block_on(f)
    }
}
