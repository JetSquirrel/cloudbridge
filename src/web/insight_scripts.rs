//! The browser runs only the built-in Insights rules: there is no folder
//! to read rules from, so this answers the desktop's `run` with none.

use crate::model::{InsightsReport, InventoryResource, InventoryScope, ResourceCost, ScriptRun};

pub fn run(
    _period: &str,
    _currency: &str,
    _resources: &[InventoryResource],
    _costs: &[ResourceCost],
    _scope: Option<&InventoryScope>,
    _report: &InsightsReport,
) -> ScriptRun {
    ScriptRun::default()
}
