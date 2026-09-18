//! CloudBridge as a library, so the same source builds twice: as the desktop
//! binary (`main.rs`) and as the browser demo (`wasm_entry.rs`).
//!
//! The pages, the view models and the alerting rules are shared. What is
//! swapped per target is the data backend underneath them — DuckDB, the
//! provider APIs and the OS keyring on the desktop; an in-memory ledger
//! seeded with demo data in the browser. That swap is the `cfg` selection
//! below: whichever backend a target gets, it answers to the same module name
//! and the same function signatures, so nothing above it has to know.

/// The domain types both backends are written against, and the handle a
/// backend hands to a closure.
pub mod model;
pub mod store;

// The statistics computed on top of whichever backend answered: forecasts,
// period comparisons, the trailing average, the data-quality findings. Pure
// functions over aggregates, so both targets compute them the same way.
pub mod analytics;

// The demo bill, as rows. Written into whichever ledger the target has, by
// that ledger's own `demo` module.
pub mod demo_data;

// Billing sources. Native builds carry the registry, the provider clients
// and the bill-file importers; the browser cannot sign a request or read a
// file, so it gets a metadata-only registry answering the same questions.
#[cfg(not(target_family = "wasm"))]
pub mod cloud;
#[cfg(target_family = "wasm")]
#[path = "web/cloud/mod.rs"]
pub mod cloud;

// The ledger: one charge fact table, read through one normalized view.
#[cfg(not(target_family = "wasm"))]
pub mod ledger;
#[cfg(target_family = "wasm")]
#[path = "web/ledger/mod.rs"]
pub mod ledger;

// Application state: accounts, budgets, alert rules and their events.
#[cfg(not(target_family = "wasm"))]
pub mod db;
#[cfg(target_family = "wasm")]
#[path = "web/db.rs"]
pub mod db;

// Fetching a bill and importing one. Both need a network or a filesystem, so
// the browser's version of this module says so rather than pretending.
#[cfg(not(target_family = "wasm"))]
pub mod ingest;
#[cfg(target_family = "wasm")]
#[path = "web/ingest.rs"]
pub mod ingest;

// Configuration is one file for both targets: the structs are shared and only
// the place a config is kept differs, so the gating is inside it.
pub mod config;

// Credentials. A browser has no keyring to keep them in, and the demo holds
// none to begin with.
#[cfg(not(target_family = "wasm"))]
pub mod secret_store;
#[cfg(target_family = "wasm")]
#[path = "web/secret_store.rs"]
pub mod secret_store;

// The AES path exists to migrate pre-0.2.0 credentials out of the database,
// which is a desktop-only concern.
#[cfg(not(target_family = "wasm"))]
pub mod crypto;

// Everything the two targets genuinely share.
pub mod alerts;
pub mod app;
pub mod ui;

// The in-memory store the browser backends are written against.
#[cfg(target_family = "wasm")]
#[path = "web/memory.rs"]
pub mod memory;

#[cfg(not(target_family = "wasm"))]
pub mod desktop;

#[cfg(target_family = "wasm")]
pub mod wasm_entry;
