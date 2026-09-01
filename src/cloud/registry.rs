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
    /// Cost accrued over a period. `trend_window_days` is how far back the
    /// trend chart reads the ledger — a window is only worth charting as
    /// far as the source's own rows are detailed enough to fill it.
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

/// Where a source's credentials conventionally sit in the environment.
///
/// Each slot lists the variables a provider's own tooling reads, most
/// canonical first, so an account can be filled in from a shell that is
/// already configured instead of being copied by hand.
pub struct EnvCredentials {
    pub access_key: &'static [&'static str],
    /// Empty for a source that authenticates with a single key.
    pub secret_key: &'static [&'static str],
    pub region: &'static [&'static str],
}

/// Credentials found in the environment, ready to fill a form with.
pub struct FoundCredentials {
    pub access_key: String,
    pub secret_key: Option<String>,
    pub region: Option<String>,
    /// The variable the access key came from, so the UI can say which
    /// environment it is offering.
    pub access_key_var: &'static str,
}

impl EnvCredentials {
    /// Read what the environment has, or `None` when it has no key.
    ///
    /// A missing secret or region is not a failure — the user can fill in
    /// the rest — but without a key there is nothing to offer.
    pub fn read(&self) -> Option<FoundCredentials> {
        self.read_with(|name| std::env::var(name).ok())
    }

    /// [`Self::read`] against an arbitrary lookup, so the ordering rules
    /// can be tested without touching the process environment.
    fn read_with(&self, lookup: impl Fn(&str) -> Option<String>) -> Option<FoundCredentials> {
        let first_set = |names: &'static [&'static str]| {
            names.iter().find_map(|name| {
                lookup(name)
                    .filter(|value| !value.trim().is_empty())
                    .map(|value| (*name, value.trim().to_string()))
            })
        };

        let (access_key_var, access_key) = first_set(self.access_key)?;

        Some(FoundCredentials {
            access_key,
            secret_key: first_set(self.secret_key).map(|(_, value)| value),
            region: first_set(self.region).map(|(_, value)| value),
            access_key_var,
        })
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
    /// Environment variables this source's credentials can be read from,
    /// or `None` for a source with no such convention.
    pub env_credentials: Option<EnvCredentials>,
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

    /// Days of trend worth charting, or `None` for a source that has no
    /// history to chart.
    pub fn trend_window_days(&self) -> Option<i64> {
        match self.reporting {
            Reporting::Periodic { trend_window_days } => Some(trend_window_days),
            Reporting::Snapshot => None,
        }
    }

    /// Credentials for this source found in the environment, if any.
    pub fn credentials_from_env(&self) -> Option<FoundCredentials> {
        self.env_credentials.as_ref().and_then(EnvCredentials::read)
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
        env_credentials: Some(EnvCredentials {
            access_key: &["AWS_ACCESS_KEY_ID"],
            secret_key: &["AWS_SECRET_ACCESS_KEY"],
            // AWS_REGION wins, as it does in the SDKs.
            region: &["AWS_REGION", "AWS_DEFAULT_REGION"],
        }),
        build: |ctx| {
            Box::new(AwsCloudService::new(
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
        // QueryBillOverview reports one row per product per month, so a
        // day-level chart has nothing finer to show: the window covers two
        // billing periods rather than two weeks of empty days. Daily
        // detail arrives with the bill export channel (P1).
        reporting: Reporting::Periodic {
            trend_window_days: 62,
        },
        env_credentials: Some(EnvCredentials {
            access_key: &["ALIBABA_CLOUD_ACCESS_KEY_ID", "ALICLOUD_ACCESS_KEY"],
            secret_key: &["ALIBABA_CLOUD_ACCESS_KEY_SECRET", "ALICLOUD_SECRET_KEY"],
            region: &["ALIBABA_CLOUD_REGION_ID", "ALICLOUD_REGION"],
        }),
        build: |ctx| {
            Box::new(AliyunCloudService::new(
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
        env_credentials: Some(EnvCredentials {
            access_key: &["DEEPSEEK_API_KEY"],
            secret_key: &[],
            region: &[],
        }),
        build: |ctx| {
            Box::new(DeepSeekService::new(
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

    /// A lookup over a fixed set of variables.
    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        }
    }

    fn aws_env() -> &'static EnvCredentials {
        get("AWS")
            .expect("AWS is registered")
            .env_credentials
            .as_ref()
            .expect("AWS reads credentials from the environment")
    }

    #[test]
    fn a_configured_shell_fills_every_field() {
        let found = aws_env()
            .read_with(env(&[
                ("AWS_ACCESS_KEY_ID", "AKIAEXAMPLE"),
                ("AWS_SECRET_ACCESS_KEY", "secret"),
                ("AWS_REGION", "eu-west-1"),
            ]))
            .expect("credentials are there");

        assert_eq!(found.access_key, "AKIAEXAMPLE");
        assert_eq!(found.secret_key.as_deref(), Some("secret"));
        assert_eq!(found.region.as_deref(), Some("eu-west-1"));
        assert_eq!(found.access_key_var, "AWS_ACCESS_KEY_ID");
    }

    #[test]
    fn the_canonical_variable_wins_over_the_older_one() {
        let found = aws_env()
            .read_with(env(&[
                ("AWS_ACCESS_KEY_ID", "AKIAEXAMPLE"),
                ("AWS_DEFAULT_REGION", "us-east-1"),
                ("AWS_REGION", "eu-west-1"),
            ]))
            .unwrap();

        assert_eq!(found.region.as_deref(), Some("eu-west-1"));
    }

    #[test]
    fn a_shell_with_no_key_offers_nothing() {
        // A secret alone is not something to fill a form with.
        assert!(aws_env()
            .read_with(env(&[("AWS_SECRET_ACCESS_KEY", "secret")]))
            .is_none());
        // Nor is a variable that is set but empty.
        assert!(aws_env()
            .read_with(env(&[("AWS_ACCESS_KEY_ID", "   ")]))
            .is_none());
    }

    #[test]
    fn a_single_key_source_needs_no_secret() {
        let deepseek = get("DeepSeek").unwrap().env_credentials.as_ref().unwrap();

        let found = deepseek
            .read_with(env(&[("DEEPSEEK_API_KEY", "sk-example")]))
            .unwrap();
        assert_eq!(found.access_key, "sk-example");
        assert_eq!(found.secret_key, None);
        assert_eq!(found.region, None);
    }

    #[test]
    fn surrounding_whitespace_is_not_part_of_a_key() {
        let found = aws_env()
            .read_with(env(&[("AWS_ACCESS_KEY_ID", " AKIAEXAMPLE\n")]))
            .unwrap();
        assert_eq!(found.access_key, "AKIAEXAMPLE");
    }
}
