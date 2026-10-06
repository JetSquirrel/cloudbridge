//! The resource inventory, held in memory.
//!
//! The browser cannot read a corkscrew database — it has no file to pick
//! and no DuckDB to open one with — so an import is refused, as a bill
//! import is. The inventory it holds is the demo's, written by
//! [`write_inventory`].

use std::path::PathBuf;

use anyhow::{anyhow, Result};

use crate::memory;
use crate::model::{InventoryResource, InventoryScope};

/// Refused: the web demo reads no files.
pub fn import_scans(_paths: &[PathBuf]) -> Result<InventoryScope> {
    Err(anyhow!(
        "The web demo imports no inventory — it ships with a demo inventory already loaded"
    ))
}

/// Replace the inventory and its scan record, as the desktop's import does.
pub fn write_inventory(scope: &InventoryScope, resources: &[InventoryResource]) {
    memory::with_store(|store| {
        *store.resources.borrow_mut() = resources.to_vec();
        *store.inventory_scope.borrow_mut() = Some(scope.clone());
    });
}
