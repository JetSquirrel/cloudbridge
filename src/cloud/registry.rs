//! Billing source registry.
//!
//! A source is a row in [`SOURCES`], not an enum variant. Adding one means
//! adding a [`SourceDescriptor`] and a parser — nothing else in the codebase
//! learns its name. That matters because the roadmap adds model-provider
//! APIs, token plans and local agent usage on top of the public clouds, and
//! the previous `CloudProvider` enum had to be matched in 48 places.
//!
//! Callers ask the descriptor what a source can do rather than who it is:
//! DeepSeek renders as a balance because its [`Reporting`] is
//! [`Reporting::Snapshot`], not because it is called DeepSeek.

use serde::{Deserialize, Serialize};

use super::{aliyun::AliyunCloudService, aws::AwsCloudService, deepseek::DeepSeekService};
use super::{BillingSource, SourceContext};

/// What a source reports, and therefore how it can be displayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reporting {
    /// Cost accrued over a period. `trend_window_days` is how far back a
    /// daily trend is worth requesting — Alibaba Cloud needs one API call
    /// per day, so it gets a shorter window than AWS.
    Periodic { trend_window_days: i64 },
    /// A point-in-time balance. There is no period cost and no history to
    /// chart.
    Snapshot,
}

/// Identifier of a billing source.
///
/// Persisted verbatim in the `cloud_accounts` table, so these strings are
/// part of the on-disk format and must not be renamed without a migration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SourceId(String);

impl SourceId {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The descriptor for this id, or `None` if no source is registered
    /// under it — an account written by a newer build, or by a build that
    /// still had the Azure and GCP enum variants.
    pub fn descriptor(&self) -> Option<&'static SourceDescriptor> {
        get(&self.0)
    }
}

impl From<&str> for SourceId {
    fn from(id: &str) -> Self {
        Self(id.to_string())
    }
}

impl From<String> for SourceId {
    fn from(id: String) -> Self {
        Self(id)
    }
}

/// Everything the application needs to know about a billing source.
pub struct SourceDescriptor {
    /// Stable identifier; see [`SourceId`].
    pub id: &'static str,
    pub display_name: &'static str,
    pub short_name: &'static str,
    /// Label for the public half of the credential.
    pub access_key_label: &'static str,
    /// Label for the secret half, or `None` when the source authenticates
    /// with a single key.
    pub secret_key_label: Option<&'static str>,
    /// Region applied when the user leaves the field blank, or `None` when
    /// the source has no notion of a region.
    pub default_region: Option<&'static str>,
    pub reporting: Reporting,
    /// Builds the client. A function pointer keeps construction in this
    /// table instead of a `match` in every caller.
    pub build: fn(SourceContext) -> Box<dyn BillingSource>,
}

impl SourceDescriptor {
    pub fn source_id(&self) -> SourceId {
        SourceId::from(self.id)
    }

    /// Whether the credential form should require a secret key.
    pub fn needs_secret_key(&self) -> bool {
        self.secret_key_label.is_some()
    }

    pub fn secret_key_placeholder(&self) -> &'static str {
        self.secret_key_label
            .unwrap_or("(Not required, leave empty)")
    }

    pub fn region_placeholder(&self) -> String {
        match self.default_region {
            Some(region) => format!("Region (optional, default {})", region),
            None => "(Not required)".to_string(),
        }
    }

    /// Region to use for an account that stored none.
    pub fn region_or_default(&self, region: Option<String>) -> Option<String> {
        region.or_else(|| self.default_region.map(str::to_string))
    }

    /// Days of daily trend worth requesting, or `None` for a source that has
    /// no history to chart.
    pub fn trend_window_days(&self) -> Option<i64> {
        match self.reporting {
            Reporting::Periodic { trend_window_days } => Some(trend_window_days),
            Reporting::Snapshot => None,
        }
    }

    /// Whether this source reports a balance rather than a period cost.
    pub fn is_snapshot(&self) -> bool {
        matches!(self.reporting, Reporting::Snapshot)
    }
}

static SOURCES: &[SourceDescriptor] = &[
    SourceDescriptor {
        id: "AWS",
        display_name: "Amazon Web Services",
        short_name: "AWS",
        access_key_label: "Access Key ID",
        secret_key_label: Some("Secret Access Key"),
        default_region: Some("us-east-1"),
        reporting: Reporting::Periodic {
            trend_window_days: 30,
        },
        build: |ctx| {
            Box::new(AwsCloudService::new(
                ctx.account_id,
                ctx.account_name,
                ctx.access_key_id,
                ctx.secret_access_key,
                ctx.region,
            ))
        },
    },
    SourceDescriptor {
        id: "Aliyun",
        display_name: "Alibaba Cloud",
        short_name: "Aliyun",
        access_key_label: "AccessKey ID",
        secret_key_label: Some("AccessKey Secret"),
        default_region: Some("cn-hangzhou"),
        // One API call per day, so a shorter window than AWS.
        reporting: Reporting::Periodic {
            trend_window_days: 7,
        },
        build: |ctx| {
            Box::new(AliyunCloudService::new(
                ctx.account_id,
                ctx.account_name,
                ctx.access_key_id,
                ctx.secret_access_key,
                ctx.region,
            ))
        },
    },
    SourceDescriptor {
        id: "DeepSeek",
        display_name: "DeepSeek",
        short_name: "DeepSeek",
        access_key_label: "API Key",
        secret_key_label: None,
        default_region: None,
        reporting: Reporting::Snapshot,
        build: |ctx| {
            Box::new(DeepSeekService::new(
                ctx.account_id,
                ctx.account_name,
                ctx.access_key_id,
                ctx.secret_access_key,
                ctx.region,
            ))
        },
    },
];

/// Every registered source, in the order they are offered in the UI.
pub fn all() -> &'static [SourceDescriptor] {
    SOURCES
}

/// The descriptor registered under `id`, if any.
pub fn get(id: &str) -> Option<&'static SourceDescriptor> {
    SOURCES.iter().find(|source| source.id == id)
}

/// The source offered first when adding an account.
pub fn default_source() -> &'static SourceDescriptor {
    &SOURCES[0]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ids are persisted in `cloud_accounts`, so a duplicate would make one of
    /// the two sources unreachable and silently reroute existing accounts.
    #[test]
    fn ids_are_unique() {
        let mut seen = Vec::new();
        for source in all() {
            assert!(
                !seen.contains(&source.id),
                "duplicate source id {}",
                source.id
            );
            seen.push(source.id);
        }
    }

    /// The round trip an account takes: descriptor -> stored id -> descriptor.
    #[test]
    fn every_descriptor_resolves_from_its_own_id() {
        for source in all() {
            let resolved = source
                .source_id()
                .descriptor()
                .unwrap_or_else(|| panic!("{} does not resolve", source.id));
            assert_eq!(resolved.id, source.id);
        }
    }

    /// A source with no region must not offer one, or the credential form
    /// would ask for a value that is silently discarded.
    #[test]
    fn region_placeholder_matches_default_region() {
        for source in all() {
            match source.default_region {
                Some(region) => {
                    assert!(
                        source.region_placeholder().contains(region),
                        "{}",
                        source.id
                    )
                }
                None => assert_eq!(source.region_placeholder(), "(Not required)"),
            }
        }
    }
}
