//! Billing sources: accounts, the data they report, and the client trait.

pub mod aliyun;
pub mod aws;
pub mod aws_focus;
pub mod billfile;
pub mod deduction;
pub mod deepseek;
pub mod raw;
pub mod registry;
pub mod s3;

use anyhow::Result;

use crate::ledger::{BalanceSnapshot, Charge};
pub use raw::{PayloadFile, RawBatch, RawPart};
pub use registry::SourceDescriptor;

/// The credentials a [`BillingSource`] is built from.
///
/// Bundled into one struct so [`SourceDescriptor::build`] can be a plain
/// function pointer. Deliberately no account id or name: a client
/// authenticates and fetches, and which account the result is filed under
/// is the ingest's business, not its own.
pub struct SourceContext {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub region: Option<String>,
    /// Where the provider's own billing export lands, if the account is
    /// backed by one (`s3://bucket/prefix` for AWS Data Exports). When set,
    /// the export is the bill and the fetch reads it instead of an API.
    pub export_uri: Option<String>,
}

pub use crate::model::{
    access_key_hint, BillingPeriod, BudgetInfo, BudgetStatus, CloudAccount, SourceId,
};

impl CloudAccount {
    /// The descriptor for this account's source, or `None` if the stored id
    /// is not registered in this build.
    pub fn descriptor(&self) -> Option<&'static SourceDescriptor> {
        self.source_id.descriptor()
    }
}

impl SourceId {
    /// The descriptor for this id, or `None` if no source is registered
    /// under it — an account written by a newer build, or by a build that
    /// still had the Azure and GCP enum variants.
    pub fn descriptor(&self) -> Option<&'static SourceDescriptor> {
        registry::get(self.as_str())
    }
}

/// What a normalizer produces: FOCUS rows ready for the ledger.
///
/// Charges and balances are separate because a balance is state, not a
/// charge — see `fct_balance_snapshot`.
#[derive(Debug, Default)]
pub struct Normalized {
    pub charges: Vec<Charge>,
    pub balances: Vec<BalanceSnapshot>,
}

/// What one fetch returned: text responses as [`RawPart`]s, and binary
/// objects (a Parquet billing export is not UTF-8) as [`PayloadFile`]s.
#[derive(Debug, Default)]
pub struct Fetched {
    pub parts: Vec<RawPart>,
    pub payload_files: Vec<PayloadFile>,
}

impl Fetched {
    /// A fetch of text responses only, which is every API source today.
    pub fn parts_only(parts: Vec<RawPart>) -> Self {
        Self {
            parts,
            payload_files: Vec::new(),
        }
    }
}

/// A source of billing data (sync, using ureq).
///
/// [`Self::fetch`] and [`Self::normalize`] are deliberately split. `fetch`
/// touches the network and interprets nothing; `normalize` interprets and
/// touches nothing. That is what makes the billing logic testable from a
/// recorded payload, and what keeps a mapping fix from costing another
/// round of paid API calls.
pub trait BillingSource: Send + Sync {
    /// Validate credentials
    fn validate_credentials(&self) -> Result<bool>;

    /// Retrieve everything the provider reports for one billing period,
    /// unchanged. The only method here that talks to the network.
    fn fetch(&self, period: &BillingPeriod) -> Result<Fetched>;

    /// Turn a fetched batch into ledger rows. Pure: no clock, no network,
    /// no database — everything it needs is in the batch.
    fn normalize(&self, batch: &RawBatch) -> Result<Normalized>;
}
