//! Driver-thread pattern for async backends behind sync traits.
//!
//! `ObjectStore` and `MetadataStore` are synchronous traits, but the
//! Postgres and S3 clients are async (tokio). `Runtime::block_on` from a
//! trait method would panic whenever the caller is itself inside a tokio
//! context (a tonic handler under `#[tokio::main]`, say), because tokio
//! forbids blocking a runtime from within that runtime. So each
//! async-backed store owns a *private* single-threaded runtime living in
//! a dedicated driver thread, captured once at open as a
//! [`tokio::runtime::Handle`].
//!
//! Every trait call *spawns* its future onto the driver runtime and
//! parks the calling thread on a plain std channel until the result
//! arrives. Because the caller never touches a tokio API that blocks,
//! this is panic-free from any thread — including tokio workers of an
//! unrelated runtime (e.g. the server's). The futures and their outputs
//! must be `Send + 'static`, which holds for all our store operations.
//!
//! The driver thread runs for the store's lifetime (it parks inside
//! `pending::<()>()`, which costs nothing) and is torn down with the
//! process — the stores are opened once per replica and never closed.

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
    /// thread or the runtime cannot be built — never in library
    /// operation. Note the opening call itself may come from inside a
    /// tokio context: [`Driver::block_on`] below never blocks a runtime.
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

    /// Runs `f` to completion on the driver runtime, blocking the caller
    /// until the result arrives.
    ///
    /// The future is spawned onto the driver runtime and the caller
    /// parks on a std mpsc receiver — no tokio blocking primitive runs
    /// on the calling thread, so this is safe from inside any tokio
    /// context (a tonic handler, `#[tokio::main]`, a test runtime, ...).
    pub(crate) fn block_on<F>(&self, f: F) -> F::Output
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let (tx, rx) = std::sync::mpsc::channel();
        self.handle.spawn(async move {
            let _ = tx.send(f.await);
        });
        rx.recv().expect("driver runtime delivers result")
    }
}
