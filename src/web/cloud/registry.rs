//! The source registry, under the path the pages reach it by.
//!
//! The desktop's `cloud::registry` is a module with a `SOURCES` table and a
//! `get` to search it. Here the table lives one level up, in
//! `src/web/cloud/mod.rs`, because the browser's registry is small enough that
//! a second module for it would only be a place to look in.
//!
//! What matters is that `crate::cloud::registry::get(id)` and
//! `use crate::cloud::registry::SourceDescriptor` mean the same thing to a
//! caller here as they do on the desktop, so the pages that ask for a
//! descriptor compile either way.

pub use super::{all, default_source, get, BillFileFormat, SourceDescriptor};
