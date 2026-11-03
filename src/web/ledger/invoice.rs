//! Invoice evidence, held in memory. The same surface as the desktop's
//! `ledger::invoice`; the comparison is shared, in
//! [`crate::analytics::reconciliation`].

use anyhow::Result;

use crate::memory;
use crate::model::{CostBasis, Invoice, PeriodKey};

/// Store invoices. One carrying the same reference for the same month and
/// account replaces the earlier entry. Returns how many were stored.
pub fn save_invoices(invoices: &[Invoice]) -> Result<usize> {
    memory::with_store(|store| {
        let mut stored = store.invoices.borrow_mut();
        for invoice in invoices {
            stored.retain(|existing| {
                existing.key != invoice.key || existing.invoice_ref != invoice.invoice_ref
            });
            stored.push(invoice.clone());
        }
        Ok(invoices.len())
    })
}

/// Every invoice on record.
pub fn all_invoices() -> Result<Vec<Invoice>> {
    memory::with_store(|store| {
        let mut invoices = store.invoices.borrow().clone();
        invoices.sort_by(|a, b| {
            b.key
                .billing_period
                .cmp(&a.key.billing_period)
                .then_with(|| a.key.provider.cmp(&b.key.provider))
                .then_with(|| a.key.account_id.cmp(&b.key.account_id))
                .then_with(|| a.invoice_ref.cmp(&b.invoice_ref))
        });
        Ok(invoices)
    })
}

/// Remove one invoice from the evidence.
pub fn delete_invoice(invoice_id: &str) -> Result<()> {
    memory::with_store(|store| {
        store
            .invoices
            .borrow_mut()
            .retain(|invoice| invoice.invoice_id != invoice_id);
        Ok(())
    })
}

/// A period's authoritative billed total per currency.
pub fn billed_totals(key: &PeriodKey) -> Result<Vec<(String, f64)>> {
    memory::with_store(|store| {
        let mut totals = std::collections::BTreeMap::<String, f64>::new();
        for period in store.periods.borrow().iter().filter(|p| p.key == *key) {
            for charge in &period.charges {
                if let (CostBasis::Authoritative, Some(amount)) =
                    (charge.cost_basis, charge.billed_cost)
                {
                    *totals.entry(charge.billing_currency.clone()).or_default() += amount;
                }
            }
        }
        Ok(totals.into_iter().collect())
    })
}
