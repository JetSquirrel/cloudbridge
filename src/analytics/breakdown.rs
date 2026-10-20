//! Signed breakdowns and resource identity, shared by desktop and web.

use std::collections::BTreeMap;

use crate::model::{BreakdownDim, BreakdownPart};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum BucketKey {
    Dimension(String),
    Resource {
        provider: String,
        account_id: String,
        resource_id: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct Bucket {
    pub key: BucketKey,
    pub label: String,
    pub amount: f64,
}

impl Bucket {
    pub fn dimension((label, amount): &(String, f64)) -> Self {
        Self {
            key: BucketKey::Dimension(label.clone()),
            label: label.clone(),
            amount: *amount,
        }
    }
}

#[derive(Default)]
pub struct Breakdown(BTreeMap<String, BTreeMap<BucketKey, Bucket>>);

impl Breakdown {
    pub fn add(
        &mut self,
        provider: &str,
        account_id: &str,
        account_name: &str,
        inner: BreakdownDim,
        parts: Vec<BreakdownPart>,
    ) {
        for part in parts {
            let (key, label) = if inner == BreakdownDim::Resource {
                let resource = match &part.inner {
                    Some(id) => match part.resource_name.as_deref() {
                        Some(name) if name != id => format!("{name} ({id})"),
                        _ => id.clone(),
                    },
                    None => "Other (no resource ID)".to_string(),
                };
                (
                    BucketKey::Resource {
                        provider: provider.to_string(),
                        account_id: account_id.to_string(),
                        resource_id: part.inner,
                    },
                    format!("{resource} / {account_name} [{provider}:{account_id}]"),
                )
            } else {
                let label = part.inner.unwrap_or_else(|| "Other".to_string());
                (BucketKey::Dimension(label.clone()), label)
            };
            let bucket = self
                .0
                .entry(part.outer)
                .or_default()
                .entry(key.clone())
                .or_insert(Bucket {
                    key,
                    label,
                    amount: 0.0,
                });
            bucket.amount += part.amount;
        }
    }

    /// Sum signed children across accounts before dropping nonpositive
    /// parents. The parent's net and the opened level therefore reconcile.
    pub fn parents(&self) -> Vec<(String, f64)> {
        let mut rows: Vec<_> = self
            .0
            .iter()
            .map(|(label, children)| {
                (
                    label.clone(),
                    children.values().map(|row| row.amount).sum::<f64>(),
                )
            })
            .filter(|(_, amount)| *amount > 0.0)
            .collect();
        rows.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        rows
    }

    pub fn within(&self, parent: &str) -> Vec<Bucket> {
        let mut rows: Vec<_> = self
            .0
            .get(parent)
            .into_iter()
            .flat_map(|children| children.values())
            .filter(|row| row.amount != 0.0)
            .cloned()
            .collect();
        rows.sort_by(|a, b| {
            b.amount
                .total_cmp(&a.amount)
                .then_with(|| a.key.cmp(&b.key))
        });
        rows
    }
}

pub struct Totals {
    pub positive: f64,
    pub adjustments: f64,
    pub net: f64,
}

pub fn totals(rows: &[Bucket]) -> Totals {
    let positive = rows
        .iter()
        .filter(|row| row.amount > 0.0)
        .map(|row| row.amount)
        .sum::<f64>();
    let adjustments = rows
        .iter()
        .filter(|row| row.amount < 0.0)
        .map(|row| row.amount)
        .sum::<f64>();
    Totals {
        positive,
        adjustments,
        net: positive + adjustments,
    }
}

/// Match periods by identity, even if an account or resource was renamed.
/// Only positive current buckets have a treemap area. Negative buckets
/// remain in `within` and in the signed totals for the table and caption.
pub fn positive_comparisons<'a>(
    current: &'a [Bucket],
    previous: &[Bucket],
) -> Vec<(&'a Bucket, f64)> {
    let previous: BTreeMap<_, _> = previous.iter().map(|row| (&row.key, row.amount)).collect();
    current
        .iter()
        .filter(|row| row.amount > 0.0)
        .map(|row| (row, previous.get(&row.key).copied().unwrap_or(0.0)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(id: Option<&str>, name: Option<&str>, amount: f64) -> BreakdownPart {
        BreakdownPart {
            outer: "EC2".into(),
            inner: id.map(str::to_string),
            resource_name: name.map(str::to_string),
            amount,
        }
    }

    #[test]
    fn credits_reconcile_across_accounts_and_remain_visible() {
        let mut breakdown = Breakdown::default();
        breakdown.add(
            "aws",
            "a",
            "Prod",
            BreakdownDim::Resource,
            vec![part(Some("i-1"), None, 100.0)],
        );
        breakdown.add(
            "aws",
            "b",
            "Credits",
            BreakdownDim::Resource,
            vec![part(None, None, -20.0)],
        );
        assert_eq!(breakdown.parents(), vec![("EC2".into(), 80.0)]);
        let rows = breakdown.within("EC2");
        let totals = totals(&rows);
        assert_eq!(
            (totals.positive, totals.adjustments, totals.net),
            (100.0, -20.0, 80.0)
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(positive_comparisons(&rows, &[]).len(), 1);
    }

    #[test]
    fn names_and_accounts_do_not_merge_resource_identity() {
        let mut current = Breakdown::default();
        current.add(
            "aws",
            "a",
            "Prod",
            BreakdownDim::Resource,
            vec![
                part(Some("i-1"), Some("web"), 60.0),
                part(Some("i-2"), Some("web"), 40.0),
            ],
        );
        current.add(
            "aws",
            "b",
            "Prod",
            BreakdownDim::Resource,
            vec![part(Some("i-1"), Some("web"), 10.0)],
        );
        let rows = current.within("EC2");
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows.iter()
                .map(|row| row.label.clone())
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            3
        );
        let mut previous = Breakdown::default();
        previous.add(
            "aws",
            "a",
            "Old account name",
            BreakdownDim::Resource,
            vec![part(Some("i-1"), Some("old resource name"), 30.0)],
        );
        let comparisons = positive_comparisons(&rows, &previous.within("EC2"));
        assert_eq!(comparisons[0].1, 30.0);
        assert_eq!(comparisons[1].1, 0.0);
        assert_eq!(comparisons[2].1, 0.0);
    }

    #[test]
    fn service_children_net_across_accounts_before_ranking() {
        let mut breakdown = Breakdown::default();
        breakdown.add(
            "aws",
            "a",
            "A",
            BreakdownDim::Service,
            vec![part(Some("Compute"), None, 100.0)],
        );
        breakdown.add(
            "aws",
            "b",
            "B",
            BreakdownDim::Service,
            vec![part(Some("Compute"), None, -20.0)],
        );
        assert_eq!(breakdown.within("EC2").len(), 1);
        assert_eq!(breakdown.within("EC2")[0].amount, 80.0);
    }
}
