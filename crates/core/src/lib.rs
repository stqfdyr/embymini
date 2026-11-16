//! EmbyMini's backend: servers and config, the Emby API, posters, the local caching proxy, playback in mpv and
//! danmaku. No UI framework in here: the UI shell holds an [`app::App`] and calls its methods.

pub mod app;
pub mod config;
pub mod danmaku;
pub mod emby;
pub mod images;
pub mod player;
pub mod rt;
pub mod status;
pub mod stream;
pub mod updater;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

/// Ignores lock poisoning: the data here is replaced whole, never left half-updated by a panic.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Subtracts without going below 0: an inexact count should undercount, never wrap into a huge number
/// that would make trimming delete everything. Shared by the video and image caches.
fn sub_total(total: &AtomicU64, n: u64) {
    // fetch_update is renamed try_update in new toolchains and the new name is missing in our MSRV: hand-rolled CAS.
    let mut v = total.load(Ordering::Relaxed);
    while let Err(now) =
        total.compare_exchange_weak(v, v.saturating_sub(n), Ordering::Relaxed, Ordering::Relaxed)
    {
        v = now;
    }
}
