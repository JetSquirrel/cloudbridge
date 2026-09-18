//! `smol::unblock`, for both of the targets CloudBridge is built for.
//!
//! The desktop moves a blocking closure onto a worker thread and hands back a
//! future for its result — the data layer is DuckDB over a file, and a page
//! load must not stall a frame. The browser has no worker thread and the web
//! backend's data layer is in memory, so the closure runs where it is
//! awaited. The call shape is identical either way, which is what lets
//! `src/ui/*` compile unchanged for both.

/// Run `f`, resolving with its output.
///
/// The browser version is bounded less than the desktop one, because there is
/// no thread boundary for the closure or its output to cross. Everything that
/// compiles against the desktop version compiles against this.
#[cfg(not(target_family = "wasm"))]
pub use real_smol::unblock;

#[cfg(target_family = "wasm")]
pub fn unblock<T, F>(f: F) -> impl std::future::Future<Output = T>
where
    F: FnOnce() -> T,
{
    async move { f() }
}
