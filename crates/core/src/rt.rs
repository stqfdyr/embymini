//! The tokio runtime the backend runs on: network, caching proxy, image cache, player sessions.
//!
//! One runtime per process, started on first use. The UI has an executor of its own (GPUI): it hands work over
//! with [`spawn`] and awaits the join handle.

use std::future::Future;
use std::sync::OnceLock;
use tokio::runtime::{Handle, Runtime};
use tokio::task::JoinHandle;

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

/// The process-wide runtime (started on the first call).
pub fn handle() -> &'static Handle {
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("starting the tokio runtime")
        })
        .handle()
}

/// Runs `f` on the runtime; callable from any thread.
pub fn spawn<F>(f: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    handle().spawn(f)
}

/// Runs blocking work (disk walks, decoding) on the runtime's blocking pool.
pub fn spawn_blocking<F, R>(f: F) -> JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    handle().spawn_blocking(f)
}

/// Waits for `f` on the calling thread. Panics on the runtime's async threads (call it from a plain or a
/// blocking thread).
pub fn block_on<F: Future>(f: F) -> F::Output {
    handle().block_on(f)
}
