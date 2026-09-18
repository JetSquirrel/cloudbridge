//! The handle a data backend passes to a closure.
//!
//! The desktop's is a DuckDB connection; the web demo's is an in-memory
//! store that holds both databases at once. A function that must not know
//! which of the two it was handed names the handle through here, and reaches
//! the data through whichever `db`/`ledger` module it belongs to.
//!
//! On the desktop the alias resolves to DuckDB's own connection type, so the
//! native backends keep taking `duckdb::Connection` directly and nothing
//! about them changed.

#[cfg(not(target_family = "wasm"))]
pub use duckdb::Connection;

/// A handle on the in-memory store.
///
/// It carries nothing: the web backend keeps one store for both databases, so
/// the handle is only the name a signature needs.
#[cfg(target_family = "wasm")]
#[derive(Debug, Clone, Copy)]
pub struct Connection;
